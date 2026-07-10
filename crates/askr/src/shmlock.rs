//! Robust cross-process spinlock for shared-memory slots.
//!
//! The old scheme spun a fixed number of iterations and then *unconditionally*
//! stole the lock — but 50 000 spins is ~100–200 µs, far shorter than a
//! scheduler time slice (10–100 ms). A holder that was merely preempted (or was
//! mid-copy of a 64 KB value) would have its lock stolen, letting two processes
//! into the same critical section: a data race and shared-memory corruption
//! (lost/garbled sessions, cache, queue jobs).
//!
//! This lock stores the **holder's PID** in the slot (0 = free). If we can't
//! acquire within a spin budget, we look at who holds it: we steal *only* from a
//! holder the kernel confirms is dead (`kill(pid, 0)` → `ESRCH`). A live but
//! preempted holder is waited on (with backoff), so we never corrupt shared
//! state — the worst case degrades to a short wait, never UB.

use std::sync::atomic::{AtomicU32, Ordering};

#[inline]
fn my_pid() -> u32 {
    // getpid(2) cannot fail.
    (unsafe { libc::getpid() }) as u32
}

/// Is `pid` still a live process? `kill(pid, 0)` sends no signal but performs
/// the permission/existence check: 0 → alive; `ESRCH` → gone; `EPERM` → exists
/// (owned by another user), so still alive.
#[inline]
fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// Acquire the slot lock, recording our PID. Blocks until held. Steals only from
/// a dead holder; waits (yield → short sleep) on a live one.
#[inline]
pub fn acquire(lock: &AtomicU32) {
    let me = my_pid();
    let mut idle_rounds: u32 = 0;
    loop {
        // Fast path: a bounded spin for an uncontended / briefly-held lock.
        for _ in 0..40_000 {
            if lock
                .compare_exchange(0, me, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            std::hint::spin_loop();
        }

        // Contended for a while — inspect the current holder.
        let holder = lock.load(Ordering::Relaxed);
        if holder == 0 {
            continue; // just freed; retry the fast path immediately
        }
        if !process_alive(holder) {
            // Steal, but CAS on the *exact* dead holder so we lose the race
            // cleanly if another process already reclaimed the slot.
            if lock
                .compare_exchange(holder, me, Ordering::Acquire, Ordering::Relaxed)
                .is_ok()
            {
                return;
            }
            continue;
        }

        // Live holder, almost certainly preempted mid-copy. Back off rather than
        // corrupt: yield a few times, then sleep briefly to bound CPU.
        idle_rounds += 1;
        if idle_rounds < 8 {
            std::thread::yield_now();
        } else {
            std::thread::sleep(std::time::Duration::from_micros(100));
        }
    }
}

/// Release a lock we hold.
#[inline]
pub fn release(lock: &AtomicU32) {
    lock.store(0, Ordering::Release);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_alive_self_and_zero() {
        assert!(process_alive(my_pid()));
        assert!(!process_alive(0));
    }

    #[test]
    fn acquire_release_roundtrip() {
        let lock = AtomicU32::new(0);
        acquire(&lock);
        assert_eq!(lock.load(Ordering::Relaxed), my_pid());
        release(&lock);
        assert_eq!(lock.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn steals_only_from_dead_holder() {
        // Fork a child that exits immediately (async-signal-safe), then reap it
        // so its PID is genuinely dead.
        let child = unsafe { libc::fork() };
        if child == 0 {
            unsafe { libc::_exit(0) };
        }
        assert!(child > 0, "fork failed");
        let mut st: libc::c_int = 0;
        unsafe { libc::waitpid(child, &mut st, 0) };

        // Simulate the dead child holding the slot lock: acquire must steal it
        // (and record our pid), not hang waiting on a corpse.
        assert!(!process_alive(child as u32));
        let lock = AtomicU32::new(child as u32);
        acquire(&lock);
        assert_eq!(lock.load(Ordering::Relaxed), my_pid());
        release(&lock);
    }
}
