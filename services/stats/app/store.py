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

    def reports_since(self, peer_id: str, since: int) -> int:
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
