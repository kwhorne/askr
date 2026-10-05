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
//! two applications and must not. It is the docroot *as configured* — made absolute, but
//! with symlinks left alone — so `/srv/app/current/public` stays one application across
//! deploys that swap `current` from one release to the next. That makes it automatic — nothing to configure, and
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
use std::hash::Hash;
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
    /// Made absolute and lexically tidied first ([`app_path`]), so
    /// `/var/www/app/public` and `/var/www/app/public/` agree, then hashed. Symlinks are
    /// *not* resolved: that is what a release deploy swaps, and resolving it made every
    /// deploy a new application — and, worse, pinned the server to the release it
    /// started on (see `config::app_root`). Memoised: this is on the request path.
    pub fn for_docroot(docroot: &Path) -> App {
        let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
        if let Some(app) = memo.lock().ok().and_then(|m| m.get(docroot).copied()) {
            return app;
        }
        let path = app_path(docroot);
        let hex = format!("{:016x}", path_hash(path.as_os_str()));
        let app = App::parse(&hex).expect("sixteen hex digits");
        if let Ok(mut m) = memo.lock() {
            m.insert(docroot.to_path_buf(), app);
        }
        app
    }

    /// The application named `id` by an `app_id` in the configuration.
    ///
    /// Hashed like a docroot, from a different input — `app_id` and a NUL in front — so
    /// it cannot collide with one. The same `id` gives the same application on every
    /// host and under every path, which a docroot cannot promise: that is what lets two
    /// boxes, or the durable SQL backends, agree on whose data is whose.
    pub fn for_id(id: &str) -> App {
        let mut input = b"app_id\0".to_vec();
        input.extend_from_slice(id.as_bytes());
        let hex = format!("{:016x}", bytes_hash(&input));
        App::parse(&hex).expect("sixteen hex digits")
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

/// `p` made absolute and tidied — `.` and repeated or trailing separators dropped —
/// without touching the filesystem, so a symlink in it stays a symlink. The one
/// definition of how a docroot is spelled, for serving and for identity alike.
pub fn app_path(p: &Path) -> PathBuf {
    let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
    abs.components().collect()
}

/// The hash a namespace is made of: SipHash-1-3 with zero keys over the path's bytes,
/// length-prefixed.
///
/// That is exactly what `DefaultHasher::new()` computes for `OsStr::hash` today — the
/// test below holds the two to the same answers — written out because std promises
/// nothing about that algorithm across releases. A toolchain that changed it would have
/// moved every application to a new namespace on upgrade, and stranded the jobs in a
/// persisted queue ring under the old one.
fn path_hash(p: &std::ffi::OsStr) -> u64 {
    bytes_hash(p.as_encoded_bytes())
}

/// [`path_hash`] over plain bytes.
fn bytes_hash(bytes: &[u8]) -> u64 {
    let mut msg = Vec::with_capacity(8 + bytes.len());
    msg.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    msg.extend_from_slice(bytes);
    sip13(&msg)
}

/// SipHash-1-3, keys (0, 0).
fn sip13(msg: &[u8]) -> u64 {
    let (mut v0, mut v1, mut v2, mut v3) = (
        0x736f_6d65_7073_6575u64,
        0x646f_7261_6e64_6f6du64,
        0x6c79_6765_6e65_7261u64,
        0x7465_6462_7974_6573u64,
    );
    let round = |v0: &mut u64, v1: &mut u64, v2: &mut u64, v3: &mut u64| {
        *v0 = v0.wrapping_add(*v1);
        *v1 = v1.rotate_left(13) ^ *v0;
        *v0 = v0.rotate_left(32);
        *v2 = v2.wrapping_add(*v3);
        *v3 = v3.rotate_left(16) ^ *v2;
        *v0 = v0.wrapping_add(*v3);
        *v3 = v3.rotate_left(21) ^ *v0;
        *v2 = v2.wrapping_add(*v1);
        *v1 = v1.rotate_left(17) ^ *v2;
        *v2 = v2.rotate_left(32);
    };
    let mut chunks = msg.chunks_exact(8);
    for c in &mut chunks {
        let m = u64::from_le_bytes(c.try_into().expect("eight bytes"));
        v3 ^= m;
        round(&mut v0, &mut v1, &mut v2, &mut v3);
        v0 ^= m;
    }
    let mut last = (msg.len() as u64 & 0xff) << 56;
    for (i, b) in chunks.remainder().iter().enumerate() {
        last |= (*b as u64) << (8 * i);
    }
    v3 ^= last;
    round(&mut v0, &mut v1, &mut v2, &mut v3);
    v0 ^= last;
    v2 ^= 0xff;
    for _ in 0..3 {
        round(&mut v0, &mut v1, &mut v2, &mut v3);
    }
    v0 ^ v1 ^ v2 ^ v3
}

/// Is `id` usable as an `app_id`? Lowercase letters, digits, `.`, `_` and `-`, starting
/// with a letter or digit, at most 64 bytes — it ends up in logs and in key prefixes.
pub fn valid_app_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= 64
        && b[0].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(c))
}

static NAMED: RwLock<Vec<App>> = RwLock::new(Vec::new());

/// Make the application at `docroot` the one named `id`, for this process and every
/// process forked from it. Called at startup, before the workers exist.
pub fn name_app(docroot: &Path, id: &str) -> App {
    let app = App::for_id(id);
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Ok(mut m) = memo.lock() {
        m.insert(docroot.to_path_buf(), app);
        m.insert(app_path(docroot), app);
    }
    if let Ok(mut n) = NAMED.write() {
        if !n.contains(&app) {
            n.push(app);
        }
    }
    app
}

/// Was `app` named by an `app_id`, rather than derived from where it lives?
#[cfg(any(feature = "sql-backend", test))]
pub fn is_named(app: &App) -> bool {
    NAMED.read().is_ok_and(|n| n.contains(app))
}

/// `key` as the durable SQL backends store it: prefixed with the current application
/// when that application has an `app_id`, and unchanged otherwise.
///
/// Only a named application is prefixed, because only its identity is the same on every
/// host that shares the database — and because prefixing everyone would have hidden every
/// existing row from the deployments already using those backends.
#[cfg(any(feature = "sql-backend", test))]
pub fn l2_key(key: &[u8]) -> Cow<'_, [u8]> {
    match current() {
        Some(app) if is_named(&app) => {
            let mut out = Vec::with_capacity(PREFIX_LEN + key.len());
            out.extend_from_slice(&app.prefix_bytes());
            out.extend_from_slice(key);
            Cow::Owned(out)
        }
        _ => Cow::Borrowed(key),
    }
}

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

/// Does this stored key belong to the current application?
///
/// With none set, only keys that carry no application do: the raw space, the same one
/// [`key`] writes into and a no-application `pop` reads from. It used to be every key.
/// That was the last place where "no application" meant "every application" — and the
/// one caller is the queue's lease check, so a process with no application set could
/// acknowledge or release any application's job by presenting its lease, and leases are
/// a global counter handed out in sequence. The same kind of omission had already cost
/// a queue that nothing drained, and a cache flush that emptied every site.
pub fn owns(stored: &[u8]) -> bool {
    match current() {
        Some(app) => stored.starts_with(&app.prefix_bytes()),
        None => split(stored).0.is_none(),
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

    /// The namespace hash is `DefaultHasher`'s answer today, held fixed: same value for
    /// every path, so no application moves on the upgrade that introduces it — and
    /// none will move when a toolchain changes the std algorithm.
    #[test]
    fn the_namespace_hash_is_the_one_applications_already_have() {
        use std::hash::{Hash, Hasher};
        let mut paths: Vec<String> = vec![
            String::new(),
            "/".into(),
            "/var/www/app/public".into(),
            "/srv/blåbær/public".into(),
            "/private/var/folders/xy/T/askr-cfg-one-123/".into(),
        ];
        // Every length across several SipHash blocks, so each tail size is covered.
        for n in 0..70 {
            paths.push(format!("/{}", "x".repeat(n)));
        }
        for p in &paths {
            let os = std::ffi::OsStr::new(p);
            let mut h = std::collections::hash_map::DefaultHasher::new();
            os.hash(&mut h);
            assert_eq!(path_hash(os), h.finish(), "{p:?}");
        }
    }

    #[test]
    fn a_symlink_in_the_docroot_is_part_of_its_name() {
        let base = std::env::temp_dir().join(format!("askr-ns-link-{}", std::process::id()));
        let (a, b) = (base.join("releases/a"), base.join("releases/b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let current = base.join("current");
        let _ = std::fs::remove_file(&current);
        std::os::unix::fs::symlink(&a, &current).unwrap();
        let before = App::for_docroot(&current.join("."));
        std::fs::remove_file(&current).unwrap();
        std::os::unix::fs::symlink(&b, &current).unwrap();
        // Through a fresh spelling, so the memo cannot answer for it.
        let after = App::for_docroot(&base.join("current/"));
        assert_eq!(
            before, after,
            "a deploy that swaps the link is the same application"
        );
        assert_ne!(
            after,
            App::for_docroot(&b),
            "the release directory is another name"
        );
        assert_eq!(
            app_path(Path::new("/srv/app/./current//public/")),
            Path::new("/srv/app/current/public")
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn an_app_id_names_the_application_wherever_it_lives() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(App::for_id("shop"), App::for_id("shop"), "same everywhere");
        assert_ne!(App::for_id("shop"), App::for_id("blog"));
        assert_ne!(
            App::for_id("shop"),
            App::for_docroot(Path::new("shop")),
            "an id is not a path"
        );
        let named = name_app(Path::new("/srv/one/public/"), "one");
        assert_eq!(for_docroot(Path::new("/srv/one/public")), named);
        assert!(is_named(&named));
        assert!(!is_named(&for_docroot(Path::new("/srv/two/public"))));

        // Only a named application's durable keys are prefixed.
        set(&named);
        assert_eq!(
            &*l2_key(b"k"),
            [&named.prefix_bytes()[..], b"k"].concat().as_slice()
        );
        set(&for_docroot(Path::new("/srv/two/public")));
        assert_eq!(&*l2_key(b"k"), b"k");
        clear();

        for ok in ["shop", "shop-2", "a.b_c", "0"] {
            assert!(valid_app_id(ok), "{ok}");
        }
        for bad in ["", "Shop", "-x", "a b", "æ", &"x".repeat(65)] {
            assert!(!valid_app_id(bad), "{bad}");
        }
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
        assert!(owns(b"anything"), "a raw key belongs to the raw space");
        assert!(
            !owns(b"00000000deadbeef\x1fuser:1"),
            "but an application's key does not belong to a process that has none"
        );

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
