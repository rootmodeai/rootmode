"""Canonical JSON and signature checking.

Must produce byte-for-byte what `rootmode_core::canonical::canonical_bytes`
produces, or every signature fails: keys sorted, no insignificant whitespace,
`sig` absent from the pre-image.
"""

from __future__ import annotations

import json

from nacl.exceptions import BadSignatureError
from nacl.signing import VerifyKey


def canonical_bytes(payload: dict) -> bytes:
    """Sorted keys, no spaces — the same pre-image the worker signed."""
    return json.dumps(payload, sort_keys=True, separators=(",", ":")).encode()


def verify(peer_id: str, payload: dict, sig_hex: str) -> bool:
    """Did the holder of `peer_id` sign this body?

    A peer id *is* an ed25519 public key, so this is the whole of the identity
    check — there is no registry to consult and nothing to look up.
    """
    try:
        key = VerifyKey(bytes.fromhex(peer_id))
        key.verify(canonical_bytes(payload), bytes.fromhex(sig_hex))
        return True
    except (BadSignatureError, ValueError, TypeError):
        return False
