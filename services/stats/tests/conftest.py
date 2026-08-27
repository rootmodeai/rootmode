"""Tests never reach GitHub: the release-count snapshot is stubbed to "GitHub
would not say" unless a test patches in numbers of its own."""

import pytest


@pytest.fixture(autouse=True)
def _no_github(monkeypatch):
    monkeypatch.setattr("app.main.release_counts", lambda repo=None: None)
