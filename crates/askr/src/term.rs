//! SIGTERM for a worker, through a pipe the worker made itself.
//!
//! tokio delivers Unix signals through one self-pipe per *process*, created the first
//! time anything builds a runtime with a signal driver, and kept for the life of the
//! process. A worker is forked from the master — and the master builds such runtimes
//! after its first round of forks (the admin plane), and with `--acme` before it (the
//! certificate request). Every worker forked after that inherits the master's pipe,
//! shared with the master and with every sibling forked since. The SIGTERM handler
//! writes its wake-up byte into that shared pipe, and whichever process reads first
//! takes it: often not the worker the signal was for. That worker never drained.
//!
//! Seen as: a graceful stop that hung until the master was killed, and a reload that
//! rolled some workers and not others — the ones it missed kept serving the previous
//! release, and nothing said the reload had stalled. With the admin plane being polled
//! through three reloads of four workers, that happened in every one of six runs; with
//! `--acme`, whose runtime is built before the first fork, every worker is exposed.
//!
//! So the worker does not use tokio's signal handling for SIGTERM. It makes its own pipe
//! after the fork, installs a handler that writes to it, and waits on it like any other
//! file. Nothing else has that pipe, so nothing else can take the wake-up.

use std::os::fd::{FromRawFd, OwnedFd};
use std::sync::atomic::{AtomicI32, Ordering};

/// Write end of this process's SIGTERM pipe, for the handler. -1 until [`install`].
static WRITE_FD: AtomicI32 = AtomicI32::new(-1);

extern "C" fn on_term(_sig: libc::c_int) {
    // Async-signal-safe: an atomic load and write(2). A full pipe drops the byte, which
    // is fine — one byte in the pipe already means "terminate".
    let fd = WRITE_FD.load(Ordering::Relaxed);
    if fd >= 0 {
        let b = b"t";
        unsafe { libc::write(fd, b.as_ptr() as *const libc::c_void, 1) };
    }
}

/// A stream of SIGTERMs for this process. Call once, in the worker, after the fork and
/// inside its tokio runtime.
pub struct Term {
    rx: tokio::net::unix::pipe::Receiver,
}

impl Term {
    /// Make the pipe and take over SIGTERM.
    pub fn install() -> std::io::Result<Term> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: a fresh pipe; both ends are owned below.
        if unsafe { libc::pipe(fds.as_mut_ptr()) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        for fd in fds {
            // Non-blocking (the handler must never block, and tokio needs it), and
            // close-on-exec (a child the worker starts must not hold it either).
            unsafe {
                let fl = libc::fcntl(fd, libc::F_GETFL);
                libc::fcntl(fd, libc::F_SETFL, fl | libc::O_NONBLOCK);
                libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
            }
        }
        // SAFETY: owned from here on.
        let read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        // The write end is deliberately leaked to the handler for the life of the process.
        WRITE_FD.store(fds[1], Ordering::SeqCst);
        // SAFETY: installing a handler that only does async-signal-safe work.
        unsafe {
            let mut sa: libc::sigaction = std::mem::zeroed();
            sa.sa_sigaction = on_term as *const () as libc::sighandler_t;
            sa.sa_flags = libc::SA_RESTART;
            libc::sigemptyset(&mut sa.sa_mask);
            if libc::sigaction(libc::SIGTERM, &sa, std::ptr::null_mut()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
        }
        Ok(Term {
            rx: tokio::net::unix::pipe::Receiver::from_owned_fd(read)?,
        })
    }

    /// Resolve when a SIGTERM has arrived (immediately, if one already has).
    pub async fn recv(&mut self) {
        let mut buf = [0u8; 64];
        loop {
            if self.rx.readable().await.is_err() {
                // The pipe failing is not a reason to stop serving; nor to spin.
                std::future::pending::<()>().await;
            }
            match self.rx.try_read(&mut buf) {
                Ok(n) if n > 0 => return,
                Ok(_) => std::future::pending::<()>().await,
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
                Err(_) => std::future::pending::<()>().await,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    /// The bug this module exists for, in miniature and deterministic: a process whose
    /// tokio signal pipe was made before the fork shares it with the child, so the
    /// child's SIGTERM wake-up can be read by the parent — and the child never hears.
    /// With [`super::Term`], made after the fork, the child hears its own signal however
    /// busy the parent is reading.
    ///
    /// Forks, so it runs the child in a fresh process and checks only exit statuses.
    #[test]
    fn a_forked_worker_hears_its_own_sigterm_even_when_the_parent_has_a_signal_pipe() {
        // The parent builds a runtime with a signal driver and keeps reading SIGTERMs —
        // the master's admin plane, in miniature. It ignores what it reads.
        let parent_rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let _ = parent_rt.block_on(async {
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        });
        let mut parent_sig = parent_rt
            .block_on(async {
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            })
            .unwrap();
        parent_rt.spawn(async move { while parent_sig.recv().await.is_some() {} });

        // Like the fleet: several workers waiting at once, all signalled together — the
        // master's stop. With a shared pipe one process can read its siblings' wake-ups.
        let (rounds, children) = (10, 8);
        let mut heard = 0;
        for _ in 0..rounds {
            let mut pids = Vec::new();
            for _ in 0..children {
                // The child says "listening" over a pipe of its own, so the parent knows
                // when to signal it without any signal of its own.
                let mut ready = [0 as libc::c_int; 2];
                assert_eq!(unsafe { libc::pipe(ready.as_mut_ptr()) }, 0);
                // SAFETY: the child only builds a runtime, waits for one signal and exits.
                match unsafe { libc::fork() } {
                    0 => {
                        let ok = std::panic::catch_unwind(|| {
                            let rt = tokio::runtime::Builder::new_current_thread()
                                .enable_io()
                                .enable_time()
                                .build()
                                .unwrap();
                            rt.block_on(async {
                                let mut t = super::Term::install().unwrap();
                                unsafe { libc::write(ready[1], b"r".as_ptr() as *const _, 1) };
                                tokio::time::timeout(std::time::Duration::from_secs(3), t.recv())
                                    .await
                                    .is_ok()
                            })
                        });
                        unsafe { libc::_exit(if matches!(ok, Ok(true)) { 0 } else { 1 }) };
                    }
                    pid => unsafe {
                        libc::close(ready[1]);
                        let mut b = [0u8; 1];
                        libc::read(ready[0], b.as_mut_ptr() as *mut _, 1);
                        libc::close(ready[0]);
                        pids.push(pid);
                    },
                }
            }
            for pid in &pids {
                unsafe { libc::kill(*pid, libc::SIGTERM) };
            }
            for pid in pids {
                let mut status = 0;
                unsafe { libc::waitpid(pid, &mut status, 0) };
                if libc::WIFEXITED(status) && libc::WEXITSTATUS(status) == 0 {
                    heard += 1;
                }
            }
        }
        let rounds = rounds * children;
        assert_eq!(heard, rounds, "every child heard its own SIGTERM");
    }
}
