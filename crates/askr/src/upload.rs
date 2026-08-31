//! Streaming `multipart/form-data` handling.
//!
//! Uploads in worker mode used to be a hole: the raw body arrived but nothing
//! parsed it, so Laravel never saw `$_FILES`. And the whole body was buffered in
//! RAM. This module fixes both — it streams the multipart body, writing each file
//! part straight to a temp file (constant memory regardless of file size) and
//! collecting the non-file fields as POST parameters. The server hands PHP the
//! `$_FILES`-shaped metadata (name, type, tmp path, size); the worker builds a
//! Laravel `UploadedFile` from it.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

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

static UPLOAD_DIR: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Where streamed upload temp files go — resolved once, and *verified* rather than
/// assumed.
///
/// `$TMPDIR/askr-uploads` is a fixed, world-known path, and on a shared host
/// another local user can get there first. Every part of the old version then went
/// quiet about it: with `recursive(true)` an existing directory is not an error, so
/// `create()` returned Ok; `set_permissions()` on a directory owned by somebody else
/// fails with EPERM; and both results were discarded with `let _ =`. Uploads — which
/// hold whatever people type into forms — streamed into a directory another user
/// owned, could read, and could substitute entries of between our create and PHP's
/// read.
///
/// When the shared path isn't ours we move to a private one rather than fail. An
/// image that pre-creates `/tmp/askr-uploads` as root and then drops to www-data is
/// a legitimate setup, not an attack, and it should not take uploads down.
fn temp_dir() -> std::io::Result<&'static Path> {
    UPLOAD_DIR
        .get_or_init(resolve_temp_dir)
        .as_deref()
        .ok_or_else(|| {
            std::io::Error::other("no private directory available for upload temp files")
        })
}

fn resolve_temp_dir() -> Option<PathBuf> {
    let base = std::env::temp_dir();
    let shared = base.join("askr-uploads");
    if let Some(dir) = private_dir(&shared) {
        return Some(dir);
    }
    let mine = base.join(format!("askr-uploads-{}-{}", euid(), std::process::id()));
    let fallback = private_dir(&mine);
    if fallback.is_some() {
        tracing::warn!(
            shared = %shared.display(),
            using = %mine.display(),
            "upload temp dir is not a 0700 directory owned by this process — \
             using a private one instead"
        );
    } else {
        tracing::error!(
            shared = %shared.display(),
            "no usable upload temp directory; uploads will be refused"
        );
    }
    fallback
}

/// Create `dir` as 0700 if it is absent, then confirm it is a directory this
/// process owns which nobody else can enter. `None` if it cannot be made to
/// satisfy that — never a "close enough".
fn private_dir(dir: &Path) -> Option<PathBuf> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        let _ = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(dir);
        // lstat, not stat: a symlink aimed at somebody else's directory has to be
        // rejected here, not followed and then measured at the far end.
        let md = std::fs::symlink_metadata(dir).ok()?;
        if !md.is_dir() || md.uid() != euid() {
            return None;
        }
        if md.mode() & 0o077 != 0 {
            // It is ours, so tighten it — and then check, rather than assume the
            // chmod did what it said. That assumption is the original bug.
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).ok()?;
            if std::fs::symlink_metadata(dir).ok()?.mode() & 0o077 != 0 {
                return None;
            }
        }
        Some(dir.to_path_buf())
    }
    #[cfg(not(unix))]
    {
        std::fs::create_dir_all(dir).ok()?;
        Some(dir.to_path_buf())
    }
}

#[cfg(unix)]
fn euid() -> u32 {
    // SAFETY: geteuid() takes no arguments, cannot fail, and cannot be unsound.
    unsafe { libc::geteuid() }
}

#[cfg(not(unix))]
fn euid() -> u32 {
    0
}

/// Create one upload temp file, refusing to open anything that is already there or
/// that turns out to be a symlink.
///
/// `create_new` is `O_CREAT|O_EXCL`, so a name that already exists is an error
/// instead of a silent truncate of somebody else's file; `O_NOFOLLOW` says the same
/// thing about a symlink twice over. Names carry pid, nanoseconds and a counter, so
/// a collision is not expected — which is exactly why one should be loud.
async fn create_temp_file(path: &Path) -> std::io::Result<tokio::fs::File> {
    let mut opts = tokio::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        opts.mode(0o600).custom_flags(libc::O_NOFOLLOW);
    }
    opts.open(path).await
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
                let tmp = temp_dir()
                    .map_err(|e| UploadError::Parse(format!("temp file: {e}")))?
                    .join(unique_name());
                let mut out = create_temp_file(&tmp)
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

    /// `/tmp/askr-uploads` is a fixed path any local user can create first. Every
    /// result the old version needed was discarded: `create()` on an existing
    /// directory is Ok, and `set_permissions()` on somebody else's is EPERM. Uploads
    /// then streamed into a directory that was readable — and substitutable — by
    /// whoever owned it. So the directory is verified now, and "close enough" is not
    /// a verdict it can return.
    #[cfg(unix)]
    #[test]
    fn the_upload_dir_is_verified_and_not_merely_created() {
        use std::os::unix::fs::PermissionsExt;

        let base = std::env::temp_dir().join(format!("askr-dirtest-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();

        // A path that does not exist yet: created, and created private.
        let fresh = base.join("fresh");
        assert_eq!(private_dir(&fresh).as_deref(), Some(fresh.as_path()));
        let mode = std::fs::symlink_metadata(&fresh)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o700, "created 0700, not umask-dependent");

        // Ours but left open by an older version: tightened, then accepted.
        let loose = base.join("loose");
        std::fs::create_dir(&loose).unwrap();
        std::fs::set_permissions(&loose, std::fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(private_dir(&loose).as_deref(), Some(loose.as_path()));
        let mode = std::fs::symlink_metadata(&loose)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o077, 0, "no access for anybody else");

        // A symlink is refused rather than followed and measured at the far end —
        // the target may be a directory somebody else owns.
        let target = base.join("elsewhere");
        std::fs::create_dir(&target).unwrap();
        let link = base.join("link");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(
            private_dir(&link).is_none(),
            "a symlink is not a directory we made"
        );

        // And a plain file sitting on the path is not a directory either.
        let file = base.join("file");
        std::fs::write(&file, b"").unwrap();
        assert!(private_dir(&file).is_none());

        let _ = std::fs::remove_dir_all(&base);
    }

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
