//! A single worker process: one PHP interpreter, one small tokio runtime for
//! I/O, accepting on a listening socket that is **shared** with every other
//! worker (the master binds it once; workers inherit the fd across fork).
//!
//! All workers `accept()` on the same socket; the kernel hands each incoming
//! connection to exactly one of them. This is the classic prefork model — it
//! distributes load on Linux *and* macOS, unlike SO_REUSEPORT whose balancing
//! is Linux-only. This is the share-nothing seed (PRD §5.1): N independent
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
pub fn run_worker(listener: StdListener, config: Config, ini: Option<String>) -> anyhow::Result<()> {
    // Boot the interpreter (its own dedicated thread) before the runtime.
    // Worker mode boots the app once; otherwise each request runs fresh.
    let php = match config.worker_script.clone() {
        Some(script) => Php::spawn_worker(script, ini)?,
        None => Php::spawn(ini)?,
    };

    listener.set_nonblocking(true)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2) // I/O only; PHP runs on its own pinned thread
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let listener = TcpListener::from_std(listener)?;
        server::run(listener, Arc::new(config), php).await
    })
}
