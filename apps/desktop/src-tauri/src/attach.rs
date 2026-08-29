//! Reading a document the user dropped into the window.
//!
//! The path never comes from the frontend. The OS hands it to us when a file
//! is dropped, and that drop is the permission: the UI cannot name a file it
//! was not given, so this is not an arbitrary-file-read primitive wearing a
//! different hat.
//!
//! What comes back is plain text. The chat screen wraps it in a `<document>`
//! marker before putting it in the prompt — one place owns that format, so it
//! cannot drift from the code that reads it back out of a stored message.
//! Nothing in a document is ever executed or treated as instruction by this
//! app: the rule that applies to model output applies to input.

use std::path::{Path, PathBuf};

use serde::Serialize;
use sha2::{Digest, Sha256};

use crate::error::{AppError, Result};

/// How much of a document to carry. Roughly 100k characters — a long report,
/// well short of a context window, and small enough that a stray 200 MB log
/// file cannot wedge the app.
pub const MAX_CHARS: usize = 100_000;

/// Refuse to even read past this. Extraction allocates, and a malformed file
/// claiming to be a PDF should not be able to exhaust memory.
pub const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// A document ready to be put in front of a model.
#[derive(Debug, Clone, Serialize)]
pub struct Attachment {
    /// File name only. The full path is deliberately not sent to the frontend
    /// or into the prompt — where the file lives is nobody's business but
    /// this machine's.
    pub name: String,
    pub text: String,
    pub chars: usize,
    /// True when the document was longer than [`MAX_CHARS`] and was cut. Said
    /// out loud rather than silently, because a model answering from half a
    /// contract will not mention that it only saw half.
    pub truncated: bool,
    /// What it was read as: "text" or "pdf".
    pub kind: &'static str,
}

/// Read and extract one dropped file.
pub fn read(path: &Path) -> Result<Attachment> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "document".to_string());

    let meta = std::fs::metadata(path)
        .map_err(|e| AppError::Invalid(format!("cannot open {name}: {e}")))?;
    if meta.is_dir() {
        return Err(AppError::Invalid(format!(
            "{name} is a folder — drop the files inside it instead"
        )));
    }
    if meta.len() > MAX_BYTES {
        return Err(AppError::Invalid(format!(
            "{name} is {} MB, too big to read",
            meta.len() / (1024 * 1024)
        )));
    }

    let extension = path
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    let (raw, kind) = match extension.as_str() {
        "pdf" => (extract_pdf(path, &name)?, "pdf"),
        _ => (read_text(path, &name)?, "text"),
    };

    let cleaned = raw.trim();
    if cleaned.is_empty() {
        return Err(AppError::Invalid(format!(
            "{name} has no text in it — a scanned page or an image-only PDF has \
             nothing to read without OCR"
        )));
    }

    let chars = cleaned.chars().count();
    let truncated = chars > MAX_CHARS;
    let text = if truncated {
        cleaned.chars().take(MAX_CHARS).collect()
    } else {
        cleaned.to_string()
    };

    Ok(Attachment {
        name,
        chars: text.chars().count(),
        truncated,
        text,
        kind,
    })
}

/// Anything that is valid UTF-8 is a text document, whatever it is called.
///
/// Going by extension would refuse a `Dockerfile`, a `.env`, a `.rs`, and
/// every other file people actually want to ask about.
fn read_text(path: &Path, name: &str) -> Result<String> {
    let bytes =
        std::fs::read(path).map_err(|e| AppError::Invalid(format!("cannot read {name}: {e}")))?;

    String::from_utf8(bytes).map_err(|_| {
        AppError::Invalid(format!(
            "{name} is not text. Text and PDF can be read; images, audio and \
             office documents cannot"
        ))
    })
}

fn extract_pdf(path: &Path, name: &str) -> Result<String> {
    // Extraction on a malformed PDF can panic inside the parser. A bad file
    // someone dropped should be an error message, not a dead window.
    let path = path.to_path_buf();
    std::panic::catch_unwind(move || pdf_extract::extract_text(&path))
        .map_err(|_| AppError::Invalid(format!("{name} could not be read — the PDF is malformed")))?
        .map_err(|e| AppError::Invalid(format!("cannot read {name}: {e}")))
}

// ------------------------------------------------------------------ pictures

/// A picture is kept, not read: copied into the app's data under its own
/// hash, so a flow can point at it by id for as long as it likes and the
/// frontend still never names a path.
pub const PICTURES_DIR: &str = "pictures";

/// A photo, not a print run. Well above anything a picture model takes.
pub const MAX_PICTURE_BYTES: u64 = 25 * 1024 * 1024;

const PICTURE_TYPES: &[(&str, &str)] = &[
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
];

/// A dropped picture, kept under [`PICTURES_DIR`].
#[derive(Debug, Clone, Serialize)]
pub struct Picture {
    /// SHA-256 of the bytes, hex. The only handle the frontend gets.
    pub id: String,
    pub name: String,
    pub mime: String,
    pub bytes: u64,
}

fn picture_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_string_lossy().to_lowercase();
    PICTURE_TYPES.iter().find(|(e, _)| *e == ext).map(|(_, m)| *m)
}

fn is_picture(path: &Path) -> bool {
    picture_mime(path).is_some()
}

/// Copy a dropped picture into `dir` under its hash and describe it.
pub fn stash_picture(path: &Path, dir: &Path) -> Result<Picture> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "picture".to_string());
    let mime = picture_mime(path)
        .ok_or_else(|| AppError::Invalid(format!("{name} is not a picture format this app knows")))?;
    let meta = std::fs::metadata(path)
        .map_err(|e| AppError::Invalid(format!("cannot open {name}: {e}")))?;
    if meta.len() > MAX_PICTURE_BYTES {
        return Err(AppError::Invalid(format!(
            "{name} is {} MB, too big for a picture",
            meta.len() / (1024 * 1024)
        )));
    }
    let bytes =
        std::fs::read(path).map_err(|e| AppError::Invalid(format!("cannot read {name}: {e}")))?;
    if bytes.is_empty() {
        return Err(AppError::Invalid(format!("{name} is empty")));
    }
    let id = hex::encode(Sha256::digest(&bytes));
    std::fs::create_dir_all(dir)?;
    let ext = PICTURE_TYPES.iter().find(|(_, m)| *m == mime).map(|(e, _)| *e).unwrap_or("png");
    let kept = dir.join(format!("{id}.{ext}"));
    if !kept.exists() {
        std::fs::write(&kept, &bytes)?;
    }
    Ok(Picture {
        id,
        name,
        mime: mime.to_string(),
        bytes: bytes.len() as u64,
    })
}

/// Where a kept picture lives, by id. The id is checked to be a hash so
/// nothing but a file this app wrote can be named.
fn picture_path(dir: &Path, id: &str) -> Result<(PathBuf, &'static str)> {
    if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(AppError::Invalid("not a picture id".into()));
    }
    for (ext, mime) in PICTURE_TYPES {
        let p = dir.join(format!("{id}.{ext}"));
        if p.exists() {
            return Ok((p, mime));
        }
    }
    Err(AppError::NotFound(format!("picture {id}")))
}

/// The bytes of a kept picture and their type.
pub fn read_picture(dir: &Path, id: &str) -> Result<(Vec<u8>, &'static str)> {
    let (path, mime) = picture_path(dir, id)?;
    let bytes = std::fs::read(&path)
        .map_err(|e| AppError::Invalid(format!("cannot read {}: {e}", path.display())))?;
    Ok((bytes, mime))
}

/// Where on the window the files landed, in the page's own coordinates.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct DropPoint {
    pub x: f64,
    pub y: f64,
}

/// Everything the frontend needs to know about a drop.
#[derive(Debug, Clone, Serialize)]
pub struct DropOutcome {
    pub attached: Vec<Attachment>,
    /// Pictures, kept under [`PICTURES_DIR`] for a flow to point at.
    pub pictures: Vec<Picture>,
    /// One line per file that could not be read, already phrased for a person.
    pub rejected: Vec<String>,
    pub at: Option<DropPoint>,
}

/// Read every dropped path, keeping what worked and explaining what did not.
///
/// A failure on one file never discards the others: dropping five documents
/// and getting nothing because the third was a photo would be maddening.
/// Pictures are kept rather than read, in `pictures_dir` when there is one.
pub fn read_all(paths: &[PathBuf], pictures_dir: Option<&Path>, at: Option<DropPoint>) -> DropOutcome {
    let mut attached = Vec::new();
    let mut pictures = Vec::new();
    let mut rejected = Vec::new();
    for path in paths {
        if is_picture(path) {
            match pictures_dir {
                Some(dir) => match stash_picture(path, dir) {
                    Ok(p) => pictures.push(p),
                    Err(e) => rejected.push(e.to_string()),
                },
                None => rejected.push("pictures cannot be kept before the app has started".into()),
            }
            continue;
        }
        match read(path) {
            Ok(a) => attached.push(a),
            Err(e) => rejected.push(e.to_string()),
        }
    }
    DropOutcome { attached, pictures, rejected, at }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str, contents: &[u8]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("rootmode-attach-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, contents).unwrap();
        path
    }

    #[test]
    fn any_utf8_file_is_readable_whatever_it_is_called() {
        // Going by extension would refuse the files people most want to ask
        // about — a Dockerfile, a .env, a source file.
        for name in ["notes.md", "Dockerfile", "main.rs", "data.csv"] {
            let path = temp(name, b"hello there");
            let a = read(&path).unwrap();
            assert_eq!(a.text, "hello there");
            assert_eq!(a.kind, "text");
            assert_eq!(a.name, name);
        }
    }

    #[test]
    fn a_binary_file_is_refused_in_words_a_person_can_act_on() {
        let path = temp("photo.png", &[0x89, 0x50, 0x4e, 0x47, 0x00, 0xff, 0xfe]);
        let err = read(&path).unwrap_err().to_string();
        assert!(err.contains("not text"), "{err}");
        assert!(err.contains("images"), "{err}");
    }

    #[test]
    fn a_dropped_picture_is_kept_under_its_hash_and_read_back_by_id() {
        let png = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 1, 2, 3];
        let path = temp("cat.PNG", &png);
        let dir = path.parent().unwrap().join("pictures");
        let outcome = read_all(&[path.clone()], Some(&dir), Some(DropPoint { x: 10.0, y: 20.0 }));
        assert!(outcome.rejected.is_empty(), "{:?}", outcome.rejected);
        assert!(outcome.attached.is_empty(), "a picture is not a document");
        let p = &outcome.pictures[0];
        assert_eq!(p.name, "cat.PNG");
        assert_eq!(p.mime, "image/png");
        assert_eq!(p.bytes, png.len() as u64);
        assert_eq!(p.id.len(), 64);
        assert_eq!(outcome.at.unwrap().x, 10.0);

        let (bytes, mime) = read_picture(&dir, &p.id).unwrap();
        assert_eq!(bytes, png);
        assert_eq!(mime, "image/png");
        // Dropping it again keeps one copy.
        let again = read_all(&[path], Some(&dir), None);
        assert_eq!(again.pictures[0].id, p.id);
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
    }

    #[test]
    fn a_picture_id_is_a_hash_and_nothing_else() {
        let dir = std::env::temp_dir().join(format!("rootmode-pictures-{}", uuid::Uuid::new_v4()));
        for bad in ["../identity.key", "abc", &"z".repeat(64), "/etc/passwd"] {
            let err = read_picture(&dir, bad).unwrap_err().to_string();
            assert!(err.contains("not a picture id"), "{bad}: {err}");
        }
        let missing = "0".repeat(64);
        assert!(read_picture(&dir, &missing).unwrap_err().to_string().contains("not found"));
    }

    #[test]
    fn a_document_dropped_with_no_pictures_dir_still_reads() {
        let path = temp("notes.txt", b"hello");
        let outcome = read_all(&[path], None, None);
        assert_eq!(outcome.attached[0].text, "hello");
        assert!(outcome.pictures.is_empty());
    }

    #[test]
    fn an_empty_document_says_so_rather_than_attaching_nothing() {
        let path = temp("blank.txt", b"   \n\t  ");
        assert!(read(&path).unwrap_err().to_string().contains("no text"));
    }

    #[test]
    fn a_long_document_is_cut_and_admits_it() {
        // Silently truncating is the dangerous version: a model answering
        // from the first half of a contract will not mention the rest.
        let long = "x".repeat(MAX_CHARS + 500);
        let path = temp("long.txt", long.as_bytes());
        let a = read(&path).unwrap();

        assert!(a.truncated);
        assert_eq!(a.chars, MAX_CHARS);
    }

    #[test]
    fn a_folder_is_refused_with_advice() {
        let dir = std::env::temp_dir().join(format!("rootmode-dir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert!(read(&dir).unwrap_err().to_string().contains("folder"));
    }

    #[test]
    fn one_bad_file_does_not_lose_the_good_ones() {
        let good = temp("good.txt", b"keep me");
        let bad = temp("bad.bin", &[0xff, 0xfe, 0x00]);
        let outcome = read_all(&[good, bad], None, None);

        assert_eq!(outcome.attached.len(), 1);
        assert_eq!(outcome.attached[0].text, "keep me");
        assert_eq!(outcome.rejected.len(), 1);
    }
}
