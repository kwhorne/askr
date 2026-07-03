//! A single worker process: one PHP interpreter, one small tokio runtime for
//! I/O, accepting on a listening socket that is **shared** with every other
//! worker (the master binds it once; workers inherit the fd across fork).
//!
//! All workers `accept()` on the same socket; the kernel hands each incoming
//! connection to exactly one of them. This is the classic prefork model — it
//! distributes load on Linux *and* macOS, unlike SO_REUSEPORT whose balancing
//! is Linux-only. This is the share-nothing seed: N independent
//! processes, no shared state, no locks on the hot path.

use std::net::{SocketAddr, TcpListener as StdListener};
use std::sync::Arc;

use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::TcpListener;

use crate::php::Php;
use crate::server::{self, Config};

/// Bind a listening socket once (in the master). SO_REUSEADDR only; the socket
/// itself is shared with workers by inheriting the fd across fork.
pub fn bind_listener(addr: SocketAddr) -> anyhow::Result<StdListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, Some(Protocol::TCP))?;
    socket.set_reuse_address(true)?;
    socket.bind(&addr.into())?;
    socket.listen(1024)?;
    Ok(socket.into())
}

/// Run the serve loop for one worker on an already-bound (shared) listener.
pub fn run_worker(
    listener: StdListener,
    config: Config,
    ini: Option<String>,
) -> anyhow::Result<()> {
    // Boot the interpreter (its own dedicated thread) before the runtime.
    // Worker mode boots the app once; otherwise each request runs fresh.
    let php = match config.worker_script.clone() {
        Some(script) => Php::spawn_worker(script, ini)?,
        None => Php::spawn(ini)?,
    };

    // Stagger recycling across workers (distinct pid per worker) so they don't
    // all recycle at the same instant and leave a gap with no live workers.
    let recycle_after = stagger(config.max_requests);

    // Build the TLS acceptor (if configured) once per worker.
    let tls = if config.tls_self_signed {
        let hosts = vec!["localhost".to_string(), config.listen.ip().to_string()];
        Some(crate::tls::self_signed(&hosts)?)
    } else {
        match (config.tls_cert.clone(), config.tls_key.clone()) {
            (Some(cert), Some(key)) => Some(crate::tls::acceptor(&cert, &key)?),
            _ => None,
        }
    };

    listener.set_nonblocking(true)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2) // I/O only; PHP runs on its own pinned thread
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let listener = TcpListener::from_std(listener)?;
        server::run(listener, Arc::new(config), php, recycle_after, tls).await
    })
}

/// Add a per-process jitter so workers recycle at different times.
fn stagger(max: usize) -> usize {
    if max == 0 {
        return 0;
    }
    let span = (max / 2).max(1);
    max + (std::process::id() as usize) % span
}
