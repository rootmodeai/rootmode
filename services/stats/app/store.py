"""Storage: raw reports in, daily buckets out.

SQLite, because the working set is one row per worker per day and the read
path is a single JSON document the explorer fetches. Postgres is a change of
`connect()` if that ever stops being true.

Nothing here stores a prompt, a result, or a client peer id — none of it
arrives in the first place. What is kept is which worker served how much.
"""

from __future__ import annotations

import json
import sqlite3
import threading
from contextlib import contextmanager
from datetime import datetime, timedelta, timezone
from pathlib import Path

SCHEMA = """
CREATE TABLE IF NOT EXISTS reports (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    peer_id     TEXT NOT NULL,
    day         TEXT NOT NULL,          -- UTC, YYYY-MM-DD
    received_at INTEGER NOT NULL,
    window_secs INTEGER NOT NULL,
    requests    INTEGER NOT NULL DEFAULT 0,
    images      INTEGER NOT NULL DEFAULT 0,
    tokens_in   INTEGER NOT NULL DEFAULT 0,
    tokens_out  INTEGER NOT NULL DEFAULT 0,
    revenue     REAL    NOT NULL DEFAULT 0,
    failures    INTEGER NOT NULL DEFAULT 0,
    rejected    INTEGER NOT NULL DEFAULT 0,
    models      TEXT    NOT NULL DEFAULT '[]',
    currency    TEXT    NOT NULL DEFAULT 'USD'
);
CREATE INDEX IF NOT EXISTS reports_day ON reports (day);
CREATE INDEX IF NOT EXISTS reports_peer_day ON reports (peer_id, day);

-- Growth: who fetched the app, and how many copies are alive.
--
-- A download is a visitor sent to an installer by /download: the platform
-- the redirect chose, the day, a country. Nothing that names the visitor.
CREATE TABLE IF NOT EXISTS downloads (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    at          INTEGER NOT NULL,
    day         TEXT NOT NULL,
    platform    TEXT NOT NULL,          -- macos-arm64 | windows-x64 | … | other
    source      TEXT NOT NULL,          -- 'button' (/download) | 'named' (/download/<os>)
    country     TEXT
);
CREATE INDEX IF NOT EXISTS downloads_day ON downloads (day);

-- An install is a copy of the desktop app that said hello while checking
-- for an update: a random id it made up for itself, its version, its OS.
-- The person can switch this off in Settings, and then the check carries
-- nothing and nothing is written here.
CREATE TABLE IF NOT EXISTS installs (
    install_id  TEXT PRIMARY KEY,
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL,
    version     TEXT NOT NULL DEFAULT '',
    os          TEXT NOT NULL DEFAULT '',
    arch        TEXT NOT NULL DEFAULT '',
    country     TEXT
);
CREATE TABLE IF NOT EXISTS heartbeats (
    day         TEXT NOT NULL,
    install_id  TEXT NOT NULL,
    PRIMARY KEY (day, install_id)
);

-- What GitHub says each release asset has been downloaded, once a day, so
-- the number on the release page becomes a curve.
CREATE TABLE IF NOT EXISTS release_downloads (
    day         TEXT NOT NULL,
    tag         TEXT NOT NULL,
    asset       TEXT NOT NULL,
    count       INTEGER NOT NULL,
    PRIMARY KEY (day, tag, asset)
);

CREATE TABLE IF NOT EXISTS workers (
    peer_id     TEXT PRIMARY KEY,
    label       TEXT NOT NULL DEFAULT '',
    country     TEXT,                   -- declared, else geolocated
    country_src TEXT,                   -- 'declared' | 'geoip'
    ip_hash     TEXT,                   -- salted, one-way; never an address
    caps        TEXT NOT NULL DEFAULT '[]',
    models      TEXT NOT NULL DEFAULT '[]',
    first_seen  INTEGER NOT NULL,
    last_seen   INTEGER NOT NULL
);
"""


def _today(ts: int) -> str:
    return datetime.fromtimestamp(ts, timezone.utc).strftime("%Y-%m-%d")


class Store:
    def __init__(self, path: str | Path):
        self.path = str(path)
        self._lock = threading.Lock()
        Path(self.path).parent.mkdir(parents=True, exist_ok=True)
        with self.connect() as conn:
            conn.executescript(SCHEMA)

    @contextmanager
    def connect(self):
        conn = sqlite3.connect(self.path, timeout=10)
        conn.row_factory = sqlite3.Row
        # A collector is write-light and read-heavy; WAL keeps the explorer's
        # reads from blocking behind a worker's POST.
        conn.execute("PRAGMA journal_mode=WAL")
        try:
            with self._lock:
                yield conn
                conn.commit()
        finally:
            conn.close()

    # ------------------------------------------------------------- writing

    def record(self, report, *, country: str | None, country_src: str | None,
               ip_hash: str | None, now: int) -> None:
        day = _today(now)
        with self.connect() as conn:
            conn.execute(
                """INSERT INTO reports
                     (peer_id, day, received_at, window_secs, requests, images,
                      tokens_in, tokens_out, revenue, failures, rejected, models,
                      currency)
                   VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?)""",
                (report.peer_id, day, now, report.window_secs, report.requests,
                 report.images, report.tokens_in, report.tokens_out,
                 report.revenue, report.failures, report.rejected,
                 json.dumps(report.models), report.currency),
            )
            conn.execute(
                """INSERT INTO workers
                     (peer_id, label, country, country_src, ip_hash, caps, models,
                      first_seen, last_seen)
                   VALUES (?,?,?,?,?,?,?,?,?)
                   ON CONFLICT(peer_id) DO UPDATE SET
                     label       = excluded.label,
                     country     = COALESCE(excluded.country, workers.country),
                     country_src = COALESCE(excluded.country_src, workers.country_src),
                     ip_hash     = COALESCE(excluded.ip_hash, workers.ip_hash),
                     caps        = excluded.caps,
                     models      = excluded.models,
                     last_seen   = excluded.last_seen""",
                (report.peer_id, report.label, country, country_src, ip_hash,
                 json.dumps(report.caps), json.dumps(report.models), now, now),
            )

    # ------------------------------------------------------------- growth

    def record_download(self, *, platform: str, source: str, country: str | None, now: int) -> None:
        with self.connect() as conn:
            conn.execute(
                "INSERT INTO downloads (at, day, platform, source, country) VALUES (?,?,?,?,?)",
                (now, _today(now), platform, source, country),
            )

    def record_heartbeat(self, *, install_id: str, version: str, os: str, arch: str,
                         country: str | None, now: int) -> bool:
        """Note that this install is alive today. True the first time it is ever seen."""
        with self.connect() as conn:
            known = conn.execute(
                "SELECT 1 FROM installs WHERE install_id = ?", (install_id,)
            ).fetchone() is not None
            conn.execute(
                """INSERT INTO installs (install_id, first_seen, last_seen, version, os, arch, country)
                   VALUES (?,?,?,?,?,?,?)
                   ON CONFLICT(install_id) DO UPDATE SET
                     last_seen = excluded.last_seen,
                     version   = excluded.version,
                     os        = excluded.os,
                     arch      = excluded.arch,
                     country   = COALESCE(excluded.country, installs.country)""",
                (install_id, now, now, version, os, arch, country),
            )
            conn.execute(
                "INSERT OR IGNORE INTO heartbeats (day, install_id) VALUES (?, ?)",
                (_today(now), install_id),
            )
            return not known

    def record_release_counts(self, rows: list[tuple[str, str, int]], now: int) -> None:
        """Today's snapshot of GitHub's per-asset counts; a later one the same day replaces it."""
        day = _today(now)
        with self.connect() as conn:
            for tag, asset, count in rows:
                conn.execute(
                    "INSERT OR REPLACE INTO release_downloads (day, tag, asset, count) VALUES (?,?,?,?)",
                    (day, tag, asset, int(count)),
                )

    def growth(self, *, days: int = 90, now: int | None = None) -> dict:
        """The founder's numbers: downloads, installs, and who is still here."""
        now = now or int(datetime.now(timezone.utc).timestamp())
        first = (datetime.fromtimestamp(now, timezone.utc) - timedelta(days=days - 1)).date()
        since = first.isoformat()

        with self.connect() as conn:
            dl_day = {r["day"]: int(r["n"]) for r in conn.execute(
                "SELECT day, COUNT(*) AS n FROM downloads WHERE day >= ? GROUP BY day", (since,))}
            dl_total = int(conn.execute("SELECT COUNT(*) AS n FROM downloads").fetchone()["n"])
            dl_platform = [(r["platform"], int(r["n"])) for r in conn.execute(
                "SELECT platform, COUNT(*) AS n FROM downloads WHERE at >= ? GROUP BY platform ORDER BY n DESC",
                (now - 30 * 86_400,))]
            dl_country = [(r["country"], int(r["n"])) for r in conn.execute(
                "SELECT country, COUNT(*) AS n FROM downloads WHERE at >= ? AND country IS NOT NULL "
                "GROUP BY country ORDER BY n DESC LIMIT 12", (now - 30 * 86_400,))]

            new_day = {r["day"]: int(r["n"]) for r in conn.execute(
                "SELECT strftime('%Y-%m-%d', first_seen, 'unixepoch') AS day, COUNT(*) AS n "
                "FROM installs WHERE first_seen >= ? GROUP BY day",
                (int(datetime.combine(first, datetime.min.time(), tzinfo=timezone.utc).timestamp()),))}
            active_day = {r["day"]: int(r["n"]) for r in conn.execute(
                "SELECT day, COUNT(*) AS n FROM heartbeats WHERE day >= ? GROUP BY day", (since,))}
            installs_total = int(conn.execute("SELECT COUNT(*) AS n FROM installs").fetchone()["n"])

            def active_since(secs: int) -> int:
                return int(conn.execute(
                    "SELECT COUNT(*) AS n FROM installs WHERE last_seen >= ?", (now - secs,)
                ).fetchone()["n"])

            def breakdown(col: str) -> list[tuple[str, int]]:
                return [(r[col] or "?", int(r["n"])) for r in conn.execute(
                    f"SELECT {col}, COUNT(*) AS n FROM installs WHERE last_seen >= ? "
                    f"GROUP BY {col} ORDER BY n DESC LIMIT 12", (now - 30 * 86_400,))]

            active_24h, active_7d, active_30d = active_since(86_400), active_since(7 * 86_400), active_since(30 * 86_400)
            by_version, by_os, by_country = breakdown("version"), breakdown("os"), breakdown("country")

            latest_day = conn.execute("SELECT MAX(day) AS d FROM release_downloads").fetchone()["d"]
            gh_assets = [(r["tag"], r["asset"], int(r["count"])) for r in conn.execute(
                "SELECT tag, asset, count FROM release_downloads WHERE day = ? ORDER BY tag DESC, asset",
                (latest_day,))] if latest_day else []
            gh_day = {r["day"]: int(r["n"]) for r in conn.execute(
                "SELECT day, SUM(count) AS n FROM release_downloads WHERE day >= ? GROUP BY day", (since,))}

        out_days = []
        prev_gh = None
        for i in range(days):
            d = (first + timedelta(days=i)).isoformat()
            gh = gh_day.get(d)
            out_days.append({
                "date": d,
                "downloads": dl_day.get(d, 0),
                "installs_new": new_day.get(d, 0),
                "installs_active": active_day.get(d, 0),
                # GitHub's counter is cumulative; the day's figure is its rise
                # since the previous snapshot, or null where none was taken.
                "github_downloads": (gh - prev_gh) if (gh is not None and prev_gh is not None) else None,
            })
            if gh is not None:
                prev_gh = gh

        return {
            "updated": datetime.fromtimestamp(now, timezone.utc).isoformat(),
            "days": out_days,
            "downloads": {
                "total": dl_total,
                "last_7d": sum(dl_day.get((first + timedelta(days=i)).isoformat(), 0)
                               for i in range(max(0, days - 7), days)),
                "by_platform_30d": [{"platform": p, "count": n} for p, n in dl_platform],
                "by_country_30d": [{"country": c, "count": n} for c, n in dl_country],
            },
            "github": _github_block(latest_day, gh_assets),
            "installs": {
                "total": installs_total,
                "new_7d": sum(new_day.get((first + timedelta(days=i)).isoformat(), 0)
                              for i in range(max(0, days - 7), days)),
                "active_24h": active_24h,
                "active_7d": active_7d,
                "active_30d": active_30d,
                "by_version_30d": [{"version": v, "count": n} for v, n in by_version],
                "by_os_30d": [{"os": o, "count": n} for o, n in by_os],
                "by_country_30d": [{"country": c, "count": n} for c, n in by_country],
            },
        }

    def reports_since(self, peer_id: str, since: int) -> int:  # noqa: E301 — see _github_block below
        """How many reports this node has filed lately, for rate limiting."""
        with self.connect() as conn:
            row = conn.execute(
                "SELECT COUNT(*) AS n FROM reports WHERE peer_id = ? AND received_at >= ?",
                (peer_id, since),
            ).fetchone()
            return int(row["n"])

    # ------------------------------------------------------------- reading

    def stats(self, *, days: int = 120, now: int | None = None) -> dict:
        """The document the explorer renders, in its own shape."""
        now = now or int(datetime.now(timezone.utc).timestamp())
        first = (datetime.fromtimestamp(now, timezone.utc) - timedelta(days=days - 1)).date()

        with self.connect() as conn:
            rows = conn.execute(
                """SELECT day,
                          SUM(tokens_in)  AS tokens_in,
                          SUM(tokens_out) AS tokens_out,
                          SUM(requests)   AS requests,
                          SUM(images)     AS images,
                          SUM(revenue)    AS revenue,
                          COUNT(DISTINCT peer_id) AS workers
                     FROM reports WHERE day >= ? GROUP BY day ORDER BY day""",
                (first.isoformat(),),
            ).fetchall()
            by_day = {r["day"]: r for r in rows}

            # Every worker seen on a day, and whether it had been seen before,
            # which is what makes "new" mean anything.
            seen = conn.execute(
                "SELECT day, peer_id, MIN(received_at) AS at FROM reports "
                "WHERE day >= ? GROUP BY day, peer_id",
                (first.isoformat(),),
            ).fetchall()
            first_day: dict[str, str] = {}
            per_day: dict[str, list[str]] = {}
            for r in seen:
                per_day.setdefault(r["day"], []).append(r["peer_id"])
                if r["peer_id"] not in first_day or r["day"] < first_day[r["peer_id"]]:
                    first_day[r["peer_id"]] = r["day"]

            model_rows = conn.execute(
                """SELECT models, SUM(tokens_in + tokens_out) AS tokens, SUM(requests) AS requests
                     FROM reports WHERE day >= date('now', '-30 day') GROUP BY models""",
            ).fetchall()

            worker_rows = conn.execute(
                """SELECT w.peer_id, w.label, w.country, w.country_src, w.caps, w.models,
                          w.last_seen,
                          COALESCE(SUM(r.tokens_in + r.tokens_out), 0) AS tokens_24h,
                          COALESCE(SUM(r.requests), 0) AS requests_24h
                     FROM workers w
                     LEFT JOIN reports r
                       ON r.peer_id = w.peer_id AND r.received_at >= ?
                    GROUP BY w.peer_id
                    ORDER BY tokens_24h DESC""",
                (now - 86_400,),
            ).fetchall()

        # Days with no reports are zeroes, not gaps: a quiet Sunday is data.
        out_days = []
        for i in range(days):
            d = (first + timedelta(days=i)).isoformat()
            row = by_day.get(d)
            peers = per_day.get(d, [])
            new = sum(1 for p in peers if first_day.get(p) == d)
            out_days.append({
                "date": d,
                "tokens_in": int(row["tokens_in"] or 0) if row else 0,
                "tokens_out": int(row["tokens_out"] or 0) if row else 0,
                "requests": int(row["requests"] or 0) if row else 0,
                "images": int(row["images"] or 0) if row else 0,
                "revenue": round(float(row["revenue"] or 0), 4) if row else 0.0,
                # These are workers that reported, not people who asked. The
                # collector never learns who submitted anything, so it counts
                # the side that talks to it and the explorer says so.
                "peers_returning": len(peers) - new,
                "peers_new": new,
            })

        totals: dict[str, float] = {}
        for r in model_rows:
            names = json.loads(r["models"] or "[]") or ["(unnamed)"]
            # A report carries the node's whole model list, not per-model
            # counts, so its tokens are shared out evenly. Honest and rough;
            # per-model reporting is the fix, not cleverer arithmetic here.
            share = float(r["tokens"] or 0) / len(names)
            for n in names:
                totals[n] = totals.get(n, 0.0) + share

        return {
            "updated": datetime.fromtimestamp(now, timezone.utc).isoformat(),
            "sample": False,
            "days": out_days,
            "models": [
                {"id": name, "tokens": int(tokens)}
                for name, tokens in sorted(totals.items(), key=lambda kv: -kv[1])[:12]
            ],
            "workers": [
                {
                    "peer": w["peer_id"][:16],
                    "label": w["label"] or w["peer_id"][:8],
                    "country": w["country"],
                    "country_src": w["country_src"],
                    "caps": json.loads(w["caps"] or "[]"),
                    "models": len(json.loads(w["models"] or "[]")),
                    "tokens_24h": int(w["tokens_24h"] or 0),
                    "requests_24h": int(w["requests_24h"] or 0),
                    "online": (now - int(w["last_seen"])) < 1800,
                }
                for w in worker_rows
            ],
        }


# Up to and including this tag, the release pipeline's own "stable filename"
# step fetched assets back off the release to re-upload them — two dmg-glob
# downloads on each of two macOS jobs, one AppImage, one msi: six fetches a
# release that GitHub counted like anyone else's. v0.1.22 onwards uploads
# from the runner's own build, so the counter is clean from there.
_LAST_INFLATED = (0, 1, 21)
_CI_FETCHES_PER_RELEASE = 6


def _release_ci_fetches(tag: str) -> int:
    """What the pipeline itself contributed to this release's count."""
    if not tag.startswith("v"):
        return 0
    try:
        parts = tuple(int(x) for x in tag[1:].split("."))
    except ValueError:
        return 0
    return _CI_FETCHES_PER_RELEASE if parts <= _LAST_INFLATED else 0


def _github_block(latest_day, gh_assets):
    """GitHub's counter, and the same number with the pipeline's own fetches
    taken back out. The raw figure stays published — it is what GitHub shows
    anyone who looks — but the headline is the one that means people."""
    raw = sum(c for _, _, c in gh_assets)
    by_tag: dict[str, int] = {}
    for t, _, c in gh_assets:
        by_tag[t] = by_tag.get(t, 0) + c
    ci = sum(min(by_tag[t], _release_ci_fetches(t)) for t in by_tag)
    return {
        "snapshot_day": latest_day,
        "total": raw - ci,
        "raw_total": raw,
        "ci_estimate": ci,
        "assets": [{"tag": t, "asset": a, "count": c} for t, a, c in gh_assets],
    }
