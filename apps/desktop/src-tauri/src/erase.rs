//! Removing a generated image so that deleting it means something.
//!
//! # What this does, and what it cannot do
//!
//! Before unlinking, the file's bytes are overwritten with random data and
//! flushed to the device. On a spinning disk, or a filesystem that writes in
//! place, that genuinely destroys the old contents.
//!
//! On the hardware most people actually have it is **best effort, not a
//! guarantee**:
//!
//! * **SSDs** remap writes for wear levelling. The overwrite lands on
//!   different physical cells; the originals stay until the controller
//!   garbage-collects them, and nothing at this level can address them.
//! * **Copy-on-write filesystems** (APFS, btrfs, ZFS) write new blocks by
//!   design and free the old ones lazily.
//! * **Snapshots and backups** — APFS local snapshots, Time Machine, any sync
//!   client — may hold a copy this code will never see.
//!
//! This is why `srm` was removed from macOS and why Apple's own secure-erase
//! options are unavailable on SSDs: on modern storage the overwrite is a
//! gesture, not a proof. Shipping it while calling it "secure deletion" would
//! be worse than not shipping it, because someone would rely on it.
//!
//! The thing that does work on this hardware is encryption: full-disk
//! encryption so nothing is readable without the volume key, or per-file keys
//! destroyed on delete so the remaining ciphertext is meaningless. Until then,
//! this makes casual recovery — undelete tools, a file scavenger — fail, and
//! the UI says exactly that rather than promising more.

use std::io::{Seek, SeekFrom, Write};
use std::path::Path;

use crate::error::Result;

/// How many overwrite passes. One is the right number: multi-pass patterns are
/// folklore from 1990s drive encodings, and on anything modern the second pass
/// adds latency without adding certainty.
const PASSES: usize = 1;

/// Overwrite a file's contents, then remove it.
///
/// A missing file is success — the caller wanted it gone, and it is.
/// Overwrite failure is *not* fatal: a file we could not scribble on should
/// still be deleted, because leaving it in place is strictly worse.
pub fn remove(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }

    if let Err(e) = overwrite(path) {
        // Read-only media, a permissions problem, a file someone else holds
        // open. Say so at debug level and still unlink.
        log::debug!(
            "could not overwrite {} before deleting: {e}",
            path.display()
        );
    }

    std::fs::remove_file(path)?;
    Ok(())
}

fn overwrite(path: &Path) -> std::io::Result<()> {
    let len = std::fs::metadata(path)?.len();
    if len == 0 {
        return Ok(());
    }

    let mut file = std::fs::OpenOptions::new().write(true).open(path)?;

    // Random rather than zeroes: a block of zeroes is itself a signal that
    // something was deliberately erased here.
    let mut chunk = vec![0u8; 64 * 1024];
    for _ in 0..PASSES {
        file.seek(SeekFrom::Start(0))?;
        let mut written = 0u64;
        while written < len {
            fill_random(&mut chunk);
            let n = chunk.len().min((len - written) as usize);
            file.write_all(&chunk[..n])?;
            written += n as u64;
        }
        // Push it at the device before unlinking, or the write may never
        // leave the page cache.
        file.sync_data()?;
    }

    Ok(())
}

fn fill_random(buf: &mut [u8]) {
    use rand::RngCore;
    rand::thread_rng().fill_bytes(buf);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(contents: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("rootmode-erase-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("secret.png");
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn the_file_is_gone_afterwards() {
        let path = temp_file(b"something private");
        remove(&path).unwrap();
        assert!(!path.exists());
    }

    #[test]
    fn the_bytes_are_overwritten_before_the_unlink() {
        // Overwrite in place, then check the block through a second handle
        // opened before deletion — proving the scribble happened rather than
        // trusting that it did.
        let path = temp_file(b"a recognisable secret string");
        overwrite(&path).unwrap();

        let after = std::fs::read(&path).unwrap();
        assert_eq!(after.len(), "a recognisable secret string".len());
        assert_ne!(after, b"a recognisable secret string");

        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn deleting_something_already_gone_is_not_an_error() {
        // Two deletes racing, or a file a user removed by hand. The caller
        // wanted it gone; it is gone.
        let path = temp_file(b"x");
        remove(&path).unwrap();
        remove(&path).unwrap();
    }

    #[test]
    fn an_empty_file_is_handled_without_a_zero_length_write() {
        let path = temp_file(b"");
        remove(&path).unwrap();
        assert!(!path.exists());
    }
}
