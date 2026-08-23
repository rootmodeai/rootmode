//! Canonical JSON used as the signing pre-image.
//!
//! Rules (v1):
//!   * object keys sorted lexicographically (serde_json's default `Map` is a
//!     `BTreeMap`, so `to_string` is already sorted)
//!   * no insignificant whitespace
//!   * the `sig` field is removed before signing/verifying
//!
//! Deliberately simple: no float canonicalisation games. Job payloads use
//! small integers and short floats; if that ever changes, bump the version.

use serde::Serialize;

use crate::{CoreError, Result};

/// Serialize `value` to canonical JSON bytes, stripping any top-level `sig`.
pub fn canonical_bytes<T: Serialize>(value: &T) -> Result<Vec<u8>> {
    let mut v = serde_json::to_value(value)?;
    if let Some(obj) = v.as_object_mut() {
        obj.remove("sig");
    }
    Ok(serde_json::to_string(&v)?.into_bytes())
}

/// Compact JSON with sorted keys, including `sig`. This is what goes on the
/// wire so the receiver can recompute the same preimage we signed.
pub fn wire_json<T: Serialize>(value: &T) -> Result<String> {
    let v = serde_json::to_value(value)?;
    Ok(serde_json::to_string(&v)?)
}

/// Same, but for an already-parsed `Value`.
pub fn canonical_bytes_of(value: &serde_json::Value) -> Result<Vec<u8>> {
    let mut v = value.clone();
    match v.as_object_mut() {
        Some(obj) => {
            obj.remove("sig");
        }
        None => return Err(CoreError::Invalid("expected a JSON object".into())),
    }
    Ok(serde_json::to_string(&v)?.into_bytes())
}
