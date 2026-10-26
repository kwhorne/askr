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

use askr_php::{UploadLimits, UploadWarning, UploadedFile};

static SEQ: AtomicU64 = AtomicU64::new(0);

/// The result of parsing a multipart body.
pub struct Parsed {
    /// Non-file form fields → POST parameters.
    pub fields: Vec<(String, String)>,
    /// Uploaded files, streamed to temp paths.
    pub files: Vec<UploadedFile>,
    /// Temp files, unlinked when this guard drops (see [`TempFiles`]).
    pub temp_paths: TempFiles,
    /// What PHP's parser would have warned about the body, in its order.
    pub warnings: Vec<UploadWarning>,
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
    /// The body could not be read, or an upload could not be stored → 400.
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

static LIMITS: OnceLock<UploadLimits> = OnceLock::new();

/// Record PHP's upload settings for this process. Called once by whatever started the
/// engine, right after it started — the worker's PHP thread, or the CoW template before it
/// forks (its children inherit the value).
pub fn set_limits(limits: UploadLimits) {
    let _ = LIMITS.set(limits);
}

/// PHP's upload settings for this process: the engine's, or PHP's defaults if no engine
/// reported them.
pub fn limits() -> UploadLimits {
    LIMITS.get().copied().unwrap_or(UploadLimits::PHP_DEFAULTS)
}

/// Whether a request's Content-Type is one PHP reads as a multipart form: its media type,
/// up to the first `;`, `,` or space, in any case (`sapi_read_post_data` in `main/SAPI.c`).
pub fn is_multipart(content_type: &str) -> bool {
    let end = content_type
        .find([';', ',', ' '])
        .unwrap_or(content_type.len());
    content_type[..end].eq_ignore_ascii_case("multipart/form-data")
}

/// Parse a multipart body from a byte stream. Files go to temp paths; fields are collected
/// in memory. `max_total` is Askr's `max_body_size`, refused with a 413 above it.
///
/// PHP's upload settings are applied as PHP's own parser (`main/rfc1867.c`) applies them,
/// so a worker sees what `$_FILES` and `$_POST` would say:
///
/// - **`post_max_size`**: a body over it leaves no fields and no files at all.
/// - **`file_uploads = 0`**: file parts are dropped; fields still arrive.
/// - **`max_file_uploads`**: file parts past it are dropped (an empty file input does not
///   count, as in PHP).
/// - **`upload_max_filesize`**, then a **`MAX_FILE_SIZE`** field sent before the file: a
///   file over either is kept as an entry with `UPLOAD_ERR_INI_SIZE` (1) or
///   `UPLOAD_ERR_FORM_SIZE` (2), and no temp file, size or type.
/// - **`max_input_vars`** and **`max_multipart_body_parts`**: the fields past the one, and
///   the parts past the other, are dropped.
///
/// Until 1.9.0 none of these applied in worker mode: any file up to `max_body_size` was
/// accepted, whatever the app's php.ini said, and the limits an app configured — and
/// Laravel reports in its validation messages — were not the ones that held.
///
/// A body that ends before its closing boundary is what PHP makes of it too: a file it ends
/// in is `UPLOAD_ERR_PARTIAL` (3), a field it ends in keeps what arrived, and the parts
/// before it are kept. A body that stops making sense part-way leaves the parts before that
/// point. PHP's parser never refuses a body, and until 1.9.3 both were a 400 here, which the
/// app never saw.
///
/// The warnings PHP's parser raises on the way — a body over `post_max_size`, a missing or
/// broken boundary, a part without a name, a limit reached — come back in
/// [`Parsed::warnings`], for the worker to raise as PHP raises them. Until 1.9.4 they were
/// lines in Askr's log, or nothing.
pub async fn parse<S, E>(
    stream: S,
    content_type: &str,
    max_total: usize,
    content_length: Option<u64>,
    limits: &UploadLimits,
) -> Result<Parsed, UploadError>
where
    S: Stream<Item = Result<Bytes, E>> + Send + Unpin + 'static,
    E: Into<Box<dyn std::error::Error + Send + Sync>> + 'static,
{
    let max_total = max_total as u64;
    if content_length.is_some_and(|n| n > max_total) {
        return Err(UploadError::TooLarge);
    }
    let post_max = u64::try_from(limits.post_max_size)
        .ok()
        .filter(|&n| n > 0 && n < max_total);
    if let (Some(len), Some(post_max)) = (content_length, post_max) {
        if len > post_max {
            return Ok(over_post_max(len, post_max));
        }
    }
    // A body without a Content-Length is measured as it streams, against the lower limit.
    let mut body = Ended {
        body: Some(stream),
        end: None,
        read: 0,
        max: max_total,
        post_max,
    };
    let boundary = match boundary(content_type) {
        Ok(b) => b,
        Err(warning) => {
            // As PHP checks post_max_size before the boundary, a body measured as it streams
            // is measured first.
            if content_length.is_none() && post_max.is_some() {
                if let Err(e) = std::future::poll_fn(|cx| body.drain(cx)).await {
                    return match failure(multer::Error::StreamReadFailed(e)) {
                        Failure::OverPostMax(len, post_max) => Ok(over_post_max(len, post_max)),
                        Failure::Refused(e) => Err(e),
                        Failure::Malformed(_) => unreachable!("a read error is not malformed"),
                    };
                }
            }
            return Ok(Parsed {
                warnings: vec![warning],
                ..Parsed::empty()
            });
        }
    };
    let end_name = format!("askr-body-end-{}", unique_name());
    body.end = Some(Bytes::from(format!(
        "\r\n--{boundary}\r\nContent-Disposition: form-data; name=\"{end_name}\"\r\n\r\n"
    )));
    let mut mp = multer::Multipart::new(body, boundary.to_string());

    let mut fields: Vec<(String, String)> = Vec::new();
    let mut files: Vec<UploadedFile> = Vec::new();
    let mut temp_paths = TempFiles::default();
    let mut warnings: Vec<UploadWarning> = Vec::new();
    // As PHP counts: down from max_file_uploads, one per file part that gets a temp file.
    let mut uploads_left = limits.max_file_uploads;
    let upload_max = limits.upload_max_filesize;
    let mut form_max: i64 = 0;
    // And down from max_multipart_body_parts, one per part with a Content-Disposition.
    let parts_limit = if limits.max_multipart_body_parts < 0 {
        limits
            .max_input_vars
            .saturating_add(limits.max_file_uploads)
    } else {
        limits.max_multipart_body_parts
    };
    let mut parts_left = parts_limit;
    let mut skip_files = false;
    let mut anonymous: u32 = 0;
    // The part the body may have ended in: the last one read, if it is kept as it arrived.
    let mut last: Option<Last> = None;
    let stop = |e: multer::Error| match failure(e) {
        Failure::OverPostMax(len, post_max) => Err(Ok(over_post_max(len, post_max))),
        Failure::Refused(e) => Err(Err(e)),
        Failure::Malformed(e) => Ok(e),
    };
    // A field, as PHP adds one: no more than max_input_vars of them, and a warning at the
    // first past it.
    let add_field = |fields: &mut Vec<(String, String)>,
                     warnings: &mut Vec<UploadWarning>,
                     count: &mut i64,
                     field: (String, String)| {
        *count += 1;
        if *count <= limits.max_input_vars {
            fields.push(field);
            return true;
        }
        if *count == limits.max_input_vars + 1 {
            warnings.push(startup(
                format!(
                    "Input variables exceeded {}. To increase the limit change max_input_vars \
                     in php.ini.",
                    limits.max_input_vars
                ),
                fields.len(),
            ));
        }
        false
    };
    let mut count = 0i64;

    'parts: loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => match stop(e) {
                Ok(e) => {
                    malformed(&e);
                    break;
                }
                Err(r) => return r,
            },
        };
        if field.name() == Some(end_name.as_str()) {
            // The body ended inside the last part (see [`Ended`]).
            match last {
                Some(Last::File(i)) => {
                    let f = &mut files[i];
                    let _ = tokio::fs::remove_file(&f.tmp_path).await;
                    cut_short(&f.field_name);
                    *f = UploadedFile {
                        field_name: std::mem::take(&mut f.field_name),
                        file_name: std::mem::take(&mut f.file_name),
                        full_path: std::mem::take(&mut f.full_path),
                        error: 3, // UPLOAD_ERR_PARTIAL
                        ..UploadedFile::default()
                    };
                }
                Some(Last::Field(i, ref bytes)) => {
                    cut_short(&fields[i].0);
                    fields[i].1 =
                        String::from_utf8_lossy(rest_of_field(bytes, boundary)).into_owned();
                }
                None => {}
            }
            break;
        }
        last = None;
        // A part without a Content-Disposition is passed over, as PHP passes over it.
        if !field
            .headers()
            .contains_key(hyper::header::CONTENT_DISPOSITION)
        {
            continue;
        }
        parts_left -= 1;
        if parts_left < 0 {
            warnings.push(startup(
                format!(
                    "Multipart body parts limit exceeded {}. To increase the limit change \
                     max_multipart_body_parts in php.ini.",
                    parts_limit as i32
                ),
                fields.len(),
            ));
            break;
        }
        let name = field.name().map(str::to_string);
        let file_name = field.file_name().map(str::to_string);
        if let (Some(name), None) = (&name, &file_name) {
            // A normal field → collect its text value. A MAX_FILE_SIZE field limits the
            // files that come after it, as in PHP (strtoll of its value; 0 is no limit).
            let name = name.clone();
            let bytes = match field.bytes().await {
                Ok(b) => b,
                Err(e) => match stop(e) {
                    Ok(e) => {
                        // The body ended before this field's data began: PHP keeps the
                        // field, empty.
                        if matches!(e, multer::Error::IncompleteFieldData { .. }) {
                            cut_short(&name);
                            add_field(
                                &mut fields,
                                &mut warnings,
                                &mut count,
                                (name, String::new()),
                            );
                        } else {
                            malformed(&e);
                        }
                        break;
                    }
                    Err(r) => return r,
                },
            };
            let value = String::from_utf8_lossy(&bytes).into_owned();
            if name.eq_ignore_ascii_case("MAX_FILE_SIZE") {
                form_max = strtoll(&value);
            }
            if add_field(&mut fields, &mut warnings, &mut count, (name, value)) {
                last = Some(Last::Field(fields.len() - 1, bytes));
            }
            continue;
        }
        // A file part, or a part with neither a name nor a filename, which PHP takes for one
        // as far as its upload count. Once PHP passes over a file part it passes over every
        // one after it: its flag for that is never cleared (`skip_upload` in rfc1867.c).
        if !limits.file_uploads {
            skip_files = true;
        } else if uploads_left <= 0 {
            skip_files = true;
            if uploads_left == 0 {
                uploads_left -= 1;
                warnings.push(startup(
                    "Maximum number of allowable file uploads has been exceeded".into(),
                    fields.len(),
                ));
            }
        }
        let Some(file_name) = file_name else {
            warnings.push(startup(
                "File Upload Mime headers garbled".into(),
                fields.len(),
            ));
            break;
        };
        // A file without a name is listed under a number, counted in the request.
        let name = name.unwrap_or_else(|| {
            anonymous += 1;
            (anonymous - 1).to_string()
        });
        if !skip_files && !php_keeps_file_name(&name) {
            tracing::warn!(
                field = %name,
                "upload: a file field whose name PHP will not repair; it and every file \
                 after it are dropped, as PHP drops them"
            );
            skip_files = true;
        }
        if skip_files {
            continue;
        }
        // PHP's type: the part's Content-Type as sent, up to any ';' — or nothing.
        let content_type = field
            .headers()
            .get(hyper::header::CONTENT_TYPE)
            .map(|v| String::from_utf8_lossy(v.as_bytes()).into_owned())
            .map(|t| t.split(';').next().unwrap_or("").to_string())
            .unwrap_or_default();
        let cancelled = |error: i32| UploadedFile {
            field_name: name.clone(),
            file_name: basename(&file_name).to_string(),
            full_path: file_name.clone(),
            error,
            ..UploadedFile::default()
        };

        // Empty filename ⇒ the browser submitted a file input with nothing chosen. PHP
        // surfaces this in $_FILES with UPLOAD_ERR_NO_FILE (4) and no temp file, so
        // Laravel's `$request->hasFile()` returns false. Match that instead of
        // fabricating a 0-byte upload with error=OK.
        if file_name.is_empty() {
            files.push(cancelled(4));
            continue;
        }

        // A file part → stream to a temp file.
        uploads_left -= 1;
        let tmp = temp_dir()
            .map_err(|e| UploadError::Parse(format!("temp file: {e}")))?
            .join(unique_name());
        let mut out = create_temp_file(&tmp)
            .await
            .map_err(|e| UploadError::Parse(format!("temp file: {e}")))?;
        temp_paths.push(tmp.clone());
        let mut size = 0u64;
        let mut error = 0; // UPLOAD_ERR_OK
        let mut field = field;
        loop {
            match field.chunk().await {
                Ok(Some(chunk)) => {
                    let total = size + chunk.len() as u64;
                    if upload_max > 0 && total > upload_max as u64 {
                        error = 1; // UPLOAD_ERR_INI_SIZE
                        tracing::warn!(
                            field = %name,
                            upload_max_filesize = upload_max,
                            "upload: a file over upload_max_filesize; the app sees \
                             UPLOAD_ERR_INI_SIZE (raise it with `[worker] ini`)"
                        );
                        break;
                    }
                    if form_max != 0 && total as i128 > form_max as i128 {
                        error = 2; // UPLOAD_ERR_FORM_SIZE
                        break;
                    }
                    if out.write_all(&chunk).await.is_err() {
                        return Err(UploadError::Parse("temp write failed".into()));
                    }
                    size = total;
                }
                Ok(None) => break,
                Err(e) => match stop(e) {
                    Ok(e) => {
                        drop(out);
                        let _ = tokio::fs::remove_file(&tmp).await;
                        if matches!(e, multer::Error::IncompleteFieldData { .. }) {
                            // The body ended before this file's data began.
                            cut_short(&name);
                            files.push(cancelled(3)); // UPLOAD_ERR_PARTIAL
                        } else {
                            malformed(&e);
                        }
                        break 'parts;
                    }
                    Err(r) => return r,
                },
            }
        }
        let _ = out.flush().await;
        if error != 0 {
            // Not kept, as PHP keeps nothing of it; the rest of the part is skipped.
            drop(out);
            let _ = tokio::fs::remove_file(&tmp).await;
            files.push(cancelled(error));
            continue;
        }
        files.push(UploadedFile {
            field_name: name,
            file_name: basename(&file_name).to_string(),
            full_path: file_name,
            content_type,
            tmp_path: tmp.to_string_lossy().into_owned(),
            size: size as usize,
            error,
        });
        last = Some(Last::File(files.len() - 1));
    }

    Ok(Parsed {
        fields,
        files,
        temp_paths,
        warnings,
    })
}

impl Parsed {
    fn empty() -> Parsed {
        Parsed {
            fields: Vec::new(),
            files: Vec::new(),
            temp_paths: TempFiles::default(),
            warnings: Vec::new(),
        }
    }
}

/// Whether PHP keeps a file field's name as it is (rfc1867.c: "never repair potential
/// malicious user input"): every `[` closed by a `]`, never a `]` before its `[`, and
/// nothing after a `]` but another `[`. A file under any other name is not PHP's to list.
fn php_keeps_file_name(name: &str) -> bool {
    let b = name.as_bytes();
    let mut depth = 0i64;
    for (i, &c) in b.iter().enumerate() {
        if c == b'[' {
            depth += 1;
        } else if c == b']' {
            depth -= 1;
            if b.get(i + 1).is_some_and(|&next| next != b'[') {
                return false;
            }
        }
        if depth < 0 {
            return false;
        }
    }
    depth == 0
}

/// A warning as PHP's parser raises most: "PHP Request Startup: " before it.
fn startup(message: String, before_field: usize) -> UploadWarning {
    UploadWarning {
        message,
        startup: true,
        before_field,
    }
}

/// The boundary in a multipart Content-Type, found as PHP finds it (`rfc1867_post_handler`
/// in `main/rfc1867.c`): after the first `boundary` (in any case, if not in lower case) and
/// the `=` after that; up to the closing quote if it is quoted, else up to the first `,` or
/// `;`, spaces and all. PHP's warning if there is none, or it is unusable.
fn boundary(content_type: &str) -> Result<&str, UploadWarning> {
    let at = content_type
        .find("boundary")
        .or_else(|| content_type.to_ascii_lowercase().find("boundary"));
    let Some(value) = at.and_then(|at| {
        let rest = &content_type[at..];
        rest.find('=').map(|eq| &rest[eq + 1..])
    }) else {
        return Err(startup(
            "Missing boundary in multipart/form-data POST data".into(),
            0,
        ));
    };
    let value = match value.strip_prefix('"') {
        Some(quoted) => match quoted.find('"') {
            Some(end) => &quoted[..end],
            None => {
                return Err(startup(
                    "Invalid boundary in multipart/form-data POST data".into(),
                    0,
                ))
            }
        },
        None => &value[..value.find([',', ';']).unwrap_or(value.len())],
    };
    // FILLUNIT less "\r\n--": PHP's buffer could not hold a longer one.
    if value.len() > 5 * 1024 - 4 {
        return Err(UploadWarning {
            message: "Boundary too large in multipart/form-data POST data".into(),
            startup: false,
            before_field: 0,
        });
    }
    Ok(value)
}

/// The body as it arrives, then — once it has ended without error — the start of one more
/// part, named for this request alone.
///
/// A body that ends before its closing boundary leaves the parser inside a part, where it
/// can only fail and say nothing of what arrived. With this tail the part the body ended in
/// ends as a part does, and the next part to arrive is this one: the sign that the body
/// ended inside the last. A body that does end with its closing boundary is never read as
/// far as this tail. One that ends between parts, or in a part's headers, still fails, as
/// the tail is no header and no part data there.
///
/// It also measures the body, which a measure inside the parser would count the tail in:
/// over `max` it is refused; over `post_max` it is read to its end, or to `max`, unparsed,
/// so its length can be told as PHP tells it — Askr's per-request mode hands PHP a body
/// without a Content-Length with its length as one.
struct Ended<S> {
    body: Option<S>,
    end: Option<Bytes>,
    read: u64,
    max: u64,
    post_max: Option<u64>,
}

type BoxError = Box<dyn std::error::Error + Send + Sync>;

/// A body over one of [`Ended`]'s limits.
#[derive(Debug)]
enum OverLimit {
    /// Over `max_body_size`.
    Body,
    /// Over `post_max_size`, with its whole length and the limit.
    PostMax(u64, u64),
}

impl std::fmt::Display for OverLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OverLimit::Body => f.write_str("body over max_body_size"),
            OverLimit::PostMax(..) => f.write_str("body over post_max_size"),
        }
    }
}

impl std::error::Error for OverLimit {}

impl<S, E> Ended<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<BoxError>,
{
    /// The next chunk of the body: `None` at its end, an error over a limit.
    fn poll_body(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Result<Bytes, BoxError>>> {
        use std::task::Poll;
        loop {
            let Some(body) = self.body.as_mut() else {
                return Poll::Ready(None);
            };
            let fail = |this: &mut Self, e: BoxError| {
                this.body = None;
                this.end = None;
                Poll::Ready(Some(Err(e)))
            };
            match std::pin::Pin::new(body).poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(chunk))) => {
                    self.read += chunk.len() as u64;
                    if self.read > self.max {
                        return fail(self, Box::new(OverLimit::Body));
                    }
                    if self.post_max.is_some_and(|m| self.read > m) {
                        continue; // read on, to the end, unparsed
                    }
                    return Poll::Ready(Some(Ok(chunk)));
                }
                Poll::Ready(Some(Err(e))) => return fail(self, e.into()),
                Poll::Ready(None) => {
                    self.body = None;
                    if let Some(post_max) = self.post_max.filter(|&m| self.read > m) {
                        self.end = None;
                        return Poll::Ready(Some(Err(Box::new(OverLimit::PostMax(
                            self.read, post_max,
                        )))));
                    }
                    return Poll::Ready(None);
                }
            }
        }
    }

    /// Read the body to its end, unparsed.
    fn drain(&mut self, cx: &mut std::task::Context<'_>) -> std::task::Poll<Result<(), BoxError>> {
        use std::task::Poll;
        loop {
            match self.poll_body(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(Some(Ok(_))) => {}
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Err(e)),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
            }
        }
    }
}

impl<S, E> Stream for Ended<S>
where
    S: Stream<Item = Result<Bytes, E>> + Unpin,
    E: Into<BoxError>,
{
    type Item = Result<Bytes, BoxError>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = &mut *self;
        match this.poll_body(cx) {
            Poll::Ready(None) => Poll::Ready(this.end.take().map(Ok)),
            other => other,
        }
    }
}

/// The last part read, kept as it arrived: the body may have ended inside it.
enum Last {
    /// An index into the fields, and the field's bytes as they arrived.
    Field(usize, Bytes),
    /// An index into the files.
    File(usize),
}

/// What an error from the parser means for the request.
enum Failure {
    /// The body is over `post_max_size` (its length, the limit): PHP's empty `$_POST` and
    /// `$_FILES`.
    OverPostMax(u64, u64),
    /// Over `max_body_size`, or the body could not be read: the app does not see it.
    Refused(UploadError),
    /// The body stops making sense here: the app gets the parts before, as PHP's parser,
    /// which never refuses a body, would give it at least those.
    Malformed(multer::Error),
}

fn failure(e: multer::Error) -> Failure {
    match e {
        multer::Error::StreamReadFailed(e) => match e.downcast_ref::<OverLimit>() {
            Some(OverLimit::PostMax(len, post_max)) => Failure::OverPostMax(*len, *post_max),
            Some(OverLimit::Body) => Failure::Refused(UploadError::TooLarge),
            None => Failure::Refused(UploadError::Parse(format!("failed to read the body: {e}"))),
        },
        e => Failure::Malformed(e),
    }
}

fn malformed(e: &multer::Error) {
    tracing::warn!(
        error = %e,
        "upload: a multipart body that stops making sense part-way; the app sees the parts \
         before that point"
    );
}

fn cut_short(field: &str) {
    tracing::warn!(
        field,
        "upload: a multipart body that ends inside a part; the app sees what PHP shows of \
         it (a file is UPLOAD_ERR_PARTIAL)"
    );
}

/// What PHP keeps of a field the body ended in (`multipart_buffer_read` in
/// `main/rfc1867.c`): what arrived, short of a tail that could still have become the next
/// boundary (`\n--` and the boundary), and of a `\r` before that tail.
fn rest_of_field<'a>(value: &'a [u8], boundary: &str) -> &'a [u8] {
    let next = format!("\n--{boundary}");
    let next = next.as_bytes();
    let from = value.len().saturating_sub(next.len());
    match (from..value.len()).find(|&i| next.starts_with(&value[i..])) {
        Some(i) => value[..i].strip_suffix(b"\r").unwrap_or(&value[..i]),
        None => value,
    }
}

/// What PHP makes of a body over `post_max_size`: no fields and no files, and a warning.
fn over_post_max(len: u64, post_max: u64) -> Parsed {
    Parsed {
        warnings: vec![startup(
            format!("POST Content-Length of {len} bytes exceeds the limit of {post_max} bytes"),
            0,
        )],
        ..Parsed::empty()
    }
}

/// C's `strtoll(s, NULL, 10)`, which PHP applies to a `MAX_FILE_SIZE` field: leading
/// whitespace, a sign, then digits for as long as they last; 0 if there are none, and
/// clamped at the ends of `i64` rather than overflowing.
fn strtoll(s: &str) -> i64 {
    let s = s.trim_start_matches([' ', '\t', '\n', '\x0b', '\x0c', '\r']);
    let (neg, digits) = match s.as_bytes().first() {
        Some(b'-') => (true, &s[1..]),
        Some(b'+') => (false, &s[1..]),
        _ => (false, s),
    };
    let mut n: i64 = 0;
    for b in digits.bytes().take_while(u8::is_ascii_digit) {
        let d = (b - b'0') as i64;
        n = match n.checked_mul(10).and_then(|n| {
            if neg {
                n.checked_sub(d)
            } else {
                n.checked_add(d)
            }
        }) {
            Some(n) => n,
            None => return if neg { i64::MIN } else { i64::MAX },
        };
    }
    n
}

/// A client's filename without its directory part, by PHP's rule (`php_ap_basename` in
/// `main/rfc1867.c`): what follows the last `/` or `\`, whichever comes later — `\`
/// too, on every platform, because browsers on Windows have sent `C:\Users\…`.
///
/// Until 1.8.6 worker mode handed the filename over as sent, so `$r['files'][..]['name']`
/// could be `../../public/shell.php`, or a folder upload's `photos/2024/a.jpg`, where
/// PHP's `$_FILES` has `shell.php` and `a.jpg`. The path the client sent is still there,
/// as `full_path`, as in PHP.
fn basename(file_name: &str) -> &str {
    match file_name.rfind(['/', '\\']) {
        Some(i) => &file_name[i + 1..],
        None => file_name,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A body as a stream, in chunks of `chunk` bytes, as a slow client sends it.
    struct Body(std::collections::VecDeque<Bytes>);

    impl Stream for Body {
        type Item = Result<Bytes, std::io::Error>;
        fn poll_next(
            mut self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Option<Self::Item>> {
            std::task::Poll::Ready(self.0.pop_front().map(Ok))
        }
    }

    const B: &str = "----askr-test";
    const CT: &str = "multipart/form-data; boundary=----askr-test";

    /// A multipart body: `(name, filename, content-type, content)`, a field if no filename.
    fn body(parts: &[(&str, Option<&str>, Option<&str>, &str)]) -> String {
        let mut s = String::new();
        for (name, file, ty, content) in parts {
            s += &format!("--{B}\r\nContent-Disposition: form-data; name=\"{name}\"");
            if let Some(f) = file {
                s += &format!("; filename=\"{f}\"");
            }
            if let Some(t) = ty {
                s += &format!("\r\nContent-Type: {t}");
            }
            s += &format!("\r\n\r\n{content}\r\n");
        }
        s + &format!("--{B}--\r\n")
    }

    async fn run(
        body: &str,
        chunk: usize,
        content_length: bool,
        limits: UploadLimits,
    ) -> Result<Parsed, UploadError> {
        let chunks = body
            .as_bytes()
            .chunks(chunk)
            .map(Bytes::copy_from_slice)
            .collect();
        let len = content_length.then_some(body.len() as u64);
        parse(Body(chunks), CT, 1 << 20, len, &limits).await
    }

    fn ok(r: Result<Parsed, UploadError>) -> Parsed {
        match r {
            Ok(p) => p,
            Err(UploadError::TooLarge) => panic!("413"),
            Err(UploadError::Parse(e)) => panic!("400: {e}"),
        }
    }

    const LIMITS: UploadLimits = UploadLimits::PHP_DEFAULTS;

    /// `upload_max_filesize`: a file over it is an entry with UPLOAD_ERR_INI_SIZE and
    /// nothing else — no temp file, size or type — and the files around it are kept.
    #[tokio::test]
    async fn a_file_over_upload_max_filesize_is_ini_size_and_kept_nowhere() {
        let b = body(&[
            ("a", Some("a.txt"), Some("text/plain"), "1"),
            (
                "big",
                Some("big.bin"),
                Some("application/x-big"),
                "0123456789",
            ),
            ("c", Some("c.txt"), Some("text/plain"), "333"),
        ]);
        let limits = UploadLimits {
            upload_max_filesize: 5,
            ..LIMITS
        };
        let p = ok(run(&b, 3, true, limits).await);
        let got: Vec<_> = p
            .files
            .iter()
            .map(|f| {
                (
                    f.field_name.as_str(),
                    f.error,
                    f.size,
                    f.content_type.as_str(),
                )
            })
            .collect();
        assert_eq!(
            got,
            [
                ("a", 0, 1, "text/plain"),
                ("big", 1, 0, ""),
                ("c", 0, 3, "text/plain")
            ]
        );
        assert_eq!(p.files[1].tmp_path, "");
        assert_eq!(p.files[1].file_name, "big.bin");
        assert_eq!(p.temp_paths.paths.iter().filter(|t| t.exists()).count(), 2);
    }

    /// `MAX_FILE_SIZE`, as a form sends it before its file inputs: UPLOAD_ERR_FORM_SIZE.
    /// After a file it is too late for that file, as in PHP; `upload_max_filesize` wins
    /// when both are crossed by the same bytes.
    #[tokio::test]
    async fn a_max_file_size_field_limits_the_files_after_it() {
        let b = body(&[
            ("early", Some("e.txt"), None, "22"),
            ("MAX_FILE_SIZE", None, None, " 1"),
            ("one", Some("1.txt"), None, "1"),
            ("two", Some("2.txt"), None, "22"),
        ]);
        let p = ok(run(&b, 64, true, LIMITS).await);
        let got: Vec<_> = p
            .files
            .iter()
            .map(|f| (f.field_name.as_str(), f.error))
            .collect();
        assert_eq!(got, [("early", 0), ("one", 0), ("two", 2)]);
        assert_eq!(p.fields, [("MAX_FILE_SIZE".to_string(), " 1".to_string())]);

        let limits = UploadLimits {
            upload_max_filesize: 1,
            ..LIMITS
        };
        let p = ok(run(&b, 64, true, limits).await);
        assert_eq!(
            p.files[2].error, 1,
            "INI_SIZE before FORM_SIZE, as PHP checks"
        );
    }

    /// `max_file_uploads`: the files past it are dropped; an empty file input is not
    /// counted, as PHP does not count it.
    #[tokio::test]
    async fn files_past_max_file_uploads_are_dropped_and_empty_inputs_do_not_count() {
        let b = body(&[
            ("a", Some("a.txt"), None, "1"),
            ("none", Some(""), Some("application/octet-stream"), ""),
            ("b", Some("b.txt"), None, "2"),
            ("c", Some("c.txt"), None, "3"),
            ("after", None, None, "kept"),
        ]);
        let limits = UploadLimits {
            max_file_uploads: 2,
            ..LIMITS
        };
        let p = ok(run(&b, 64, true, limits).await);
        let got: Vec<_> = p
            .files
            .iter()
            .map(|f| (f.field_name.as_str(), f.error, f.content_type.as_str()))
            .collect();
        assert_eq!(got, [("a", 0, ""), ("none", 4, ""), ("b", 0, "")]);
        assert_eq!(p.fields, [("after".to_string(), "kept".to_string())]);
    }

    /// `file_uploads = 0`: no files at all; the fields still arrive.
    #[tokio::test]
    async fn with_file_uploads_off_files_are_dropped_and_fields_kept() {
        let b = body(&[
            ("note", None, None, "hello"),
            ("doc", Some("a.txt"), None, "x"),
        ]);
        let limits = UploadLimits {
            file_uploads: false,
            ..LIMITS
        };
        let p = ok(run(&b, 64, true, limits).await);
        assert!(p.files.is_empty());
        assert_eq!(p.fields, [("note".to_string(), "hello".to_string())]);
    }

    /// `post_max_size`: a body over it leaves no fields and no files — declared by its
    /// Content-Length, or found while streaming one without. Askr's own `max_body_size`
    /// is still a 413.
    #[tokio::test]
    async fn a_body_over_post_max_size_leaves_nothing_and_max_body_size_is_a_413() {
        let b = body(&[
            ("note", None, None, "hello"),
            ("doc", Some("a.txt"), None, &"x".repeat(200)),
        ]);
        let limits = UploadLimits {
            post_max_size: 100,
            ..LIMITS
        };
        for content_length in [true, false] {
            let p = ok(run(&b, 16, content_length, limits).await);
            assert!(
                p.fields.is_empty() && p.files.is_empty(),
                "content_length {content_length}"
            );
            assert_eq!(p.temp_paths.paths.iter().filter(|t| t.exists()).count(), 0);
        }
        // Declared over the limit, the body is not read at all: here it never arrives.
        let p = ok(parse(Body(Default::default()), CT, 1 << 20, Some(200), &limits).await);
        assert!(p.fields.is_empty() && p.files.is_empty());

        let chunks = std::iter::once(Bytes::from(b.clone())).collect();
        let r = parse(Body(chunks), CT, 64, Some(b.len() as u64), &limits).await;
        assert!(matches!(r, Err(UploadError::TooLarge)));
    }

    /// PHP's `type`: the part's Content-Type as sent, up to any ';' — and nothing when the
    /// part has none (not `application/octet-stream`).
    #[tokio::test]
    async fn the_type_is_the_content_type_as_sent_up_to_a_semicolon() {
        let b = body(&[
            ("a", Some("a.txt"), Some("text/plain; charset=utf-8"), "1"),
            ("b", Some("b.txt"), Some("text/plain-file3;"), "1"),
            ("c", Some("c.txt"), None, "1"),
        ]);
        let p = ok(run(&b, 64, true, LIMITS).await);
        let got: Vec<_> = p.files.iter().map(|f| f.content_type.as_str()).collect();
        assert_eq!(got, ["text/plain", "text/plain-file3", ""]);
    }

    /// A body that ends inside a file: UPLOAD_ERR_PARTIAL, nothing of it kept, and the parts
    /// before it kept — however the body arrives, the tail that ends it in a chunk of its
    /// own or not. A file exactly at upload_max_filesize is partial, not over it.
    #[tokio::test]
    async fn a_body_that_ends_inside_a_file_is_a_partial_upload() {
        let b = body(&[
            ("note", None, None, "hi"),
            ("a", Some("a.txt"), Some("text/plain"), "123"),
        ]);
        let cut = format!(
            "{}\r\n--{B}\r\nContent-Disposition: form-data; name=\"cut\"; filename=\"c.txt\"\r\n\
             Content-Type: text/plain\r\n\r\n4567",
            b.strip_suffix(&format!("\r\n--{B}--\r\n")).unwrap()
        );
        let limits = UploadLimits {
            upload_max_filesize: 4,
            ..LIMITS
        };
        for chunk in [1, 3, 7, 4096] {
            let p = ok(run(&cut, chunk, true, limits).await);
            let got: Vec<_> = p
                .files
                .iter()
                .map(|f| {
                    (
                        f.field_name.as_str(),
                        f.error,
                        f.size,
                        f.content_type.as_str(),
                        f.tmp_path.is_empty(),
                    )
                })
                .collect();
            assert_eq!(
                got,
                [("a", 0, 3, "text/plain", false), ("cut", 3, 0, "", true)],
                "chunk {chunk}"
            );
            assert_eq!(p.files[1].file_name, "c.txt");
            assert_eq!(p.fields, [("note".to_string(), "hi".to_string())]);
            assert_eq!(
                p.temp_paths.paths.iter().filter(|t| t.exists()).count(),
                1,
                "the partial file is not kept"
            );
        }

        // Cut inside a file that is already over the limit: that error stands, and the file
        // before it is whole.
        let over = format!("{cut}89");
        let p = ok(run(&over, 4096, true, limits).await);
        let got: Vec<_> = p
            .files
            .iter()
            .map(|f| (f.field_name.as_str(), f.error))
            .collect();
        assert_eq!(got, [("a", 0), ("cut", 1)]);
    }

    /// A body that ends inside a field: what arrived, short of a tail that could still have
    /// been the next boundary, as PHP keeps it.
    #[tokio::test]
    async fn a_body_that_ends_inside_a_field_keeps_what_arrived() {
        let start = format!("--{B}\r\nContent-Disposition: form-data; name=\"cut\"\r\n\r\n");
        for (tail, want) in [
            ("half", "half"),
            ("half\r\n", "half"),
            ("half\n", "half"),
            ("half\r", "half\r"),
            ("half\r\n--", "half"),
            ("half\r\n------askr", "half"),
            ("half\r\n----askr", "half\r\n----askr"),
            ("half\r\n-x", "half\r\n-x"),
            ("a\r\nb", "a\r\nb"),
        ] {
            for chunk in [1, 4096] {
                let p = ok(run(&format!("{start}{tail}"), chunk, true, LIMITS).await);
                assert_eq!(
                    p.fields,
                    [("cut".to_string(), want.to_string())],
                    "{tail:?}, chunk {chunk}"
                );
            }
        }
    }

    /// A body that ends before a part's data, or stops making sense, leaves the parts before
    /// that point and is no error; a body that cannot be read still is one.
    #[tokio::test]
    async fn a_body_cut_between_parts_or_malformed_keeps_the_parts_before() {
        let note = format!("--{B}\r\nContent-Disposition: form-data; name=\"note\"\r\n\r\nhi\r\n");
        for (tail, fields, files) in [
            ("", vec![("note", "hi")], vec![]),
            (
                "--x\r\nContent-Disposition: form-data; name=\"x\"\r\n",
                vec![("note", "hi"), ("x", "")],
                vec![],
            ),
            (
                "--x\r\nContent-Disposition: form-data; name=\"x\"; filename=\"x.txt\"\r\n",
                vec![("note", "hi")],
                vec![("x", 3)],
            ),
            (
                "--x\r\nnot a header\r\n\r\nx\r\n--x--\r\n",
                vec![("note", "hi")],
                vec![],
            ),
        ] {
            let body = format!("{note}{}", tail.replace("--x", &format!("--{B}")));
            for chunk in [1, 4096] {
                let p = ok(run(&body, chunk, true, LIMITS).await);
                let got: Vec<_> = p
                    .fields
                    .iter()
                    .map(|(k, v)| (k.as_str(), v.as_str()))
                    .collect();
                assert_eq!(got, fields, "{tail:?}, chunk {chunk}");
                let got: Vec<_> = p
                    .files
                    .iter()
                    .map(|f| (f.field_name.as_str(), f.error))
                    .collect();
                assert_eq!(got, files, "{tail:?}, chunk {chunk}");
            }
        }

        struct Broken(bool);
        impl Stream for Broken {
            type Item = Result<Bytes, std::io::Error>;
            fn poll_next(
                mut self: std::pin::Pin<&mut Self>,
                _: &mut std::task::Context<'_>,
            ) -> std::task::Poll<Option<Self::Item>> {
                let first = std::mem::replace(&mut self.0, false);
                std::task::Poll::Ready(Some(if first {
                    Ok(Bytes::from(format!("--{B}\r\n")))
                } else {
                    Err(std::io::Error::other("connection reset"))
                }))
            }
        }
        let r = parse(Broken(true), CT, 1 << 20, None, &LIMITS).await;
        assert!(matches!(r, Err(UploadError::Parse(_))));
    }

    /// The limit counts the body as it arrives, to the byte, and nothing Askr adds to it.
    #[tokio::test]
    async fn a_body_at_post_max_size_is_kept_and_one_byte_more_is_not() {
        let b = body(&[("note", None, None, "hello")]);
        let at = UploadLimits {
            post_max_size: b.len() as i64,
            ..LIMITS
        };
        let p = ok(run(&b, 5, false, at).await);
        assert_eq!(p.fields, [("note".to_string(), "hello".to_string())]);
        let under = UploadLimits {
            post_max_size: b.len() as i64 - 1,
            ..LIMITS
        };
        let p = ok(run(&b, 5, false, under).await);
        assert!(p.fields.is_empty());
    }

    /// PHP's media type match, and its boundary rules — the warning when there is none.
    #[test]
    fn the_boundary_is_found_as_php_finds_it() {
        for (ct, multipart) in [
            ("multipart/form-data; boundary=x", true),
            ("Multipart/Form-Data;boundary=x", true),
            ("multipart/form-data", true),
            ("multipart/form-data,x", true),
            ("multipart/form-datax; boundary=x", false),
            ("multipart/mixed; boundary=x", false),
        ] {
            assert_eq!(is_multipart(ct), multipart, "{ct:?}");
        }
        for (ct, want) in [
            ("multipart/form-data; boundary=abc", Ok("abc")),
            (
                "multipart/form-data; boundary=abc; charset=utf-8",
                Ok("abc"),
            ),
            ("multipart/form-data; boundary=foo, bar", Ok("foo")),
            ("multipart/form-data; boundary=\"a;b,c\"; x=y", Ok("a;b,c")),
            ("multipart/form-data; BOUNDARY=abc", Ok("abc")),
            ("multipart/form-data; boundary= abc", Ok(" abc")),
            ("multipart/form-data; boundary=", Ok("")),
            ("multipart/form-data", Err("Missing boundary")),
            ("multipart/form-data; boundary", Err("Missing boundary")),
            (
                "multipart/form-data; boundary=\"abc",
                Err("Invalid boundary"),
            ),
        ] {
            let got = boundary(ct).map_err(|w| {
                assert!(w.startup);
                w.message.split(" in ").next().unwrap().to_string()
            });
            assert_eq!(got, want.map_err(str::to_string), "{ct:?}");
        }
        let long = format!("multipart/form-data; boundary={}", "x".repeat(5117));
        let w = boundary(&long).unwrap_err();
        assert_eq!(
            w.message,
            "Boundary too large in multipart/form-data POST data"
        );
        assert!(
            !w.startup,
            "raised through the SAPI's hook, without the startup prefix"
        );
        let fits = format!("multipart/form-data; boundary={}", "x".repeat(5116));
        assert!(boundary(&fits).is_ok());
    }

    /// rfc1867.c's rule for a file field's name, and the numbers a nameless file is given.
    #[test]
    fn a_file_field_s_name_is_kept_by_php_s_rule() {
        for (name, kept) in [
            ("doc", true),
            ("a[]", true),
            ("a[b][c]", true),
            ("a[b][]", true),
            ("", true),
            ("0", true),
            ("foo[]bar", false),
            ("a[b]c[d]", false),
            ("a[b", false),
            ("a]b", false),
            ("a[b]]", false),
            ("a][", false),
            ("a[[b]]", false),
        ] {
            assert_eq!(php_keeps_file_name(name), kept, "{name:?}");
        }
    }

    #[tokio::test]
    async fn a_nameless_file_is_numbered_and_a_file_php_will_not_repair_ends_the_files() {
        let nameless = |file: &str| {
            format!("--{B}\r\nContent-Disposition: form-data; filename=\"{file}\"\r\n\r\nx\r\n")
        };
        let b = [
            nameless("a.txt"),
            body(&[("doc", Some("d.txt"), None, "x")]).replace(&format!("--{B}--\r\n"), ""),
            nameless("b.txt"),
            body(&[
                ("foo[]bar", Some("1.txt"), None, "x"),
                ("note", None, None, "kept"),
                ("ok", Some("2.txt"), None, "x"),
            ]),
        ]
        .concat();
        let p = ok(run(&b, 64, true, LIMITS).await);
        let got: Vec<_> = p
            .files
            .iter()
            .map(|f| (f.field_name.as_str(), f.file_name.as_str()))
            .collect();
        assert_eq!(got, [("0", "a.txt"), ("doc", "d.txt"), ("1", "b.txt")]);
        assert_eq!(p.fields, [("note".to_string(), "kept".to_string())]);
    }

    #[test]
    fn max_file_size_is_read_as_strtoll_reads_it() {
        for (s, n) in [
            ("100", 100),
            ("  42abc", 42),
            ("+7", 7),
            ("-3", -3),
            ("abc", 0),
            ("", 0),
            ("1e3", 1),
            ("99999999999999999999", i64::MAX),
            ("-99999999999999999999", i64::MIN),
        ] {
            assert_eq!(strtoll(s), n, "{s:?}");
        }
    }

    /// The cases php-src's own upload tests send, and the ones an attacker would.
    #[test]
    fn a_filename_loses_its_directory_part_as_php_strips_it() {
        for (sent, name) in [
            ("file1.txt", "file1.txt"),
            ("directory/subdirectory/file2.txt", "file2.txt"),
            ("C:\\foo\\bar/file3.txt", "file3.txt"),
            ("C:\\Users\\kh\\cv.pdf", "cv.pdf"),
            ("a/b\\c.txt", "c.txt"),
            ("../../public/shell.php", "shell.php"),
            ("..\\..\\shell.php", "shell.php"),
            ("dir/", ""),
            ("blåbær/søknad.pdf", "søknad.pdf"),
            ("", ""),
        ] {
            assert_eq!(basename(sent), name, "{sent:?}");
        }
    }

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
