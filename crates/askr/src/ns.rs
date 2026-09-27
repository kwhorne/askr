//! Per-application namespace for everything that lives in shared memory.
//!
//! The KV cache, the job queue and the response-cache tag table are one region per
//! *instance*. With `[[site]]` hosting several applications in one instance, that
//! meant any application's PHP could read every other's cache and sessions, flush all
//! of it with one `askr_cache_flush()`, acknowledge another application's jobs by id,
//! and invalidate its cached pages by tag name. Two sites deployed from the same
//! codebase under one `APP_KEY` shared sessions across domains.
//!
//! The namespace is derived from the application's **docroot**, not from the host: two
//! domains serving one docroot are one application and should share; two docroots are
//! two applications and must not. That makes it automatic — nothing to configure, and
//! nothing to get wrong — and it makes the sidecars fall out naturally: a queue worker
//! belongs to the application at the configured docroot, and takes that namespace.
//!
//! Single-application instances get exactly one namespace and never notice. Keys grow
//! by [`PREFIX_LEN`] bytes, so the effective maximum key length is that much shorter.
//!
//! The namespace is process-global, set as each request is handed to PHP (a PHP
//! worker serves one request at a time) and once at boot for sidecars. Broadcasting
//! is deliberately *not* namespaced: one instance has one Pusher secret, so it serves
//! one application's realtime traffic — see HOSTING.md.

use std::borrow::Cow;
use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, RwLock};

/// Separates the namespace from the key. ASCII unit separator: not something an
/// application puts in a cache key, and never a byte in a hex namespace.
pub const SEP: u8 = 0x1f;
/// Sixteen hex digits plus the separator.
pub const PREFIX_LEN: usize = 17;

/// One application's identity in shared memory: sixteen lowercase hex digits, derived
/// from its docroot.
///
/// A type rather than a `String` on purpose. Every fault in this area so far came from
/// identity being a loose string that code had to remember to carry: sidecars took the
/// wrong one (1.5.1–1.6.x, a queue nothing could drain), `by_queue` dropped it (two
/// applications' `mail` lanes reported as one), and the backlog classifier compared
/// display names (a dead queue reported as merely busy). An `App` can only be made from
/// a docroot or parsed from a stored key, so it cannot hold a stray string, and APIs that
/// report on shared memory hand it back typed rather than leaving it to be stripped off.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct App([u8; PREFIX_LEN - 1]);

impl App {
    /// The application rooted at `docroot`.
    ///
    /// Canonicalised first, so `/var/www/app/public` and `/var/www/app/public/` — or a
    /// symlink to either — agree, then hashed. Memoised: this is on the request path, and
    /// canonicalisation is a syscall.
    pub fn for_docroot(docroot: &Path) -> App {
        let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(app) = memo.lock().ok().and_then(|m| m.get(docroot).copied()) {
            return app;
        }
        let canonical = std::fs::canonicalize(docroot).unwrap_or_else(|_| docroot.to_path_buf());
        let mut h = std::collections::hash_map::DefaultHasher::new();
        canonical.as_os_str().hash(&mut h);
        let hex = format!("{:016x}", h.finish());
        let app = App::parse(&hex).expect("sixteen hex digits");
        if let Ok(mut m) = memo.lock() {
            m.insert(docroot.to_path_buf(), app);
        }
        app
    }

    /// Exactly sixteen hex digits, as written into a stored key or passed across the PHP
    /// boundary. Anything else is not an application.
    pub fn parse(s: &str) -> Option<App> {
        let b = s.as_bytes();
        if b.len() != PREFIX_LEN - 1 || !b.iter().all(u8::is_ascii_hexdigit) {
            return None;
        }
        let mut out = [0u8; PREFIX_LEN - 1];
        for (o, c) in out.iter_mut().zip(b) {
            *o = c.to_ascii_lowercase();
        }
        Some(App(out))
    }

    pub fn as_str(&self) -> &str {
        // Only ever built from ASCII hex digits.
        std::str::from_utf8(&self.0).expect("hex is ASCII")
    }

    /// `hex` + [`SEP`], the bytes a stored key starts with.
    pub(crate) fn prefix_bytes(&self) -> [u8; PREFIX_LEN] {
        let mut p = [SEP; PREFIX_LEN];
        p[..PREFIX_LEN - 1].copy_from_slice(&self.0);
        p
    }
}

impl std::fmt::Display for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::fmt::Debug for App {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "App({})", self.as_str())
    }
}

/// The application whose shared memory this process's PHP is talking to.
///
/// Ambient, because the PHP extension functions (`askr_cache_get('k')`, …) carry no
/// application argument, and the one thread running PHP serves one request at a time. It
/// is set as each request is handed to PHP, and once at boot for sidecars. Everything on
/// the Rust side that can be told the application explicitly is — the response cache,
/// the reporting scans — so this is the bridge's context, not a convenience for Rust.
static CURRENT: RwLock<Option<App>> = RwLock::new(None);
static MEMO: OnceLock<Mutex<HashMap<PathBuf, App>>> = OnceLock::new();

/// Shorthand for [`App::for_docroot`].
pub fn for_docroot(docroot: &Path) -> App {
    App::for_docroot(docroot)
}

/// Make `app` the application for shared-memory operations on this thread's PHP.
pub fn set(app: &App) {
    if let Ok(mut cur) = CURRENT.write() {
        *cur = Some(*app);
    }
}

/// [`set`] from the text form a request carries across the `askr_php` boundary, which
/// cannot name this crate's types.
///
/// That text is always written by [`App`]'s `Display`, so a value that does not parse is
/// a bug, not input. It is logged as one, and the thread falls back to no application
/// rather than keeping whichever application the previous request had — which would
/// silently read and write another application's keys.
pub fn set_from_request(text: &str) {
    match App::parse(text) {
        Some(app) => set(&app),
        None => {
            tracing::error!(namespace = %text, "request carries a malformed application id; serving it with none");
            clear();
        }
    }
}

/// No application: the raw table. For tests and tooling; a serving process always has one.
pub fn clear() {
    if let Ok(mut cur) = CURRENT.write() {
        *cur = None;
    }
}

/// The application in force, if any.
pub fn current() -> Option<App> {
    CURRENT.read().ok().and_then(|c| *c)
}

/// `key`, prefixed with the current application — or unchanged when none is set, so a
/// process that never called [`set`] (tests, tooling) sees the raw table.
pub fn key(key: &[u8]) -> Cow<'_, [u8]> {
    match current() {
        Some(app) => {
            let mut out = Vec::with_capacity(PREFIX_LEN + key.len());
            out.extend_from_slice(&app.prefix_bytes());
            out.extend_from_slice(key);
            Cow::Owned(out)
        }
        None => Cow::Borrowed(key),
    }
}

/// Does this stored key belong to the current application? With none set, everything
/// does — the raw view.
pub fn owns(stored: &[u8]) -> bool {
    match current() {
        Some(app) => stored.starts_with(&app.prefix_bytes()),
        None => true,
    }
}

/// A stored key taken apart: the application it belongs to, and the name a person would
/// recognise.
///
/// One call for both halves, replacing a pair (`namespace_of` + `strip`) that callers had
/// to remember to use together — and the failure was always using only `strip`, which is
/// how two applications' `mail` queues were reported as one. A key without a namespace
/// comes back as `(None, key)`, and a raw key that merely contains [`SEP`] is not mistaken
/// for a namespaced one.
pub fn split(stored: &[u8]) -> (Option<App>, &[u8]) {
    if stored.get(PREFIX_LEN - 1) == Some(&SEP) {
        if let Some(app) = std::str::from_utf8(&stored[..PREFIX_LEN - 1])
            .ok()
            .and_then(App::parse)
        {
            return (Some(app), &stored[PREFIX_LEN..]);
        }
    }
    (None, stored)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Tests share the process-global namespace, so they serialise on this.
    pub(crate) static GUARD: Mutex<()> = Mutex::new(());

    /// A fixed application for tests. Panics on a malformed literal, which is a test bug.
    pub(crate) fn app(hex: &str) -> App {
        App::parse(hex).unwrap_or_else(|| panic!("not an app: {hex:?}"))
    }

    #[test]
    fn a_docroot_maps_to_one_stable_application_and_different_roots_differ() {
        let a = for_docroot(Path::new("/var/www/one/public"));
        let b = for_docroot(Path::new("/var/www/two/public"));
        assert_eq!(a.as_str().len(), 16);
        assert!(a.as_str().bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, for_docroot(Path::new("/var/www/one/public")), "stable");
        assert_ne!(a, b, "two applications, two identities");
    }

    #[test]
    fn only_sixteen_hex_digits_are_an_application() {
        assert!(App::parse("00000000deadbeef").is_some());
        assert_eq!(
            App::parse("00000000DEADBEEF"),
            App::parse("00000000deadbeef")
        );
        for bad in [
            "",
            "deadbeef",
            "00000000deadbeef0",
            "zzzzzzzzzzzzzzzz",
            "0000000 deadbeef",
        ] {
            assert!(App::parse(bad).is_none(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn a_key_is_prefixed_only_while_an_application_is_set() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        clear();
        assert_eq!(&*key(b"user:1"), b"user:1", "no application: the raw table");
        assert!(owns(b"anything"));

        let a = app("00000000deadbeef");
        set(&a);
        let k = key(b"user:1");
        assert_eq!(k.len(), PREFIX_LEN + 6);
        assert!(k.starts_with(b"00000000deadbeef\x1f"));
        assert_eq!(
            split(&k),
            (Some(a), &b"user:1"[..]),
            "split undoes key, both halves"
        );
        assert!(owns(&k));
        assert!(
            !owns(b"11111111deadbeef\x1fuser:1"),
            "another application's key"
        );
        // A raw key that merely contains the separator is not a namespaced one.
        assert_eq!(split(b"odd\x1fkey"), (None, &b"odd\x1fkey"[..]));
        // Nor is sixteen bytes that are not hex, followed by the separator.
        assert_eq!(split(b"zzzzzzzzzzzzzzzz\x1fk").0, None);
        clear();
    }
}
