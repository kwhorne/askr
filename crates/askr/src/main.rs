//! Askr — a standalone, memory-safe PHP application server.
//!
//! A1: serve a single PHP application over HTTP through the embedded interpreter.

mod cgi;
mod php;
mod server;

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::php::Php;
use crate::server::Config;

#[derive(Parser)]
#[command(name = "askr", version, about = "The smartest, most efficient PHP web server.")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve a PHP application over HTTP.
    Serve {
        /// Document root (the app's public/ directory). Defaults to ./public
        /// if present, otherwise the current directory.
        #[arg(long)]
        root: Option<PathBuf>,

        /// Front controller, relative to the document root.
        #[arg(long, default_value = "index.php")]
        front: PathBuf,

        /// Address to listen on.
        #[arg(long, default_value = "127.0.0.1:8000")]
        listen: SocketAddr,

        /// Mark requests as HTTPS in $_SERVER (when behind a TLS terminator).
        #[arg(long)]
        https: bool,

        /// Extra php.ini lines, e.g. to load opcache. Overrides $ASKR_PHP_INI.
        #[arg(long)]
        ini: Option<String>,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "askr=info".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            root,
            front,
            listen,
            https,
            ini,
        } => {
            let docroot = resolve_root(root)?;
            let script = docroot.join(&front);
            if !script.is_file() {
                anyhow::bail!(
                    "front controller not found: {} (use --root / --front)",
                    script.display()
                );
            }

            let ini = ini.or_else(|| std::env::var("ASKR_PHP_INI").ok());
            let php = Php::spawn(ini)?;

            server::run(
                Config {
                    docroot,
                    front_controller: front,
                    listen,
                    https,
                },
                php,
            )
            .await
        }
    }
}

fn resolve_root(root: Option<PathBuf>) -> anyhow::Result<PathBuf> {
    let root = match root {
        Some(r) => r,
        None => {
            let public = PathBuf::from("public");
            if public.is_dir() {
                public
            } else {
                PathBuf::from(".")
            }
        }
    };
    let canonical = std::fs::canonicalize(&root)
        .map_err(|e| anyhow::anyhow!("bad --root {}: {e}", root.display()))?;
    Ok(canonical)
}
