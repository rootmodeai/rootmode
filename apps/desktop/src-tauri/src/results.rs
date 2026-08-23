//! Turning a `job.result` message into something on disk.
//!
//! The peer's claimed `sha256` is never trusted: we hash the bytes ourselves
//! and refuse the result if they disagree. Filenames embed the hash, so the
//! content address is visible in the file manager, not just in the app.

use std::path::{Path, PathBuf};

use base64::Engine;
use rootmode_core::{sha256_hex, JobKind, JobResult};

use crate::error::{AppError, Result};
use crate::store::{now, ResultRecord};

/// Persist a result and return the record to store and show.
pub fn materialize(result: &JobResult, download_dir: &Path) -> Result<ResultRecord> {
    match result.kind {
        JobKind::Llm => {
            let text = result
                .text
                .clone()
                .ok_or_else(|| AppError::Invalid("llm result has no text".into()))?;
            let actual = sha256_hex(text.as_bytes());
            verify(&actual, &result.sha256)?;
            Ok(ResultRecord {
                job_id: result.job_id,
                kind: JobKind::Llm,
                sha256: actual,
                text: Some(text),
                image_path: None,
                meta: result.meta.clone(),
                created_at: now(),
            })
        }
        JobKind::Image | JobKind::Video => {
            let what = if result.kind == JobKind::Video {
                "video"
            } else {
                "image"
            };
            let payload = result.image_path_or_b64.as_deref().ok_or_else(|| {
                AppError::Invalid(format!("{what} result has no {what} data"))
            })?;
            let bytes = decode_image_payload(payload)?;
            let actual = sha256_hex(&bytes);
            verify(&actual, &result.sha256)?;

            let path = write_image(&bytes, &actual, download_dir)?;
            Ok(ResultRecord {
                job_id: result.job_id,
                kind: result.kind,
                sha256: actual,
                text: None,
                image_path: Some(path.to_string_lossy().into_owned()),
                meta: result.meta.clone(),
                created_at: now(),
            })
        }
    }
}

fn verify(actual: &str, claimed: &str) -> Result<()> {
    if claimed.is_empty() || actual.eq_ignore_ascii_case(claimed) {
        Ok(())
    } else {
        Err(AppError::Invalid(format!(
            "hash mismatch: peer claimed {claimed}, bytes hash to {actual}"
        )))
    }
}

/// `image_path_or_b64` is base64 over the wire. A filesystem path is accepted
/// only for a worker running on this machine — a remote peer naming a local
/// path would otherwise be an arbitrary-file-read primitive.
fn decode_image_payload(payload: &str) -> Result<Vec<u8>> {
    if looks_like_path(payload) {
        let path = PathBuf::from(payload);
        if !path.is_absolute() {
            return Err(AppError::Invalid("image path must be absolute".into()));
        }
        return std::fs::read(&path).map_err(|e| {
            AppError::Invalid(format!("cannot read image at {}: {e}", path.display()))
        });
    }
    base64::engine::general_purpose::STANDARD
        .decode(payload.trim())
        .map_err(|e| AppError::Invalid(format!("image is not valid base64: {e}")))
}

fn looks_like_path(s: &str) -> bool {
    s.starts_with('/') || (s.len() > 2 && s.as_bytes()[1] == b':' && s.contains('\\'))
}

fn write_image(bytes: &[u8], sha256: &str, download_dir: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(download_dir)?;
    let ext = detect_ext(bytes);
    let path = download_dir.join(format!("rootmode-{}.{ext}", &sha256[..16]));
    // Same bytes, same name: rewriting is a no-op, so skip it.
    if !path.exists() {
        std::fs::write(&path, bytes)?;
    }
    Ok(path)
}

fn detect_ext(bytes: &[u8]) -> &'static str {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        "png"
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        "jpg"
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        "webp"
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        "gif"
    } else if bytes.get(4..8) == Some(b"ftyp") {
        "mp4"
    } else if bytes.starts_with(b"\x1A\x45\xDF\xA3") {
        "webm"
    } else {
        "bin"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rootmode_core::PROTOCOL_VERSION;
    use uuid::Uuid;

    fn dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("rootmode-res-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn image_result(bytes: &[u8], claimed: Option<&str>) -> JobResult {
        JobResult {
            v: PROTOCOL_VERSION,
            job_id: Uuid::new_v4(),
            kind: JobKind::Image,
            tool_calls: Vec::new(),
            sha256: claimed
                .map(str::to_string)
                .unwrap_or_else(|| sha256_hex(bytes)),
            text: None,
            image_path_or_b64: Some(base64::engine::general_purpose::STANDARD.encode(bytes)),
            thinking: None,
            meta: serde_json::json!({}),
        }
    }

    #[test]
    fn writes_png_named_by_hash() {
        let d = dir();
        let bytes = b"\x89PNG\r\n\x1a\nnot-really-a-png-but-close-enough";
        let rec = materialize(&image_result(bytes, None), &d).unwrap();
        let path = PathBuf::from(rec.image_path.unwrap());
        assert!(path.exists());
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".png"));
        assert!(path.to_string_lossy().contains(&rec.sha256[..16]));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }

    #[test]
    fn rejects_hash_mismatch() {
        let d = dir();
        let err = materialize(
            &image_result(b"\x89PNG\r\n\x1a\nx", Some(&"a".repeat(64))),
            &d,
        )
        .unwrap_err();
        assert!(err.to_string().contains("hash mismatch"));
    }

    #[test]
    fn path_payloads_are_read_only_when_absolute_and_present() {
        // Relative, path-shaped input is never opened — it falls through to
        // base64 and fails there.
        assert!(decode_image_payload("etc/passwd").is_err());
        // An absolute path that is not there is an error, not an empty image.
        let err = decode_image_payload("/definitely/not/here.png").unwrap_err();
        assert!(err.to_string().contains("cannot read image"));
    }

    #[test]
    fn writes_mp4_named_by_hash() {
        let d = dir();
        // ISO-BMFF: a 4-byte size then `ftyp`. Not a playable file — just
        // enough for the extension so the player is given the right name.
        let mut bytes = vec![0, 0, 0, 24];
        bytes.extend_from_slice(b"ftypisom");
        bytes.extend_from_slice(&[0u8; 12]);
        let rec = materialize(
            &JobResult {
                v: PROTOCOL_VERSION,
                job_id: Uuid::new_v4(),
                kind: JobKind::Video,
                tool_calls: Vec::new(),
                sha256: sha256_hex(&bytes),
                text: None,
                image_path_or_b64: Some(base64::engine::general_purpose::STANDARD.encode(&bytes)),
                thinking: None,
                meta: serde_json::json!({}),
            },
            &d,
        )
        .unwrap();
        let path = PathBuf::from(rec.image_path.unwrap());
        assert!(path
            .file_name()
            .unwrap()
            .to_string_lossy()
            .ends_with(".mp4"));
        assert_eq!(rec.kind, JobKind::Video);
    }
}
