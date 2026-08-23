"""Where a report came from.

The address is used and thrown away. What is kept is a two-letter country
code, and — so the same node can be recognised across reports without holding
the address that identifies it — a salted hash. Neither can be turned back
into an IP, and the salt is per-deployment.

Resolution order:

1. What the operator declared in their config. They know where their machine
   is; a database only guesses.
2. A local MaxMind GeoLite2 country database, if one is on disk. Local so no
   third party sees the addresses of the network's workers, which is the whole
   reason not to call an API here.
3. Nothing. A worker with no country is shown as "not stated" rather than
   assigned one.
"""

from __future__ import annotations

import hashlib
import ipaddress
import os
from functools import lru_cache

_MMDB = os.environ.get("ROOTMODE_GEOIP_DB", "/var/lib/rootmode/GeoLite2-Country.mmdb")
_SALT = os.environ.get("ROOTMODE_IP_SALT", "")


@lru_cache(maxsize=1)
def _reader():
    """The MaxMind reader, or None when no database is installed."""
    if not os.path.exists(_MMDB):
        return None
    try:
        import geoip2.database

        return geoip2.database.Reader(_MMDB)
    except Exception:  # pragma: no cover - a missing optional dependency
        return None


def country_of(ip: str | None) -> str | None:
    """The country a public address sits in, or None."""
    if not ip:
        return None
    try:
        parsed = ipaddress.ip_address(ip)
    except ValueError:
        return None
    # A worker on a private range is on somebody's LAN, and the address says
    # nothing about where in the world that is.
    if parsed.is_private or parsed.is_loopback or parsed.is_link_local:
        return None

    reader = _reader()
    if reader is None:
        return None
    try:
        answer = reader.country(ip)
        return (answer.country.iso_code or None) if answer else None
    except Exception:
        return None


def fingerprint(ip: str | None) -> str | None:
    """A stable, non-reversible handle for an address.

    Lets the collector notice that fifty reports came from one machine without
    keeping the machine's address. With no salt configured it returns None —
    an unsalted hash of an IPv4 address is trivially reversible by brute force,
    and a false sense of anonymity is worse than none.
    """
    if not ip or not _SALT:
        return None
    return hashlib.sha256(f"{_SALT}:{ip}".encode()).hexdigest()[:16]


def client_ip(headers, fallback: str | None) -> str | None:
    """The address a report actually came from.

    `X-Forwarded-For` is only trusted when the deployment says it is behind a
    proxy: taken on faith, any client could claim to be in any country and put
    itself on the map.
    """
    if os.environ.get("ROOTMODE_TRUST_PROXY", "").lower() in ("1", "true", "yes"):
        forwarded = headers.get("x-forwarded-for")
        if forwarded:
            return forwarded.split(",")[0].strip()
    return fallback
