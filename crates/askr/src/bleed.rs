//! State bleed found while serving: what the state-bleed detector reported, kept where
//! the admin plane can show it.
//!
//! The detector (`examples/askr-paranoid.php`) runs in each worker, compares the
//! application's mutable state between requests, and calls `askr_state_bleed($json)`
//! with what keeps growing. Before this, its findings went to the log and nowhere else —
//! which is how a fault Askr knows about stays invisible to the product watching it. Here
//! they land in a small table in shared memory, one row per leaking key, and
//! `/api/status` lists them under `state_bleed`.
//!
//! Findings are rare by construction (in production the detector reports a key only after
//! it grew in several consecutive samples), so one lock for the whole table is enough.

use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU32, AtomicU64, AtomicU8, Ordering};

/// Leaking keys the table can hold; further ones are counted in `dropped`.
pub const SLOTS: usize = 64;
const KEY_MAX: usize = 160;
const FP_MAX: usize = 48;

#[repr(C)]
struct Text<const N: usize> {
    len: AtomicU64,
    bytes: [AtomicU8; N],
}

impl<const N: usize> Text<N> {
    fn set(&self, s: &str) {
        let mut n = s.len().min(N);
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        for (d, b) in self.bytes.iter().zip(&s.as_bytes()[..n]) {
            d.store(*b, Ordering::Relaxed);
        }
        self.len.store(n as u64, Ordering::Relaxed);
    }
    fn get(&self) -> String {
        let n = (self.len.load(Ordering::Relaxed) as usize).min(N);
        let v: Vec<u8> = self.bytes[..n]
            .iter()
            .map(|b| b.load(Ordering::Relaxed))
            .collect();
        String::from_utf8_lossy(&v).into_owned()
    }
}

#[repr(C)]
struct Row {
    used: AtomicU64,
    app: Text<16>,
    key: Text<KEY_MAX>,
    from: Text<FP_MAX>,
    to: Text<FP_MAX>,
    reports: AtomicU64,
    first_ms: AtomicU64,
    last_ms: AtomicU64,
}

#[repr(C)]
struct Table {
    lock: AtomicU32,
    dropped: AtomicU64,
    rows: [Row; SLOTS],
}

static TABLE: AtomicPtr<Table> = AtomicPtr::new(ptr::null_mut());

/// Map the table. Call once in the master before forking.
pub fn init() {
    if !TABLE.load(Ordering::SeqCst).is_null() {
        return;
    }
    if let Some(t) = map_table() {
        TABLE.store(t, Ordering::SeqCst);
    }
}

fn map_table() -> Option<*mut Table> {
    // SAFETY: an anonymous shared mapping, zero-filled; all-zero is an empty Table.
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            std::mem::size_of::<Table>(),
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANON,
            -1,
            0,
        )
    };
    (p != libc::MAP_FAILED).then_some(p as *mut Table)
}

fn table() -> Option<&'static Table> {
    let p = TABLE.load(Ordering::Acquire);
    // SAFETY: set once by `init` to a mapping that lives for the process.
    (!p.is_null()).then(|| unsafe { &*p })
}

struct Guard<'a>(&'a AtomicU32);
impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.0.store(0, Ordering::Release);
    }
}
fn lock(t: &Table) -> Guard<'_> {
    while t
        .lock
        .compare_exchange_weak(0, 1, Ordering::Acquire, Ordering::Relaxed)
        .is_err()
    {
        std::hint::spin_loop();
    }
    Guard(&t.lock)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// One finding, as the detector sends it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct Finding {
    pub key: String,
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
}

/// Record the detector's findings for `app`. Returns how many were taken.
fn record_in(t: &Table, app: &str, findings: &[Finding], now: u64) -> usize {
    let _g = lock(t);
    let mut taken = 0;
    for f in findings.iter().filter(|f| !f.key.is_empty()) {
        let existing = t.rows.iter().find(|r| {
            r.used.load(Ordering::Relaxed) == 1 && r.key.get() == f.key && r.app.get() == app
        });
        let row = match existing {
            Some(r) => r,
            None => match t.rows.iter().find(|r| r.used.load(Ordering::Relaxed) == 0) {
                Some(r) => {
                    r.app.set(app);
                    r.key.set(&f.key);
                    r.first_ms.store(now, Ordering::Relaxed);
                    r.used.store(1, Ordering::Relaxed);
                    tracing::warn!(
                        key = %f.key,
                        app,
                        from = %f.from,
                        to = %f.to,
                        "state bleed: this keeps growing between requests in a worker — \
                         see GET /api/status `state_bleed`"
                    );
                    r
                }
                None => {
                    t.dropped.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
            },
        };
        row.from.set(&f.from);
        row.to.set(&f.to);
        row.reports.fetch_add(1, Ordering::Relaxed);
        row.last_ms.store(now, Ordering::Relaxed);
        taken += 1;
    }
    taken
}

/// One leaking key, as `/api/status` reports it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Leak {
    pub key: String,
    /// The application it was found in (sixteen hex digits), when one was set.
    pub app: Option<String>,
    /// The fingerprint before and after the last growth, e.g. `array:2` → `array:9`.
    pub from: String,
    pub to: String,
    /// How many times a worker reported it.
    pub reports: u64,
    pub first_seen_secs: u64,
    pub last_seen_secs: u64,
}

fn snapshot_of(t: &Table, now: u64) -> (Vec<Leak>, u64) {
    let _g = lock(t);
    let mut out: Vec<Leak> = t
        .rows
        .iter()
        .filter(|r| r.used.load(Ordering::Relaxed) == 1)
        .map(|r| {
            let app = r.app.get();
            Leak {
                key: r.key.get(),
                app: (!app.is_empty()).then_some(app),
                from: r.from.get(),
                to: r.to.get(),
                reports: r.reports.load(Ordering::Relaxed),
                first_seen_secs: now.saturating_sub(r.first_ms.load(Ordering::Relaxed)) / 1000,
                last_seen_secs: now.saturating_sub(r.last_ms.load(Ordering::Relaxed)) / 1000,
            }
        })
        .collect();
    out.sort_by(|a, b| b.reports.cmp(&a.reports).then_with(|| a.key.cmp(&b.key)));
    (out, t.dropped.load(Ordering::Relaxed))
}

/// Every leaking key reported so far, most-reported first, and how many findings did not
/// fit.
pub fn snapshot() -> (Vec<Leak>, u64) {
    table()
        .map(|t| snapshot_of(t, now_ms()))
        .unwrap_or_default()
}

extern "C" fn c_bleed(json: *const std::ffi::c_char, len: usize) -> std::ffi::c_int {
    crate::ffi::guard("bleed::report", 0, || {
        let bytes = unsafe { crate::ffi::bytes(json, len) };
        let Ok(findings) = serde_json::from_slice::<Vec<Finding>>(bytes) else {
            tracing::warn!("askr_state_bleed(): not a JSON list of {{key, from, to}}");
            return 0;
        };
        let Some(t) = table() else { return 0 };
        let app = crate::ns::current()
            .map(|a| a.to_string())
            .unwrap_or_default();
        (record_in(t, &app, &findings, now_ms()) > 0) as std::ffi::c_int
    })
}

/// Register `askr_state_bleed()` with the PHP shim for this process.
pub fn register_bridge() {
    // SAFETY: one-time registration of a 'static function.
    unsafe { askr_php::bleed_bridge::askr_php_set_bleed_bridge(c_bleed) };
}

#[cfg(test)]
mod tests {
    use super::*;

    fn own() -> &'static Table {
        // SAFETY: freshly mapped and leaked for the test process.
        unsafe { &*map_table().expect("mmap") }
    }

    fn f(key: &str, from: &str, to: &str) -> Finding {
        Finding {
            key: key.into(),
            from: from.into(),
            to: to.into(),
        }
    }

    #[test]
    fn a_key_is_one_row_per_application_and_keeps_its_latest_growth() {
        let t = own();
        record_in(
            t,
            "aaaaaaaaaaaaaaaa",
            &[f("App\\Cart::$items", "array:2", "array:5")],
            1_000,
        );
        record_in(
            t,
            "aaaaaaaaaaaaaaaa",
            &[f("App\\Cart::$items", "array:5", "array:9")],
            61_000,
        );
        record_in(
            t,
            "bbbbbbbbbbbbbbbb",
            &[f("App\\Cart::$items", "array:1", "array:2")],
            61_000,
        );
        let (leaks, dropped) = snapshot_of(t, 121_000);
        assert_eq!(dropped, 0);
        assert_eq!(leaks.len(), 2, "{leaks:?}");
        let a = &leaks[0];
        assert_eq!(
            (a.reports, a.from.as_str(), a.to.as_str()),
            (2, "array:5", "array:9")
        );
        assert_eq!(a.app.as_deref(), Some("aaaaaaaaaaaaaaaa"));
        assert_eq!((a.first_seen_secs, a.last_seen_secs), (120, 60));
    }

    #[test]
    fn a_full_table_counts_what_it_could_not_keep() {
        let t = own();
        let many: Vec<Finding> = (0..SLOTS + 5)
            .map(|i| f(&format!("K{i}"), "a", "b"))
            .collect();
        assert_eq!(record_in(t, "", &many, 0), SLOTS);
        let (leaks, dropped) = snapshot_of(t, 0);
        assert_eq!((leaks.len(), dropped), (SLOTS, 5));
        assert_eq!(leaks[0].app, None);
    }

    #[test]
    fn the_detector_s_json_is_what_is_parsed() {
        let v: Vec<Finding> = serde_json::from_str(
            r#"[{"key":"App\\Foo::$cache","from":"array:2","to":"array:3","samples":3}]"#,
        )
        .unwrap();
        assert_eq!(v, vec![f("App\\Foo::$cache", "array:2", "array:3")]);
    }
}
