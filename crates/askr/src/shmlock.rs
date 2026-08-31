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
//! acquire within a spin budget, we look at who holds it: we steal from a holder the
//! kernel confirms is dead (`kill(pid, 0)` → `ESRCH`). A live but preempted holder is
//! waited on with backoff, so a mid-copy holder is never interrupted.
//!
//! `kill(pid, 0)` answers "does a process with this number exist", which is not quite
//! the question asked. A holder can die while `pid_max` wraps and the number be reused
//! by something long-lived — after which every waiter sees a live holder that will
//! never release, and the region wedges for good. So there is a second, far slower
//! condition: a holder whose PID has not changed for `STUCK_LIMIT` (10 s) is stolen
//! from as well, with an error logged. That is the same steal this module was written
//! to remove, four orders of magnitude further out — a bounded stall instead of a
//! permanent hang, and never the sub-millisecond theft that corrupted state.

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
    // The holder we have been waiting on, and since when. Local state, so this needs
    // no extra word in the slot and works the same on Linux and macOS.
    let mut watched: Option<(u32, std::time::Instant)> = None;
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

        // A live-looking holder may be a *different* process that inherited the PID.
        //
        // `kill(pid, 0)` answers "does a process with this number exist", which is not
        // the question. The holder can die while pid_max wraps — minutes on a
        // fork-heavy box — and the number be handed to something long-lived. From then
        // on every waiter sees a live holder that will never release, and the wait
        // below is unbounded: the region wedges permanently, with no log to say why.
        //
        // So a holder that does not move for STUCK_LIMIT is stolen from regardless of
        // what `kill` says. This is the same steal the module header argues against,
        // and the difference is the timescale: the old scheme stole after 100–200 µs,
        // shorter than a scheduler slice, which is why it corrupted state. A holder
        // copying at most 64 KB that has not finished in ten seconds is not preempted,
        // it is gone — and a bounded stall with a loud log beats a permanent hang.
        match watched {
            Some((p, since)) if p == holder => {
                if since.elapsed() >= STUCK_LIMIT {
                    if lock
                        .compare_exchange(holder, me, Ordering::Acquire, Ordering::Relaxed)
                        .is_ok()
                    {
                        tracing::error!(
                            holder,
                            waited_secs = STUCK_LIMIT.as_secs(),
                            "shared-memory slot lock stolen from a holder the kernel \
                             still reports as live — most likely the original holder \
                             died and its PID was reused. If this repeats, the region \
                             may be corrupt; restart the server."
                        );
                        return;
                    }
                    watched = None;
                    continue;
                }
            }
            _ => watched = Some((holder, std::time::Instant::now())),
        }

        // Live holder, almost certainly preempted mid-copy. Prefer *yielding*
        // (which keeps the thread runnable, unlike a sleep that parks it and — on
        // a Tokio worker — stalls that reactor) for a good while: a holder copying
        // a ≤64 KB value resumes within microseconds, so most contention clears
        // without ever sleeping. Only a genuinely stuck holder reaches the bounded
        // sleep, which stays small (10 µs → 200 µs cap) so we neither burn a core
        // nor park a Tokio worker for long.
        idle_rounds += 1;
        if idle_rounds < 64 {
            std::thread::yield_now();
        } else {
            let shift = (idle_rounds - 64).min(5);
            let us = (10u64 << shift).min(200);
            std::thread::sleep(std::time::Duration::from_micros(us));
        }
    }
}

/// How long a holder may sit unchanged before a waiter takes the lock anyway.
///
/// Four orders of magnitude above any legitimate critical section here (a ≤64 KB copy
/// under a slot lock), so reaching it means the holder is not coming back.
const STUCK_LIMIT: std::time::Duration = std::time::Duration::from_secs(10);

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
