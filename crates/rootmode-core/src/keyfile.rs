//! Key material on disk.
//!
//! The one exception to this crate's no-filesystem rule: both the desktop
//! client and the worker need to persist an identity, and having two
//! implementations of "write a secret with the right permissions" is how one of
//! them ends up world-readable.
//!
//! The secret is a 32-byte ed25519 seed, hex-encoded. On unix the file is
//! `0600` and its directory `0700`.

use std::path::Path;

use crate::{identity::Identity, CoreError, Result};

/// Load the identity at `path`, or generate and persist a new one.
pub fn load_or_create(path: &Path) -> Result<Identity> {
    if path.exists() {
        let hex = std::fs::read_to_string(path).map_err(io)?;
        return Identity::from_secret_hex(hex.trim());
    }
    let identity = Identity::generate();
    write_secret(path, &identity.export_secret_hex())?;
    Ok(identity)
}

/// Replace whatever is at `path` with `secret_hex`. The caller is responsible
/// for warning that the previous key is gone.
pub fn import(path: &Path, secret_hex: &str) -> Result<Identity> {
    let identity = Identity::from_secret_hex(secret_hex)?;
    write_secret(path, &identity.export_secret_hex())?;
    Ok(identity)
}

fn write_secret(path: &Path, secret_hex: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).map_err(io)?;
            restrict(dir, 0o700)?;
        }
    }

    // Write-then-rename so an interrupted write cannot truncate a live key.
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, secret_hex).map_err(io)?;
    restrict(&tmp, 0o600)?;
    std::fs::rename(&tmp, path).map_err(io)?;
    restrict(path, 0o600)?;
    Ok(())
}

#[cfg(unix)]
fn restrict(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).map_err(io)
}

#[cfg(not(unix))]
fn restrict(_path: &Path, _mode: u32) -> Result<()> {
    // Windows inherits the user profile ACL, which is already user-only.
    Ok(())
}

fn io(e: std::io::Error) -> CoreError {
    CoreError::Key(e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn key_path() -> PathBuf {
        std::env::temp_dir()
            .join(format!("rootmode-key-{}", uuid::Uuid::new_v4()))
            .join("identity.key")
    }

    #[test]
    fn generates_once_then_loads() {
        let p = key_path();
        let a = load_or_create(&p).unwrap();
        let b = load_or_create(&p).unwrap();
        assert_eq!(a.peer_id(), b.peer_id());
    }

    #[test]
    fn import_replaces() {
        let p = key_path();
        let original = load_or_create(&p).unwrap();
        let other = Identity::generate();
        assert_eq!(
            import(&p, &other.export_secret_hex()).unwrap().peer_id(),
            other.peer_id()
        );
        assert_ne!(other.peer_id(), original.peer_id());
        assert_eq!(load_or_create(&p).unwrap().peer_id(), other.peer_id());
    }

    #[test]
    fn rejects_garbage_on_disk() {
        let p = key_path();
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, "not a key").unwrap();
        assert!(load_or_create(&p).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let p = key_path();
        load_or_create(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o077,
            0,
            "key file must not be group/world accessible"
        );
    }
}
