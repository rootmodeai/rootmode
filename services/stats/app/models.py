"""What a worker sends, and what the explorer reads back.

The report shape mirrors `crates/rootmode-worker/src/stats.rs`. If the two
disagree the Rust side is the truth, because that is what is deployed on other
people's machines.
"""

from __future__ import annotations

from pydantic import BaseModel, Field, field_validator

# A frame far bigger than any honest report, refused before it is parsed.
MAX_BODY_BYTES = 64 * 1024

# One window may not claim more than this. A node reporting a trillion tokens
# in five minutes is broken or lying, and either way it should not be able to
# move a public chart on its own.
MAX_TOKENS_PER_WINDOW = 50_000_000_000
MAX_REQUESTS_PER_WINDOW = 5_000_000


class Report(BaseModel):
    """One node's account of what it served over one window."""

    v: int = 1
    peer_id: str = Field(min_length=64, max_length=64)
    label: str = Field(default="", max_length=64)
    country: str | None = Field(default=None, max_length=2)
    caps: list[str] = Field(default_factory=list, max_length=8)
    models: list[str] = Field(default_factory=list, max_length=200)
    window_secs: int = Field(default=300, ge=1, le=86_400)

    requests: int = Field(default=0, ge=0, le=MAX_REQUESTS_PER_WINDOW)
    images: int = Field(default=0, ge=0, le=MAX_REQUESTS_PER_WINDOW)
    tokens_in: int = Field(default=0, ge=0, le=MAX_TOKENS_PER_WINDOW)
    tokens_out: int = Field(default=0, ge=0, le=MAX_TOKENS_PER_WINDOW)
    tokens_cached: int = Field(default=0, ge=0, le=MAX_TOKENS_PER_WINDOW)
    revenue: float = Field(default=0.0, ge=0.0, le=1_000_000.0)
    failures: int = Field(default=0, ge=0, le=MAX_REQUESTS_PER_WINDOW)
    # Turned away before any work started. Kept apart from `failures`: one is
    # a node breaking, the other is a node enforcing its own rules.
    rejected: int = Field(default=0, ge=0, le=MAX_REQUESTS_PER_WINDOW)

    currency: str = Field(default="USD", max_length=8)
    sig: str | None = None

    @field_validator("peer_id")
    @classmethod
    def _hex_key(cls, v: str) -> str:
        # The peer id is an ed25519 public key, which is what the signature is
        # checked against — anything else cannot be verified at all.
        int(v, 16)
        return v.lower()

    @field_validator("country")
    @classmethod
    def _country(cls, v: str | None) -> str | None:
        if not v:
            return None
        v = v.strip().upper()
        return v if len(v) == 2 and v.isalpha() else None

