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

static CURRENT: RwLock<String> = RwLock::new(String::new());
static MEMO: OnceLock<Mutex<HashMap<PathBuf, String>>> = OnceLock::new();

/// The namespace for an application rooted at `docroot`.
///
/// Canonicalised first, so `/var/www/app/public` and `/var/www/app/public/` — or a
/// symlink to either — agree, then hashed to sixteen hex digits. Memoised: this is on
/// the request path, and canonicalisation is a syscall.
pub fn for_docroot(docroot: &Path) -> String {
    let memo = MEMO.get_or_init(|| Mutex::new(HashMap::new()));
    if let Some(ns) = memo.lock().ok().and_then(|m| m.get(docroot).cloned()) {
        return ns;
    }
    let canonical = std::fs::canonicalize(docroot).unwrap_or_else(|_| docroot.to_path_buf());
    let mut h = std::collections::hash_map::DefaultHasher::new();
    canonical.as_os_str().hash(&mut h);
    let ns = format!("{:016x}", h.finish());
    if let Ok(mut m) = memo.lock() {
        m.insert(docroot.to_path_buf(), ns.clone());
    }
    ns
}

/// Make `ns` the namespace for shared-memory operations on this thread's PHP.
pub fn set(ns: &str) {
    if let Ok(mut cur) = CURRENT.write() {
        if cur.as_str() != ns {
            cur.clear();
            cur.push_str(ns);
        }
    }
}

/// The namespace in force, empty when none has been set.
pub fn current() -> String {
    CURRENT.read().map(|c| c.clone()).unwrap_or_default()
}

/// `key`, prefixed with the current namespace — or unchanged when none is set, so a
/// process that never called [`set`] (tests, tooling) sees the raw table.
pub fn key(key: &[u8]) -> Cow<'_, [u8]> {
    let cur = CURRENT.read();
    match cur.as_deref() {
        Ok(ns) if !ns.is_empty() => {
            let mut out = Vec::with_capacity(ns.len() + 1 + key.len());
            out.extend_from_slice(ns.as_bytes());
            out.push(SEP);
            out.extend_from_slice(key);
            Cow::Owned(out)
        }
        _ => Cow::Borrowed(key),
    }
}

/// The current namespace's prefix bytes (`ns` + [`SEP`]), empty when none is set.
pub fn prefix() -> Vec<u8> {
    let mut p = current().into_bytes();
    if !p.is_empty() {
        p.push(SEP);
    }
    p
}

/// Does this stored key belong to the current namespace? With no namespace set,
/// everything does — the raw view.
pub fn owns(stored: &[u8]) -> bool {
    let p = prefix();
    p.is_empty() || stored.starts_with(&p)
}

/// A stored key without its namespace, for anything that shows keys to people.
pub fn strip(stored: &[u8]) -> &[u8] {
    match stored.get(PREFIX_LEN - 1) {
        Some(&SEP)
            if stored[..PREFIX_LEN - 1]
                .iter()
                .all(|b| b.is_ascii_hexdigit()) =>
        {
            &stored[PREFIX_LEN..]
        }
        _ => stored,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Tests share the process-global namespace, so they serialise on this.
    pub(crate) static GUARD: Mutex<()> = Mutex::new(());

    #[test]
    fn a_docroot_maps_to_one_stable_namespace_and_different_roots_differ() {
        let a = for_docroot(Path::new("/var/www/one/public"));
        let b = for_docroot(Path::new("/var/www/two/public"));
        assert_eq!(a.len(), 16);
        assert!(a.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(a, for_docroot(Path::new("/var/www/one/public")), "stable");
        assert_ne!(a, b, "two applications, two namespaces");
    }

    #[test]
    fn a_key_is_prefixed_only_while_a_namespace_is_set() {
        let _g = GUARD.lock().unwrap_or_else(|e| e.into_inner());
        set("");
        assert_eq!(&*key(b"user:1"), b"user:1", "no namespace: the raw table");
        assert!(owns(b"anything"));

        set("00000000deadbeef");
        let k = key(b"user:1");
        assert_eq!(k.len(), PREFIX_LEN + 6);
        assert!(k.starts_with(b"00000000deadbeef\x1f"));
        assert_eq!(strip(&k), b"user:1", "strip undoes key");
        assert!(owns(&k));
        assert!(
            !owns(b"11111111deadbeef\x1fuser:1"),
            "another namespace's key"
        );
        // A raw key that merely contains the separator is not a namespaced one.
        assert_eq!(strip(b"odd\x1fkey"), b"odd\x1fkey");
        set("");
    }
}
