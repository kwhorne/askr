//! Typed, declarative configuration (`askr.toml`).
//!
//! A config file is the source of truth a GUI / admin tooling edits. It mirrors
//! the `serve` flags. `askr config check <file>` validates and prints the
//! resolved settings without starting the server.

use std::net::SocketAddr;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::Deserialize;

use crate::server::Config;

/// The on-disk config file (`askr.toml`).
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    #[serde(default)]
    pub server: ServerSection,
    #[serde(default)]
    pub worker: WorkerSection,
    #[serde(default)]
    pub tls: TlsSection,
    #[serde(default)]
    pub admin: AdminSection,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerSection {
    /// Address to listen on, e.g. "0.0.0.0:8000".
    pub listen: String,
    /// Document root (the app's public/ directory).
    pub root: PathBuf,
    /// Front controller, relative to the root.
    #[serde(default = "default_front")]
    pub front: String,
    /// Worker processes: a number, or "auto" (= CPU cores).
    #[serde(default = "default_workers")]
    pub workers: String,
    /// Recycle each worker after this many requests (0 = never).
    #[serde(default)]
    pub max_requests: usize,
    /// Max request body size, e.g. "16M".
    #[serde(default = "default_body")]
    pub max_body_size: String,
    /// Mark requests as HTTPS in $_SERVER (e.g. behind a TLS terminator).
    #[serde(default)]
    pub https: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSection {
    /// Worker script — boot the app once and serve many (Octane model).
    pub script: Option<PathBuf>,
    /// Application base path, exported as $ASKR_APP_BASE for the worker script.
    pub app_base: Option<PathBuf>,
    /// Extra php.ini lines (e.g. to load opcache).
    pub ini: Option<String>,
    /// Dev only: detect state bleed between requests (expensive; worker mode).
    #[serde(default)]
    pub paranoid: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsSection {
    pub cert: Option<PathBuf>,
    pub key: Option<PathBuf>,
    #[serde(default)]
    pub self_signed: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AdminSection {
    /// Admin dashboard/API listen address (e.g. "127.0.0.1:9000"). Off if unset.
    pub listen: Option<String>,
}

impl Default for ServerSection {
    fn default() -> Self {
        ServerSection {
            listen: "127.0.0.1:8000".into(),
            root: PathBuf::from("public"),
            front: default_front(),
            workers: default_workers(),
            max_requests: 0,
            max_body_size: default_body(),
            https: false,
        }
    }
}

fn default_front() -> String {
    "index.php".into()
}
fn default_workers() -> String {
    "auto".into()
}
fn default_body() -> String {
    "16M".into()
}

/// The fully-resolved runtime configuration produced from a file.
pub struct Resolved {
    pub config: Config,
    pub workers: usize,
    pub ini: Option<String>,
    pub app_base: Option<PathBuf>,
    pub paranoid: bool,
    pub admin_listen: Option<SocketAddr>,
}

impl FileConfig {
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        let cfg: FileConfig =
            toml::from_str(&text).with_context(|| format!("parsing config {}", path.display()))?;
        Ok(cfg)
    }

    /// Validate and resolve into a runtime [`Config`], checking that paths and
    /// certificates actually exist.
    pub fn resolve(self, cpus: usize) -> Result<Resolved> {
        let listen: SocketAddr = self
            .server
            .listen
            .parse()
            .with_context(|| format!("invalid server.listen {:?}", self.server.listen))?;

        let docroot = std::fs::canonicalize(&self.server.root)
            .with_context(|| format!("server.root {} not found", self.server.root.display()))?;

        let front = PathBuf::from(&self.server.front);
        anyhow::ensure!(
            docroot.join(&front).is_file(),
            "front controller not found: {}",
            docroot.join(&front).display()
        );

        let workers = match self.server.workers.as_str() {
            "auto" => cpus.max(1),
            n => n
                .parse::<usize>()
                .with_context(|| format!("invalid server.workers {:?}", self.server.workers))?
                .max(1),
        };

        let max_body_size = crate::parse_size(&self.server.max_body_size)?;

        if let Some(script) = &self.worker.script {
            anyhow::ensure!(
                script.is_file(),
                "worker.script not found: {}",
                script.display()
            );
        }
        if let Some(base) = &self.worker.app_base {
            anyhow::ensure!(
                base.is_dir(),
                "worker.app_base not found: {}",
                base.display()
            );
        }

        // TLS validation.
        let tls_self_signed = self.tls.self_signed;
        match (&self.tls.cert, &self.tls.key) {
            (Some(c), Some(k)) => {
                anyhow::ensure!(c.is_file(), "tls.cert not found: {}", c.display());
                anyhow::ensure!(k.is_file(), "tls.key not found: {}", k.display());
                anyhow::ensure!(
                    !tls_self_signed,
                    "set either tls.self_signed or tls.cert/key, not both"
                );
            }
            (None, None) => {}
            _ => anyhow::bail!("tls.cert and tls.key must both be set"),
        }
        let tls_on = self.tls.cert.is_some() || tls_self_signed;

        let admin_listen = match &self.admin.listen {
            Some(a) => Some(
                a.parse::<SocketAddr>()
                    .with_context(|| format!("invalid admin.listen {a:?}"))?,
            ),
            None => None,
        };

        Ok(Resolved {
            config: Config {
                docroot,
                front_controller: front,
                listen,
                https: self.server.https || tls_on,
                worker_script: self.worker.script,
                max_requests: self.server.max_requests,
                tls_cert: self.tls.cert,
                tls_key: self.tls.key,
                tls_self_signed,
                max_body_size,
            },
            workers,
            ini: self.worker.ini,
            app_base: self.worker.app_base,
            paranoid: self.worker.paranoid,
            admin_listen,
        })
    }
}
