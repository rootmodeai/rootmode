"""What the collector must and must not accept.

Run with `pytest` from `services/stats`.
"""

from __future__ import annotations

import json
import os
import tempfile

import pytest
from fastapi.testclient import TestClient
from nacl.signing import SigningKey

os.environ.setdefault("ROOTMODE_STATS_DB", os.path.join(tempfile.mkdtemp(), "stats.db"))

from app.canonical import canonical_bytes  # noqa: E402
from app.main import app, store  # noqa: E402

client = TestClient(app)


def signed(body: dict, key: SigningKey | None = None) -> dict:
    key = key or SigningKey.generate()
    body = dict(body)
    body["peer_id"] = key.verify_key.encode().hex()
    body.pop("sig", None)
    sig = key.sign(canonical_bytes(body)).signature.hex()
    return {**body, "sig": sig}


def a_report(**over) -> dict:
    return {
        "v": 1,
        "label": "test-box",
        "caps": ["llm"],
        "models": ["llama-3.1-70b"],
        "window_secs": 300,
        "requests": 12,
        "images": 0,
        "tokens_in": 40_000,
        "tokens_out": 12_000,
        "revenue": 0.031,
        "failures": 0,
        "currency": "USD",
        **over,
    }


def test_a_signed_report_is_accepted_and_counted():
    r = client.post("/report", json=signed(a_report()))
    assert r.status_code == 200, r.text
    assert r.json()["ok"] is True

    stats = client.get("/stats.json").json()
    today = stats["days"][-1]
    assert today["tokens_in"] >= 40_000
    assert today["requests"] >= 12
    assert stats["sample"] is False


def test_an_unsigned_report_is_refused():
    body = a_report()
    body["peer_id"] = "ab" * 32
    r = client.post("/report", json=body)
    # Without this, one machine could invent a thousand workers and every
    # chart on the explorer would be fiction.
    assert r.status_code == 401


def test_a_report_signed_by_the_wrong_key_is_refused():
    body = signed(a_report())
    other = SigningKey.generate()
    body["peer_id"] = other.verify_key.encode().hex()  # claim someone else's id
    r = client.post("/report", json=body)
    assert r.status_code == 401


def test_edited_numbers_break_the_signature():
    body = signed(a_report())
    body["tokens_in"] = 999_999_999  # a middlebox inflating the chart
    r = client.post("/report", json=body)
    assert r.status_code == 401


def test_absurd_counts_are_refused_before_they_move_a_chart():
    r = client.post("/report", json=signed(a_report(tokens_out=10**15)))
    assert r.status_code == 422


def test_a_declared_country_beats_geolocation():
    r = client.post("/report", json=signed(a_report(country="de")))
    assert r.status_code == 200
    assert r.json()["country"] == "DE"

    workers = client.get("/stats.json").json()["workers"]
    mine = [w for w in workers if w["country"] == "DE"]
    assert mine and mine[0]["country_src"] == "declared"


def test_a_loopback_address_is_not_given_a_country():
    # The test client connects from 127.0.0.1: a private address says nothing
    # about where in the world a machine is, so it is left blank.
    r = client.post("/report", json=signed(a_report()))
    assert r.json()["country"] is None


def test_reporting_too_often_is_throttled():
    key = SigningKey.generate()
    codes = [
        client.post("/report", json=signed(a_report(requests=1), key)).status_code
        for _ in range(260)
    ]
    assert 429 in codes, "one key cannot write without limit"


def test_the_document_has_the_shape_the_explorer_reads():
    doc = client.get("/stats.json?days=7").json()
    assert set(doc) >= {"updated", "days", "models", "workers", "sample"}
    assert len(doc["days"]) == 7
    for day in doc["days"]:
        assert set(day) >= {
            "date", "tokens_in", "tokens_out", "requests",
            "revenue", "peers_returning", "peers_new",
        }
    # Quiet days are zeroes rather than gaps, so a chart cannot silently skip
    # a Sunday.
    assert all(isinstance(d["tokens_in"], int) for d in doc["days"])


def test_no_prompt_or_client_ever_reaches_storage():
    client.post("/report", json=signed(a_report()))
    with store.connect() as conn:
        columns = {
            row["name"]
            for table in ("reports", "workers")
            for row in conn.execute(f"PRAGMA table_info({table})")
        }
    # There is nowhere to put one, which is the point.
    for forbidden in ("prompt", "text", "result", "client", "address", "remote"):
        assert not any(forbidden in c for c in columns), f"{forbidden} in {columns}"
    # The one address-derived column is a salted one-way hash, and with no
    # salt configured it is not even written.
    assert "ip_hash" in columns
    with store.connect() as conn:
        stored = {row["ip_hash"] for row in conn.execute("SELECT ip_hash FROM workers")}
    assert "127.0.0.1" not in stored


def test_download_sends_a_mac_visitor_the_dmg():
    r = client.get("/download", headers={"user-agent": "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)"}, follow_redirects=False)
    assert r.status_code == 302
    assert r.headers["location"].endswith("rootmode-macos-arm64.dmg")


def test_download_sends_a_windows_visitor_the_msi():
    r = client.get("/download", headers={"user-agent": "Mozilla/5.0 (Windows NT 10.0; Win64; x64)"}, follow_redirects=False)
    assert r.status_code == 302
    assert r.headers["location"].endswith("rootmode-windows-x64.msi")


def test_download_sends_a_linux_visitor_the_appimage():
    r = client.get("/download", headers={"user-agent": "Mozilla/5.0 (X11; Linux x86_64)"}, follow_redirects=False)
    assert r.status_code == 302
    assert r.headers["location"].endswith("rootmode-linux-x86_64.AppImage")


def test_named_download_paths():
    r = client.get("/download/windows", follow_redirects=False)
    assert r.status_code == 302
    assert r.headers["location"].endswith("rootmode-windows-x64.msi")
    assert client.get("/download/beos", follow_redirects=False).status_code == 404


def test_both_url_spellings_reach_the_same_handler():
    # Workers are configured with the short one on the site's own domain; the
    # versioned one exists for the day two shapes have to be live at once.
    body = signed(a_report())
    assert client.post("/v1/report", json=body).status_code == 200
    assert client.get("/v1/stats.json?days=1").status_code == 200
    assert client.get("/stats.json?days=1").status_code == 200


def test_canonical_bytes_match_the_rust_form():
    # Sorted keys, no whitespace, `sig` excluded — byte-for-byte what
    # rootmode_core::canonical produces, or every signature fails.
    assert canonical_bytes({"b": 2, "a": 1}) == b'{"a":1,"b":2}'
    assert json.loads(canonical_bytes({"a": [1, 2]})) == {"a": [1, 2]}


def test_a_report_signed_by_the_rust_worker_verifies_here():
    """The failure this catches only shows up in production.

    Two languages serialising the same report differently — a float, a key
    order, an omitted field — means every signature fails and the collector
    silently records nothing. The fixture is real output from
    `cargo run -p rootmode-worker --example signed_report`, awkward float and
    all.
    """
    fixture = os.path.join(os.path.dirname(__file__), "fixtures", "rust_report.json")
    with open(fixture) as f:
        body = json.load(f)

    r = client.post("/report", json=body)
    assert r.status_code == 200, r.text
    assert r.json()["country"] == "DE"
