"""Pick a GitHub-release installer from a browser User-Agent.

The files themselves live on GitHub Releases under stable names. This
module only decides which name to send someone to.
"""

from __future__ import annotations

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
