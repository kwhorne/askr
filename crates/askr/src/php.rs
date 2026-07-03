//! The PHP execution worker.
//!
//! A non-ZTS interpreter is a per-thread, single-instance thing: it must be
//! created on, and only ever touched by, one OS thread. So we pin an
//! [`askr_php::Interpreter`] to a dedicated thread and feed it requests over a
//! channel. tokio owns the sockets; this thread owns PHP.
//!
//! One interpreter handles one request at a time (exactly like an FPM worker).
//! Cross-core concurrency is a later milestone: fork one such process per core
//! (SO_REUSEPORT), each with its own interpreter and CoW-shared warm heap.

use std::thread;

use tokio::sync::{mpsc, oneshot};

use askr_php::{Interpreter, Request, Response};

struct Job {
    req: Request,
    reply: oneshot::Sender<Result<Response, String>>,
}

/// A handle to the pinned PHP interpreter thread. Cheap to clone.
#[derive(Clone)]
pub struct Php {
    tx: mpsc::Sender<Job>,
}

impl Php {
    /// Boot an interpreter on a dedicated thread. `ini` is appended to the
    /// engine's INI (e.g. to load opcache) via `$ASKR_PHP_INI`.
    pub fn spawn(ini: Option<String>) -> anyhow::Result<Self> {
        let (tx, mut rx) = mpsc::channel::<Job>(1024);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

        thread::Builder::new()
            .name("askr-php".into())
            .spawn(move || {
                if let Some(ini) = ini {
                    // SAFETY: set before the interpreter boots, single-threaded here.
                    std::env::set_var("ASKR_PHP_INI", ini);
                }
                let mut php = match Interpreter::new() {
                    Ok(p) => {
                        let _ = ready_tx.send(Ok(()));
                        p
                    }
                    Err(e) => {
                        let _ = ready_tx.send(Err(e.to_string()));
                        return;
                    }
                };
                tracing::info!(version = %php.php_version(), "embedded PHP ready");

                while let Some(job) = rx.blocking_recv() {
                    let res = php.handle(&job.req).map_err(|e| e.to_string());
                    let _ = job.reply.send(res);
                }
            })?;

        ready_rx
            .recv()
            .map_err(|_| anyhow::anyhow!("php thread died during startup"))?
            .map_err(|e| anyhow::anyhow!("php_embed_init failed: {e}"))?;

        Ok(Php { tx })
    }

    /// Run one request through the interpreter.
    pub async fn handle(&self, req: Request) -> Result<Response, String> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job { req, reply })
            .await
            .map_err(|_| "php worker unavailable".to_string())?;
        rx.await.map_err(|_| "php worker dropped reply".to_string())?
    }
}
