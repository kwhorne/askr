//! Shared-memory broadcast ring — the fan-out bus behind `askr_broadcast()`.
//!
//! PHP (in any worker or sidecar) publishes an event into a circular buffer in
//! shared memory. Every worker process has a background task tailing the ring
//! and pushing matching events to the SSE connections it holds locally. So a
//! publish from *any* process reaches subscribers in *all* processes — with no
//! external broker (this is the in-binary alternative to Reverb/Pusher for the
//! common case).
//!
//! A light seqlock per slot keeps reads consistent under concurrent writes; a
//! slow reader that falls more than a ring behind simply drops the oldest
//! events (fine for live updates).

use std::ptr;
use std::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

const CHAN_MAX: usize = 64;
const PAYLOAD_MAX: usize = 3072;
const RING_SLOTS: u64 = 1024;

#[repr(C)]
struct Event {
    seq: AtomicU64, // commit marker; 0 = empty
    chan_len: u32,
    payload_len: u32,
    chan: [u8; CHAN_MAX],
    payload: [u8; PAYLOAD_MAX],
}

#[repr(C)]
struct Ring {
    write_seq: AtomicU64,
    _pad: [u64; 7],
    slots: [Event; RING_SLOTS as usize],
}

static RING_PTR: AtomicPtr<Ring> = AtomicPtr::new(ptr::null_mut());

/// A delivered event: (channel, payload).
pub type Delivered = (Vec<u8>, Vec<u8>);

/// Map the shared ring. Call in the master before forking.
pub fn init() {
    if !RING_PTR.load(Ordering::SeqCst).is_null() {
        return;
    }
    let size = std::mem::size_of::<Ring>();
    // SAFETY: anonymous shared mapping; zeroed pages are a valid empty ring.
    let p = unsafe {
        libc::mmap(
            ptr::null_mut(),
            size,
            libc::PROT_READ | libc::PROT_WRITE,
            libc::MAP_SHARED | libc::MAP_ANON,
            -1,
            0,
        )
    };
    if p == libc::MAP_FAILED {
        tracing::warn!("broadcast: mmap failed; disabled");
        return;
    }
    RING_PTR.store(p as *mut Ring, Ordering::SeqCst);
    tracing::info!(mib = size / 1024 / 1024, "broadcast ring mapped");
}

pub fn enabled() -> bool {
    !RING_PTR.load(Ordering::SeqCst).is_null()
}

fn ring() -> Option<*mut Ring> {
    let p = RING_PTR.load(Ordering::SeqCst);
    if p.is_null() {
        None
    } else {
        Some(p)
    }
}

/// Publish an event. Returns false if disabled or the event is too large.
pub fn publish(chan: &[u8], payload: &[u8]) -> bool {
    let Some(r) = ring() else {
        return false;
    };
    if chan.len() > CHAN_MAX || payload.len() > PAYLOAD_MAX {
        return false;
    }
    // SAFETY: shared mapping; distinct seq → distinct slot for concurrent
    // publishers (collisions only N apart, caught by the reader's seqlock).
    unsafe {
        let seq = (*r).write_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let slot = &raw mut (*r).slots[((seq - 1) % RING_SLOTS) as usize];
        // Mark in-progress (seq 0) so a reader can't accept a half-written slot.
        (*slot).seq.store(0, Ordering::Release);
        ptr::write(ptr::addr_of_mut!((*slot).chan_len), chan.len() as u32);
        ptr::write(ptr::addr_of_mut!((*slot).payload_len), payload.len() as u32);
        ptr::copy_nonoverlapping(
            chan.as_ptr(),
            ptr::addr_of_mut!((*slot).chan) as *mut u8,
            chan.len(),
        );
        ptr::copy_nonoverlapping(
            payload.as_ptr(),
            ptr::addr_of_mut!((*slot).payload) as *mut u8,
            payload.len(),
        );
        (*slot).seq.store(seq, Ordering::Release); // commit
    }
    true
}

/// The current write sequence — a subscriber starts here to skip history.
pub fn current_seq() -> u64 {
    ring()
        .map(|r| unsafe { (*r).write_seq.load(Ordering::Acquire) })
        .unwrap_or(0)
}

/// Read events with sequence in `(last, now]`. Returns the events and the new
/// `last`. Events older than a full ring are dropped.
pub fn read_from(last: u64) -> (Vec<Delivered>, u64) {
    let Some(r) = ring() else {
        return (Vec::new(), last);
    };
    let w = unsafe { (*r).write_seq.load(Ordering::Acquire) };
    if w == 0 || w <= last {
        return (Vec::new(), w);
    }
    let start = (last + 1).max(w.saturating_sub(RING_SLOTS - 1));
    let mut out = Vec::new();
    for s in start..=w {
        // SAFETY: shared mapping; seqlock ensures we only accept a fully-written,
        // non-overwritten slot; lengths are clamped before slicing.
        unsafe {
            let slot = &raw const (*r).slots[((s - 1) % RING_SLOTS) as usize];
            if (*slot).seq.load(Ordering::Acquire) != s {
                continue;
            }
            let clen = (ptr::read(ptr::addr_of!((*slot).chan_len)) as usize).min(CHAN_MAX);
            let plen = (ptr::read(ptr::addr_of!((*slot).payload_len)) as usize).min(PAYLOAD_MAX);
            let chan =
                std::slice::from_raw_parts(ptr::addr_of!((*slot).chan) as *const u8, clen).to_vec();
            let payload =
                std::slice::from_raw_parts(ptr::addr_of!((*slot).payload) as *const u8, plen)
                    .to_vec();
            if (*slot).seq.load(Ordering::Acquire) == s {
                out.push((chan, payload));
            }
        }
    }
    (out, w)
}

// --- PHP bridge -----------------------------------------------------------

use std::ffi::{c_char, c_int};

extern "C" fn c_broadcast(
    chan: *const c_char,
    clen: usize,
    payload: *const c_char,
    plen: usize,
) -> c_int {
    let chan = unsafe { std::slice::from_raw_parts(chan as *const u8, clen) };
    let payload = unsafe { std::slice::from_raw_parts(payload as *const u8, plen) };
    publish(chan, payload) as c_int
}

/// Register the broadcast callback with the shim for this process.
pub fn register_bridge() {
    if !enabled() {
        return;
    }
    unsafe { askr_php::broadcast_bridge::askr_php_set_broadcast_bridge(c_broadcast) };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_publish_read() {
        init();
        assert!(enabled());
        let start = current_seq();
        assert!(publish(b"orders", b"created:1"));
        assert!(publish(b"chat", b"hi"));
        let (events, _last) = read_from(start);
        assert_eq!(events.len(), 2);
        assert_eq!(events[0], (b"orders".to_vec(), b"created:1".to_vec()));
        assert_eq!(events[1], (b"chat".to_vec(), b"hi".to_vec()));
    }
}
