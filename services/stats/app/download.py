"""Pick a GitHub-release installer from a browser User-Agent.

The files themselves live on GitHub Releases under stable names. This
module only decides which name to send someone to.
"""

from __future__ import annotations

import os

GITHUB_REPO = "rootmodeai/rootmode"

ASSETS = {
    "macos-arm64": "rootmode-macos-arm64.dmg",
    "macos-x64": "rootmode-macos-x64.dmg",
    "windows-x64": "rootmode-windows-x64.msi",
    "linux-x64": "rootmode-linux-x86_64.AppImage",
}


def _repo(repo: str | None) -> str:
    return repo.strip() if repo and repo.strip() else GITHUB_REPO


def latest_url(filename: str, repo: str | None = None) -> str:
    return f"https://github.com/{_repo(repo)}/releases/latest/download/{filename}"


def releases_url(repo: str | None = None) -> str:
    return f"https://github.com/{_repo(repo)}/releases/latest"


def platform_for(user_agent: str) -> str | None:
    """A key in ASSETS, or None when this is not a desktop OS we ship."""
    ua = (user_agent or "").lower()
    if "android" in ua:
        return None
    if "iphone" in ua or "ipad" in ua or "ipod" in ua:
        return None
    if "windows" in ua or "win32" in ua or "win64" in ua:
        return "windows-x64"
    if "mac os" in ua or "macintosh" in ua:
        # navigator.platform is "MacIntel" on Apple Silicon too, so the
        # User-Agent cannot tell Intel from ARM. Almost every Mac that
        # will download this is ARM; Intel is linked separately.
        return "macos-arm64"
    if "linux" in ua or "x11" in ua or "cros" in ua:
        return "linux-x64"
    return None


def download_url(user_agent: str, repo: str | None = None) -> str:
    key = platform_for(user_agent)
    if key is None:
        return releases_url(repo)
    return latest_url(ASSETS[key], repo)


# Explicit paths for the "also Windows / Linux" links on the site.
NAMED = {
    "macos": "macos-arm64",
    "macos-intel": "macos-x64",
    "windows": "windows-x64",
    "linux": "linux-x64",
}


def named_url(os_name: str, repo: str | None = None) -> str | None:
    key = NAMED.get(os_name)
    if key is None:
        return None
    return latest_url(ASSETS[key], repo)


_VERSION_TTL = 300
_version_cache: tuple[float, dict] | None = None


def latest_version(repo: str | None = None) -> dict | None:
    """Tag of the newest GitHub Release, or None if GitHub will not say."""
    global _version_cache
    import json
    import time
    import urllib.error
    import urllib.request

    now = time.time()
    if _version_cache and now - _version_cache[0] < _VERSION_TTL:
        return _version_cache[1]
    name = _repo(repo)
    req = urllib.request.Request(
        f"https://api.github.com/repos/{name}/releases/latest",
        headers={
            "Accept": "application/vnd.github+json",
            "User-Agent": "rootmode",
        },
    )
    token = os.environ.get("ROOTMODE_GITHUB_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=8) as resp:
            data = json.load(resp)
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, OSError):
        return None
    tag = str(data.get("tag_name") or "").strip()
    if not tag:
        return None
    ver = tag[1:] if tag.startswith("v") else tag
    body = {"version": ver, "tag": tag, "url": "https://rootmode.ai/download"}
    _version_cache = (now, body)
    return body


_COUNTS_TTL = 600
_counts_cache: tuple[float, list[tuple[str, str, int]]] | None = None


def release_counts(repo: str | None = None) -> list[tuple[str, str, int]] | None:
    """(tag, asset, download_count) for every asset of the last releases.

    GitHub's own counter: every fetch of the file, from the site's redirect
    or anywhere else. None if GitHub will not say.
    """
    global _counts_cache
    import json
    import time
    import urllib.error
    import urllib.request

    now = time.time()
    if _counts_cache and now - _counts_cache[0] < _COUNTS_TTL:
        return _counts_cache[1]
    name = _repo(repo)
    req = urllib.request.Request(
        f"https://api.github.com/repos/{name}/releases?per_page=30",
        headers={"Accept": "application/vnd.github+json", "User-Agent": "rootmode"},
    )
    token = os.environ.get("ROOTMODE_GITHUB_TOKEN") or os.environ.get("GITHUB_TOKEN")
    if token:
        req.add_header("Authorization", f"Bearer {token}")
    try:
        with urllib.request.urlopen(req, timeout=8) as resp:
            data = json.load(resp)
    except (urllib.error.URLError, TimeoutError, json.JSONDecodeError, OSError):
        return None
    rows: list[tuple[str, str, int]] = []
    for rel in data if isinstance(data, list) else []:
        tag = str(rel.get("tag_name") or "").strip()
        for asset in rel.get("assets") or []:
            name_ = str(asset.get("name") or "")
            if tag and name_:
                rows.append((tag, name_, int(asset.get("download_count") or 0)))
    _counts_cache = (now, rows)
    return rows
