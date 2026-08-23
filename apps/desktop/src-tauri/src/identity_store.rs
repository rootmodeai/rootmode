//! The desktop client's key file: `identity.key` inside the app data
//! directory, never in the repo.
//!
//! The permission-sensitive part lives in [`rootmode_core::keyfile`] so the
//! worker binary and this client cannot drift apart on it.

use std::path::{Path, PathBuf};

use rootmode_core::{keyfile, Identity};

use crate::error::Result;

pub const KEY_FILE: &str = "identity.key";

pub fn key_path(app_data: &Path) -> PathBuf {
    app_data.join(KEY_FILE)
}

/// Load the existing identity, or generate and persist a new one.
pub fn load_or_create(app_data: &Path) -> Result<Identity> {
    Ok(keyfile::load_or_create(&key_path(app_data))?)
}

/// Replace the stored identity. The caller warns the user that the previous
/// key is gone.
pub fn import(app_data: &Path, secret_hex: &str) -> Result<Identity> {
    Ok(keyfile::import(&key_path(app_data), secret_hex)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("rootmode-id-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    #[test]
    fn generates_once_then_loads() {
        let d = dir();
        let a = load_or_create(&d).unwrap();
        let b = load_or_create(&d).unwrap();
        assert_eq!(a.peer_id(), b.peer_id());
    }

    #[test]
    fn import_replaces() {
        let d = dir();
        let original = load_or_create(&d).unwrap();
        let other = Identity::generate();
        let imported = import(&d, &other.export_secret_hex()).unwrap();
        assert_eq!(imported.peer_id(), other.peer_id());
        assert_ne!(imported.peer_id(), original.peer_id());
        assert_eq!(load_or_create(&d).unwrap().peer_id(), other.peer_id());
    }

    #[cfg(unix)]
    #[test]
    fn key_file_is_not_world_readable() {
        use std::os::unix::fs::PermissionsExt;
        let d = dir();
        load_or_create(&d).unwrap();
        let mode = std::fs::metadata(key_path(&d))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "key file must not be group/world accessible"
        );
    }
}
