"""Downloads and installs: counted from what already happens, never more.

Run with `pytest` from `services/stats`.
"""

from __future__ import annotations

import os
import tempfile

os.environ.setdefault("ROOTMODE_STATS_DB", os.path.join(tempfile.mkdtemp(), "stats.db"))

from fastapi.testclient import TestClient  # noqa: E402

from app import download  # noqa: E402
from app.main import app, store  # noqa: E402

client = TestClient(app)
MAC = "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0) AppleWebKit/537.36"
WIN = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36"


def growth() -> dict:
    return client.get("/stats.json").json()["growth"]


def test_a_download_redirect_is_counted_by_platform(monkeypatch):
    # GitHub is not consulted in a test.
    monkeypatch.setattr(download, "release_counts", lambda repo=None: None)
    monkeypatch.setattr("app.main.release_counts", lambda repo=None: None)
    before = growth()["downloads"]["total"]

    r = client.get("/download", headers={"user-agent": MAC}, follow_redirects=False)
    assert r.status_code == 302 and r.headers["location"].endswith("rootmode-macos-arm64.dmg")
    r = client.get("/download/windows", headers={"user-agent": WIN}, follow_redirects=False)
    assert r.status_code == 302

    g = growth()
    assert g["downloads"]["total"] == before + 2
    assert g["days"][-1]["downloads"] >= 2
    platforms = {p["platform"]: p["count"] for p in g["downloads"]["by_platform_30d"]}
    assert platforms.get("macos-arm64", 0) >= 1 and platforms.get("windows-x64", 0) >= 1
    # A visitor on a phone is sent to the releases page and counted as "other".
    r = client.get("/download", headers={"user-agent": "Mozilla/5.0 (iPhone)"}, follow_redirects=False)
    assert "releases/latest" in r.headers["location"]


def test_an_install_that_says_hello_is_counted_once_a_day(monkeypatch):
    monkeypatch.setattr("app.main.release_counts", lambda repo=None: None)
    monkeypatch.setattr("app.main.latest_version", lambda repo=None: {"version": "0.1.11", "tag": "v0.1.11", "url": "u"})
    before = growth()["installs"]["total"]
    hello = {
        "x-rootmode-install": "3f0c9a1e2b4d4c6a8e1f0a2b3c4d5e6f",
        "x-rootmode-version": "0.1.11",
        "x-rootmode-os": "macos",
        "x-rootmode-arch": "aarch64",
    }
    assert client.get("/version", headers=hello).json()["version"] == "0.1.11"
    # Twice in a day is still one install, alive on one day.
    client.get("/version", headers=hello)

    g = growth()
    assert g["installs"]["total"] == before + 1
    assert g["installs"]["active_24h"] >= 1
    assert g["days"][-1]["installs_new"] >= 1
    assert g["days"][-1]["installs_active"] >= 1
    versions = {v["version"]: v["count"] for v in g["installs"]["by_version_30d"]}
    assert versions.get("0.1.11", 0) >= 1


def test_a_check_without_a_hello_counts_nothing(monkeypatch):
    monkeypatch.setattr("app.main.release_counts", lambda repo=None: None)
    monkeypatch.setattr("app.main.latest_version", lambda repo=None: {"version": "0.1.11", "tag": "v0.1.11", "url": "u"})
    before = growth()["installs"]["total"]
    client.get("/version")
    # Too short to be an id this app would make: ignored, not stored.
    client.get("/version", headers={"x-rootmode-install": "abc"})
    assert growth()["installs"]["total"] == before


def test_githubs_own_counts_are_snapshotted(monkeypatch):
    monkeypatch.setattr("app.main.release_counts",
                        lambda repo=None: [("v0.1.11", "rootmode-macos-arm64.dmg", 40), ("v0.1.11", "rootmode-windows-x64.msi", 2)])
    g = growth()
    counts = {a["asset"]: a["count"] for a in g["github"]["assets"] if a["tag"] == "v0.1.11"}
    assert counts["rootmode-macos-arm64.dmg"] == 40 and counts["rootmode-windows-x64.msi"] == 2
    assert g["github"]["total"] >= 42
