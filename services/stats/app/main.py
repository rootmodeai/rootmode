"""rootmode stats collector.

Workers post signed counts of what they served; the explorer reads a single
aggregated document back. That is the whole service.

It is centralised, and that is a real trade — so the boundary is drawn as
tightly as it can be:

It shares a process and a domain with the site: `rootmode.ai` serves the pages,
`rootmode.ai/report` takes reports, `rootmode.ai/stats.json` feeds the explorer.
One box, one certificate, no cross-origin anything.

* **Workers report; installs only say hello.** A client never reports what it
  did, so no prompt, no answer, and no client peer id passes through here.
  What is collected is what a machine served, by a machine that chose to say
  so — and, from the desktop's daily update check, that an install exists:
  a random id it made up, its version and OS, nothing more, and off in its
  Settings.
* **Reports are signed.** A peer id is an ed25519 public key, so the collector
  can tell a real node's numbers from an invented one's without an account,
  an API key, or a registry.
* **Addresses are used, not kept.** The connecting address is resolved to a
  country and dropped. What persists is two letters and, if a salt is set, a
  one-way hash.
* **Reporting is opt-in.** A worker with no `[stats] url` never contacts this
  service, and is a full member of the network regardless. Every number here
  is therefore a floor, never a census.
"""

from __future__ import annotations

import json
import os
import time

from fastapi import FastAPI, Header, Request, Response
from fastapi.middleware.cors import CORSMiddleware
from fastapi.responses import FileResponse, JSONResponse, RedirectResponse
from fastapi.staticfiles import StaticFiles

from .canonical import verify
from .download import NAMED, download_url, latest_version, named_url, platform_for, release_counts
from .geo import client_ip, country_of, fingerprint
from .models import MAX_BODY_BYTES, Report
from .store import Store

DB_PATH = os.environ.get("ROOTMODE_STATS_DB", "/var/lib/rootmode/stats.db")
REQUIRE_SIG = os.environ.get("ROOTMODE_REQUIRE_SIG", "true").lower() in ("1", "true", "yes")
# A node reporting every 5 minutes files 12 an hour. The cap is generous
# enough for a 30-second interval and still bounds what one key can write.
MAX_REPORTS_PER_HOUR = int(os.environ.get("ROOTMODE_MAX_REPORTS_PER_HOUR", "240"))
ORIGINS = [o for o in os.environ.get("ROOTMODE_CORS", "*").split(",") if o]
# The site itself, served by this same process when the directory is there.
# One box, one port, same origin — which also means the explorer's fetch of
# /v1/stats.json is not a cross-origin request at all.
WEB_DIR = os.environ.get("ROOTMODE_WEB_DIR", "/srv/web")

app = FastAPI(
    title="rootmode stats",
    description="Workers report what they served. Clients report nothing.",
    version="1.0.0",
)
app.add_middleware(
    CORSMiddleware,
    # The explorer is a static page on another origin; it only ever reads.
    allow_origins=ORIGINS or ["*"],
    allow_methods=["GET", "POST"],
    allow_headers=["content-type"],
)

store = Store(DB_PATH)


# Both spellings, one handler. `/report` is what workers are configured with —
# short, on the site's own domain, no subdomain to certificate — and `/v1/report`
# is there for the day the shape changes and both have to be served at once.
@app.post("/report")
@app.post("/v1/report")
async def report(request: Request, content_length: int | None = Header(default=None)):
    """Accept one window of counters from one worker."""
    if content_length is not None and content_length > MAX_BODY_BYTES:
        return JSONResponse({"error": "report too large"}, status_code=413)

    raw = await request.body()
    if len(raw) > MAX_BODY_BYTES:
        return JSONResponse({"error": "report too large"}, status_code=413)

    # The signature covers the bytes the worker sent, so it is checked against
    # those and not against the parsed model: validation upper-cases a country
    # code and fills defaults, and verifying the tidied version would reject
    # every honest report while passing nothing extra.
    try:
        received = json.loads(raw)
        if not isinstance(received, dict):
            raise ValueError("not an object")
    except ValueError as e:
        return JSONResponse({"error": f"not a report: {e}"}, status_code=422)

    try:
        payload = Report.model_validate(received)
    except Exception as e:
        return JSONResponse({"error": f"not a report: {e}"}, status_code=422)

    if payload.v != 1:
        return JSONResponse({"error": f"unknown report version {payload.v}"}, status_code=400)

    # Anyone can post; only a key holder can post *as* a node. Without this a
    # single machine could invent a thousand workers and every chart on the
    # explorer would be fiction.
    if REQUIRE_SIG:
        if not payload.sig:
            return JSONResponse({"error": "unsigned report"}, status_code=401)
        signed_over = {k: v for k, v in received.items() if k != "sig"}
        if not verify(payload.peer_id, signed_over, payload.sig):
            return JSONResponse({"error": "signature does not match peer_id"}, status_code=401)

    now = int(time.time())
    if store.reports_since(payload.peer_id, now - 3600) >= MAX_REPORTS_PER_HOUR:
        return JSONResponse({"error": "reporting too often"}, status_code=429)

    ip = client_ip(request.headers, request.client.host if request.client else None)
    geo = country_of(ip)
    # The operator's own word wins: they know where the box is, and a
    # geolocation database is guessing from a route.
    country = payload.country or geo
    source = "declared" if payload.country else ("geoip" if geo else None)

    store.record(
        payload,
        country=country,
        country_src=source,
        ip_hash=fingerprint(ip),
        now=now,
    )
    return {"ok": True, "country": country, "next_report_in": payload.window_secs}


@app.get("/stats.json")
@app.get("/v1/stats.json")
def stats(response: Response, days: int = 120):
    """Everything the explorer draws, in one document."""
    days = max(1, min(days, 365))
    # Cached briefly: the page is public and the numbers move on a 5-minute
    # cadence at best, so there is nothing to gain from computing this per hit.
    response.headers["Cache-Control"] = "public, max-age=60"
    doc = store.stats(days=days)
    # Downloads and installs ride along, so the explorer shows the people
    # side as well as the machine side. GitHub's own per-asset counts are
    # snapshotted here too, once a day, cached so the API is not hammered.
    repo = os.environ.get("ROOTMODE_GITHUB_REPO") or None
    rows = release_counts(repo)
    if rows:
        store.record_release_counts(rows, int(time.time()))
    doc["growth"] = store.growth(days=days)
    return doc


@app.get("/report")
def report_help():
    """Somebody opened it in a browser.

    Without this the static mount answers 404 and the operator wonders whether
    they deployed the collector at all.
    """
    return {
        "ok": True,
        "hint": "POST a signed worker report here. See services/stats/README.md.",
        "stats": "/stats.json",
    }


@app.get("/healthz")
def healthz():
    return {"ok": True}


def _count_download(request: Request, platform: str, source: str) -> None:
    """One more person sent to an installer. The address becomes a country and is gone."""
    now = int(time.time())
    ip = client_ip(request.headers, request.client.host if request.client else None)
    country = country_of(ip)
    store.record_download(platform=platform, source=source, country=country, now=now)


@app.get("/download")
def download(request: Request):
    """Send the visitor the installer for their OS. No picker."""
    repo = os.environ.get("ROOTMODE_GITHUB_REPO") or None
    ua = request.headers.get("user-agent", "")
    _count_download(request, platform_for(ua) or "other", "button")
    return RedirectResponse(download_url(ua, repo), status_code=302)


# What the desktop says about itself while asking for the newest version.
# Headers, not the query string, so nothing about an install is written into
# an access log. Absent — the person switched it off — nothing is recorded.
_HELLO_ID = "x-rootmode-install"
_HELLO_VERSION = "x-rootmode-version"
_HELLO_OS = "x-rootmode-os"
_HELLO_ARCH = "x-rootmode-arch"


def _clean_hello(value: str | None, limit: int) -> str:
    value = (value or "").strip()
    return "".join(c for c in value if c.isalnum() or c in ".-_")[:limit]


@app.get("/version")
def version(request: Request):
    """What the desktop compares itself against. None if GitHub is silent."""
    install = _clean_hello(request.headers.get(_HELLO_ID), 64)
    if len(install) >= 16:
        now = int(time.time())
        ip = client_ip(request.headers, request.client.host if request.client else None)
        country = country_of(ip)
        ver = _clean_hello(request.headers.get(_HELLO_VERSION), 32)
        os_ = _clean_hello(request.headers.get(_HELLO_OS), 16)
        arch = _clean_hello(request.headers.get(_HELLO_ARCH), 16)
        store.record_heartbeat(install_id=install, version=ver, os=os_, arch=arch,
                               country=country, now=now)
    repo = os.environ.get("ROOTMODE_GITHUB_REPO") or None
    body = latest_version(repo)
    if body is None:
        return JSONResponse({"version": None, "tag": None, "url": "https://rootmode.ai/download"})
    return body


@app.get("/download/{os_name}")
def download_named(os_name: str, request: Request):
    repo = os.environ.get("ROOTMODE_GITHUB_REPO") or None
    url = named_url(os_name, repo)
    if url is None:
        return JSONResponse({"detail": "not found"}, status_code=404)
    _count_download(request, NAMED[os_name], "named")
    return RedirectResponse(url, status_code=302)


# Pages are files under pages/, not public *.html URLs.
_PAGES = {
    "/": "index.html",
    "/manifesto": "pages/manifesto.html",
    "/worker": "pages/worker.html",
    "/explorer": "pages/explorer.html",
    "/protocol": "pages/protocol.html",
    "/discovery": "pages/discovery.html",
    "/brand": "pages/brand.html",
}


def _html_page(name: str):
    path = os.path.join(WEB_DIR, name)
    if not os.path.isfile(path):
        return JSONResponse({"detail": "not found"}, status_code=404)
    return FileResponse(path, media_type="text/html; charset=utf-8")


def _add_page(url: str, filename: str) -> None:
    async def page():
        return _html_page(filename)

    page.__name__ = f"page_{filename.replace('/', '_').replace('.', '_')}"
    app.add_api_route(url, page, methods=["GET"], include_in_schema=False)
    if url != "/":
        async def slash():
            return RedirectResponse(url, status_code=301)

        slash.__name__ = f"slash_{filename.replace('/', '_').replace('.', '_')}"
        app.add_api_route(url + "/", slash, methods=["GET"], include_in_schema=False)


for _url, _file in _PAGES.items():
    _add_page(_url, _file)

# Only /assets is a public directory — not the page files, not package.json.
_assets = os.path.join(WEB_DIR, "assets")
if os.path.isdir(_assets):
    app.mount("/assets", StaticFiles(directory=_assets), name="assets")
