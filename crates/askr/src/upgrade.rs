//! `askr upgrade` — self-update the release install (binary + bundled libphp).
//!
//! An Askr install is a *directory* (the release tarball: `askr` + `lib/` +
//! `examples/`), not a single file, so an upgrade swaps the whole prefix
//! atomically: extract next to it, then `rename` old aside and new into place.
//! The running server keeps its `mmap`'d libphp until it's restarted.
//!
//! Zero extra Rust deps: `curl` fetches (redirects + system TLS, exactly like the
//! documented install), `sha2` verifies the checksum, system `tar` extracts.

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "kwhorne/askr";

pub struct Options {
    pub check: bool,
    pub version: Option<String>,
    pub restart: bool,
}

pub fn run(opts: Options) -> Result<()> {
    let current = env!("CARGO_PKG_VERSION");

    let pinned = opts.version.is_some();
    let target = match &opts.version {
        Some(v) => v.trim_start_matches('v').to_string(),
        None => latest_version().context("could not determine the latest release")?,
    };

    println!("askr {current} → {target}");
    if !pinned && target == current {
        println!("✓ already on the latest release ({current}).");
        return Ok(());
    }
    if opts.check {
        if target == current {
            println!("✓ up to date.");
        } else {
            println!("↑ {target} is available — run `askr upgrade` to install it.");
        }
        return Ok(());
    }

    // Platform / environment guards.
    if !cfg!(target_os = "linux") {
        bail!("self-upgrade only ships for the Linux release; build from source on this platform.");
    }
    if in_container() {
        bail!(
            "running inside a container — upgrade by pulling a new image tag instead:\n    \
             docker pull ghcr.io/{REPO}:{target}"
        );
    }
    require_tool("curl")?;
    require_tool("tar")?;

    let arch = match std::env::consts::ARCH {
        a @ ("x86_64" | "aarch64") => a,
        other => bail!("no prebuilt release for this architecture ({other}); build from source."),
    };

    // Locate the install prefix (<prefix>/askr with a sibling <prefix>/lib).
    let prefix = install_prefix().context("could not locate the Askr install directory")?;
    let parent = prefix
        .parent()
        .context("install prefix has no parent")?
        .to_path_buf();
    ensure_writable(&parent)?;

    // Work on the same filesystem as the prefix so the final swap is an atomic rename.
    let work = parent.join(format!(".askr-upgrade-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&work);
    std::fs::create_dir_all(&work).with_context(|| format!("create {}", work.display()))?;
    let _cleanup = Cleanup(work.clone());

    let name = format!("askr-{target}-linux-{arch}");
    let base = format!("https://github.com/{REPO}/releases/download/v{target}");
    let tarball = work.join(format!("{name}.tar.gz"));
    let sumfile = work.join(format!("{name}.tar.gz.sha256"));

    println!("↓ downloading {name}.tar.gz …");
    download(&format!("{base}/{name}.tar.gz"), &tarball)?;
    download(&format!("{base}/{name}.tar.gz.sha256"), &sumfile)?;

    println!("· verifying sha256 …");
    verify_sha256(&tarball, &sumfile)?;

    println!("· extracting …");
    let status = Command::new("tar")
        .arg("xzf")
        .arg(&tarball)
        .arg("-C")
        .arg(&work)
        .status()
        .context("running tar")?;
    if !status.success() {
        bail!("tar extraction failed");
    }
    let extracted = work.join(&name);
    if !extracted.join("askr").is_file() {
        bail!(
            "unexpected release layout: {}/askr missing",
            extracted.display()
        );
    }
    make_executable(&extracted.join("askr"))?;
    let runsh = extracted.join("askr-run.sh");
    if runsh.is_file() {
        make_executable(&runsh)?;
    }

    // Atomic swap, keeping the previous install for rollback.
    let backup = parent.join("askr.old");
    let _ = std::fs::remove_dir_all(&backup);
    std::fs::rename(&prefix, &backup)
        .with_context(|| format!("move {} aside", prefix.display()))?;
    if let Err(e) = std::fs::rename(&extracted, &prefix) {
        let _ = std::fs::rename(&backup, &prefix); // roll back
        return Err(anyhow::anyhow!("install failed, rolled back: {e}"));
    }

    println!(
        "✓ upgraded to {target}. Previous version kept at {} (rollback: `askr upgrade --version {current}`).",
        backup.display()
    );

    if opts.restart {
        println!("↻ systemctl restart askr …");
        match Command::new("systemctl")
            .arg("restart")
            .arg("askr")
            .status()
        {
            Ok(s) if s.success() => println!("✓ service restarted."),
            _ => println!("! automatic restart failed — run: sudo systemctl restart askr"),
        }
    } else {
        println!("→ restart to load it:  sudo systemctl restart askr");
    }
    Ok(())
}

/// Resolve the latest version by following the `/releases/latest` redirect to
/// `/releases/tag/vX.Y.Z` — no API token, User-Agent, or rate limit involved.
fn latest_version() -> Result<String> {
    let out = Command::new("curl")
        .args([
            "-sIL",
            "--retry",
            "3",
            "--connect-timeout",
            "20",
            "-o",
            "/dev/null",
            "-w",
            "%{url_effective}",
            &format!("https://github.com/{REPO}/releases/latest"),
        ])
        .output()
        .context("running curl")?;
    if !out.status.success() {
        bail!("curl failed while checking the latest release");
    }
    let url = String::from_utf8_lossy(&out.stdout);
    let tag = url.trim().rsplit('/').next().unwrap_or("").trim();
    let v = tag.trim_start_matches('v');
    if v.is_empty() || !v.chars().next().unwrap_or('x').is_ascii_digit() {
        bail!("could not parse a version from {url:?}");
    }
    Ok(v.to_string())
}

fn download(url: &str, dest: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args([
            "-fSL",
            "--no-progress-meter",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--retry",
            "3",
            "--connect-timeout",
            "20",
            "-o",
        ])
        .arg(dest)
        .arg(url)
        .status()
        .context("running curl")?;
    if !status.success() {
        bail!("download failed: {url}");
    }
    Ok(())
}

fn verify_sha256(file: &Path, sumfile: &Path) -> Result<()> {
    let want = std::fs::read_to_string(sumfile)?;
    let want = want.split_whitespace().next().unwrap_or("").to_lowercase();
    if want.len() != 64 {
        bail!("malformed checksum file");
    }
    let mut f = std::fs::File::open(file)?;
    let mut hasher = Sha256::new();
    std::io::copy(&mut f, &mut hasher)?;
    let mut got = String::with_capacity(64);
    for b in hasher.finalize() {
        let _ = write!(got, "{b:02x}");
    }
    if got != want {
        bail!("checksum mismatch!\n  expected {want}\n  got      {got}");
    }
    Ok(())
}

fn install_prefix() -> Result<PathBuf> {
    let exe = std::fs::canonicalize(std::env::current_exe()?)?;
    let dir = exe.parent().context("exe has no parent")?.to_path_buf();
    if !dir.join("lib").is_dir() {
        bail!(
            "{} has no lib/ — this doesn't look like a release install; \
             self-upgrade only works on the release tarball layout.",
            dir.display()
        );
    }
    Ok(dir)
}

fn ensure_writable(dir: &Path) -> Result<()> {
    let probe = dir.join(format!(".askr-write-test-{}", std::process::id()));
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            Ok(())
        }
        Err(_) => bail!("{} is not writable — re-run with sudo.", dir.display()),
    }
}

fn require_tool(name: &str) -> Result<()> {
    let ok = Command::new(name)
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    if !ok {
        bail!("`{name}` is required for upgrade but was not found on PATH.");
    }
    Ok(())
}

fn in_container() -> bool {
    Path::new("/.dockerenv").exists() || Path::new("/run/.containerenv").exists()
}

fn make_executable(p: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = std::fs::metadata(p)?.permissions();
        perm.set_mode(0o755);
        std::fs::set_permissions(p, perm)?;
    }
    Ok(())
}

struct Cleanup(PathBuf);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
