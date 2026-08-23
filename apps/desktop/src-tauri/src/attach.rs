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

/// Everything the frontend needs to know about a drop.
#[derive(Debug, Clone, Serialize)]
pub struct DropOutcome {
    pub attached: Vec<Attachment>,
    /// One line per file that could not be read, already phrased for a person.
    pub rejected: Vec<String>,
}

/// Read every dropped path, keeping what worked and explaining what did not.
///
/// A failure on one file never discards the others: dropping five documents
/// and getting nothing because the third was a photo would be maddening.
pub fn read_all(paths: &[PathBuf]) -> DropOutcome {
    let mut attached = Vec::new();
    let mut rejected = Vec::new();
    for path in paths {
        match read(path) {
            Ok(a) => attached.push(a),
            Err(e) => rejected.push(e.to_string()),
        }
    }
    DropOutcome { attached, rejected }
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
        let outcome = read_all(&[good, bad]);

        assert_eq!(outcome.attached.len(), 1);
        assert_eq!(outcome.attached[0].text, "keep me");
        assert_eq!(outcome.rejected.len(), 1);
    }
}
