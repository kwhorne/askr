//! Streaming `multipart/form-data` handling.
//!
//! Uploads in worker mode used to be a hole: the raw body arrived but nothing
//! parsed it, so Laravel never saw `$_FILES`. And the whole body was buffered in
//! RAM. This module fixes both — it streams the multipart body, writing each file
//! part straight to a temp file (constant memory regardless of file size) and
//! collecting the non-file fields as POST parameters. The server hands PHP the
//! `$_FILES`-shaped metadata (name, type, tmp path, size); the worker builds a
//! Laravel `UploadedFile` from it.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use bytes::Bytes;
use futures_core::Stream;
use tokio::io::AsyncWriteExt;

use askr_php::UploadedFile;

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The result of parsing a multipart body.
pub struct Parsed {
    /// Non-file form fields → POST parameters.
    pub fields: Vec<(String, String)>,
    /// Uploaded files, streamed to temp paths.
    pub files: Vec<UploadedFile>,
    /// Temp files, unlinked when this guard drops (see [`TempFiles`]).
    pub temp_paths: TempFiles,
}

/// Owns the on-disk temp paths for an upload and unlinks them on drop, so a
/// failed parse (partial upload) *or* a client that disconnects while PHP is
/// running never leaks files under `/tmp/askr-uploads` — the guard is dropped
/// whether the request completes, errors, or its future is cancelled mid-await.
/// `move_uploaded_file()` may have already renamed a path away; a missing file
/// is fine (the unlink is best-effort).
#[derive(Default)]
pub struct TempFiles {
    paths: Vec<PathBuf>,
}

impl TempFiles {
    fn push(&mut self, p: PathBuf) {
        self.paths.push(p);
    }
}

impl Drop for TempFiles {
    fn drop(&mut self) {
        for p in &self.paths {
            let _ = std::fs::remove_file(p);
        }
    }
}

pub enum UploadError {
    /// The stream (or a field) exceeded the configured size limit → 413.
    TooLarge,
    /// Malformed multipart body → 400.
    Parse(String),
}

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join("askr-uploads");
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn unique_name() -> String {
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("askr-{}-{}-{}.upload", std::process::id(), nanos, n)
}

/// Parse a multipart body from a byte stream. Files go to temp paths (bounded by
/// `max_total`); non-file fields are collected in memory (also bounded).
pub async fn parse<S, E>(stream: S, boundary: &str, max_total: usize) -> Result<Parsed, UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
    let constraints = multer::Constraints::new()
        .size_limit(multer::SizeLimit::new().whole_stream(max_total as u64));
    let mut mp = multer::Multipart::with_constraints(stream, boundary.to_string(), constraints);

    let mut fields = Vec::new();
    let mut files = Vec::new();
    let mut temp_paths = TempFiles::default();

    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(map_err(e)),
        };
        let name = field.name().unwrap_or("").to_string();
        let file_name = field.file_name().map(|s| s.to_string());
        let content_type = field
            .content_type()
            .map(|m| m.to_string())
            .unwrap_or_else(|| "application/octet-stream".to_string());

        match file_name {
            // Empty filename ⇒ the browser submitted a file input with nothing
            // chosen. PHP surfaces this in $_FILES with UPLOAD_ERR_NO_FILE (4) and
            // no temp file, so Laravel's `$request->hasFile()` returns false. Match
            // that instead of fabricating a 0-byte upload with error=OK.
            Some(ref fname) if fname.is_empty() => {
                files.push(UploadedFile {
                    field_name: name,
                    file_name: String::new(),
                    content_type,
                    tmp_path: String::new(),
                    size: 0,
                    error: 4, // UPLOAD_ERR_NO_FILE
                });
            }
            // A file part → stream to a temp file.
            Some(file_name) => {
                let tmp = temp_dir().join(unique_name());
                let mut out = tokio::fs::File::create(&tmp)
                    .await
                    .map_err(|e| UploadError::Parse(format!("temp file: {e}")))?;
                temp_paths.push(tmp.clone());
                let mut size = 0usize;
                let mut field = field;
                loop {
                    match field.chunk().await {
                        Ok(Some(chunk)) => {
                            size += chunk.len();
                            if out.write_all(&chunk).await.is_err() {
                                return Err(UploadError::Parse("temp write failed".into()));
                            }
                        }
                        Ok(None) => break,
                        Err(e) => return Err(map_err(e)),
                    }
                }
                let _ = out.flush().await;
                files.push(UploadedFile {
                    field_name: name,
                    file_name,
                    content_type,
                    tmp_path: tmp.to_string_lossy().into_owned(),
                    size,
                    error: 0, // UPLOAD_ERR_OK
                });
            }
            // A normal field → collect its text value.
            None => {
                let bytes = field.bytes().await.map_err(map_err)?;
                fields.push((name, String::from_utf8_lossy(&bytes).into_owned()));
            }
        }
    }

    Ok(Parsed {
        fields,
        files,
        temp_paths,
    })
}

fn map_err(e: multer::Error) -> UploadError {
    match e {
        multer::Error::StreamSizeExceeded { .. } | multer::Error::FieldSizeExceeded { .. } => {
            UploadError::TooLarge
        }
        other => UploadError::Parse(other.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempfiles_unlink_on_drop() {
        let p = std::env::temp_dir().join(format!("askr-droptest-{}.tmp", std::process::id()));
        std::fs::write(&p, b"x").unwrap();
        assert!(p.exists());
        {
            let mut t = TempFiles::default();
            t.push(p.clone());
        } // guard drops here
        assert!(!p.exists(), "temp file should be unlinked on drop");
    }
}
