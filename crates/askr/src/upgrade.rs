//! `askr upgrade` — self-update the release install (binary + bundled libphp).
//!
//! An Askr install is a *directory* (the release tarball: `askr` + `lib/` +
//! `examples/`), not a single file, so an upgrade swaps the whole prefix
//! atomically: extract next to it, then `rename` old aside and new into place.
//! The running server keeps its `mmap`'d libphp until it's restarted.
//!
//! `curl` fetches (redirects + system TLS, exactly like the documented install),
//! `sha2` verifies the checksum, `minisign-verify` verifies the signature, system
//! `tar` extracts.

use anyhow::{bail, Context, Result};
use minisign_verify::{PublicKey, Signature};
use sha2::{Digest, Sha256};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

const REPO: &str = "kwhorne/askr";

/// The release signing key, embedded at build time.
///
/// A file in the repository rather than something fetched at runtime, because that is
/// the whole point: an attacker who wants to change what `askr upgrade` trusts has to
/// change the source and get it built and released, not just serve a different file.
const RELEASE_PUBKEY: &str = include_str!("../../../keys/release.pub");

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

    // The checksum and the tarball come from the same release, so it proves the
    // download arrived intact and nothing about who produced it: a compromised
    // release, account or CI token serves a matching pair. The signature is the part
    // that answers provenance, and this runs as root — so a bad one is a refusal, not
    // a warning.
    match release_key() {
        Some(key) => {
            let sigfile = work.join(format!("{name}.tar.gz.minisig"));
            println!("· verifying signature …");
            download(&format!("{base}/{name}.tar.gz.minisig"), &sigfile).context(
                "no signature published for this release (or it could not be fetched) — \
                 this build requires one, so it will not be installed",
            )?;
            verify_signature(&tarball, &sigfile, &key)?;
            println!("✓ signed by the key this build trusts");
        }
        None => {
            println!(
                "! NO SIGNING KEY in this build: the download was checked against its own \
                 checksum and nothing more. That proves it arrived intact, not who made \
                 it. See docs/RELEASING.md."
            );
        }
    }

    println!("· extracting …");
    // `--no-same-owner` / `--no-same-permissions` are the whole security of this step.
    // Extraction runs as root (the install prefix is root-owned, so `askr upgrade`
    // needs sudo), and GNU tar as root restores the *archive's* uid, gid and mode bits
    // rather than the extracting user's. The release tarball is built by a CI runner,
    // so the recorded owner is that runner's uid — commonly 1001. Restored verbatim,
    // /opt/askr/askr ends up owned by whichever local account happens to hold uid 1001
    // on this machine, and that account can then rewrite the binary systemd starts as
    // root. The permissions half is the same hazard by a different route: a mode
    // recorded as world-writable, or a setuid bit, would be reproduced faithfully.
    // No chown afterwards: with --no-same-owner tar creates everything as the
    // effective uid, which is already root.
    let status = Command::new("tar")
        .arg("--no-same-owner")
        .arg("--no-same-permissions")
        .arg("-xzf")
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

/// The embedded public key, or `None` while no key has been configured.
///
/// Absent has to stay possible: a build with no key must not be a build that cannot
/// upgrade, and this shipped unsigned for a long time. It says so on every upgrade
/// instead.
fn release_key() -> Option<PublicKey> {
    parse_key(RELEASE_PUBKEY)
}

/// Pull a minisign public key out of a `.pub` file's text.
///
/// Split out from [`release_key`] so both outcomes are testable without depending on
/// what is committed today: a malformed key and a deliberately unconfigured file must
/// be told apart, and only one of them is a bug.
fn parse_key(text: &str) -> Option<PublicKey> {
    // minisign public keys are base64 of a 42-byte structure beginning with the
    // two-byte algorithm tag, which always renders as "RW". Comment lines cannot match.
    text.lines()
        .map(str::trim)
        .find(|l| l.starts_with("RW") && l.len() >= 40)
        .and_then(|l| PublicKey::from_base64(l).ok())
}

/// Verify the tarball against its minisign signature, streamed.
///
/// Prehashed signatures only. A legacy minisign signature covers the raw bytes, which
/// would mean holding the whole tarball in memory to check one; modern minisign
/// produces prehashed by default, so refusing legacy costs nothing and removes a mode
/// nobody should still be using.
fn verify_signature(file: &Path, sigfile: &Path, key: &PublicKey) -> Result<()> {
    let text = std::fs::read_to_string(sigfile).context("reading the signature file")?;
    let sig =
        Signature::decode(&text).map_err(|e| anyhow::anyhow!("malformed signature file: {e}"))?;
    let mut verifier = key.verify_stream(&sig).map_err(|e| {
        anyhow::anyhow!(
            "signature rejected before the tarball was even read ({e}) — it was made by \
             a different key, or in minisign's legacy non-prehashed mode"
        )
    })?;
    let mut f = std::fs::File::open(file)?;
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        verifier.update(&buf[..n]);
    }
    verifier.finalize().map_err(|e| {
        anyhow::anyhow!(
            "SIGNATURE VERIFICATION FAILED ({e}). This tarball was not signed by the key \
             this build trusts. Refusing to install it."
        )
    })?;
    Ok(())
}

fn verify_sha256(file: &Path, sumfile: &Path) -> Result<()> {
    let want = std::fs::read_to_string(sumfile)?;
    let want = want.split_whitespace().next().unwrap_or("").to_lowercase();
    if want.len() != 64 {
        bail!("malformed checksum file");
    }
    // Hashed in chunks rather than via `io::copy`: from digest 0.11 a hasher is no
    // longer an `io::Write`, and a release tarball shouldn't be read into memory
    // whole just to checksum it.
    let mut f = std::fs::File::open(file)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> std::path::PathBuf {
        let n = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("askr-up-{name}-{n}"));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The published minisign test vector: a prehashed signature over the four bytes
    /// `test`, from minisign-verify's own test suite. Using a real vector rather than a
    /// round trip means this test would catch the format being mis-parsed, not just
    /// our own code agreeing with itself.
    const VECTOR_PUBKEY: &str = "RWQf6LRCGA9i53mlYecO4IzT51TGPpvWucNSCh1CBM0QTaLn73Y7GFO3";
    const VECTOR_SIG: &str = "untrusted comment: signature from minisign secret key
RUQf6LRCGA9i559r3g7V1qNyJDApGip8MfqcadIgT9CuhV3EMhHoN1mGTkUidF/z7SrlQgXdy8ofjb7bNJJylDOocrCo8KLzZwo=
trusted comment: timestamp:1556193335\tfile:test
y/rUw2y8/hOUYjZU71eHp/Wo1KZ40fGy2VJEDl34XMJM+TX48Ss/17u3IvIfbVR1FkZZSNCisQbuQY+bHwhEBg==";

    /// `askr upgrade` runs as root and replaces the binary systemd starts. A signature
    /// that verifies has to mean the bytes are the ones that were signed, and a
    /// tampered tarball has to be a refusal — not a warning, not a fallback to the
    /// checksum that travels with it.
    #[test]
    fn a_tampered_tarball_is_refused_and_an_intact_one_is_not() {
        let d = tmp("sig");
        let key = PublicKey::from_base64(VECTOR_PUBKEY).expect("the vector key parses");
        let sig = d.join("payload.minisig");
        std::fs::write(&sig, VECTOR_SIG).unwrap();

        let good = d.join("payload");
        std::fs::write(&good, b"test").unwrap();
        verify_signature(&good, &sig, &key).expect("the signed bytes must verify");

        // One byte different is the whole point.
        let bad = d.join("payload.tampered");
        std::fs::write(&bad, b"Test").unwrap();
        let err = verify_signature(&bad, &sig, &key)
            .expect_err("a tampered file must not verify")
            .to_string();
        assert!(err.contains("SIGNATURE VERIFICATION FAILED"), "got {err}");

        // A signature file that is not one at all.
        std::fs::write(&sig, "not a signature").unwrap();
        assert!(verify_signature(&good, &sig, &key).is_err());

        let _ = std::fs::remove_dir_all(&d);
    }

    /// This repository has a signing key, and losing it would silently return
    /// `askr upgrade` to checksum-only trust — the failure mode is that nothing fails.
    /// So the committed key is asserted, not merely tolerated.
    #[test]
    fn the_committed_release_key_is_present_and_usable() {
        assert!(
            release_key().is_some(),
            "keys/release.pub holds no usable minisign public key. If that is \
             deliberate, this test is the thing to change — but note that upgrades \
             then verify a checksum and nothing about provenance."
        );
        assert!(
            !RELEASE_PUBKEY.contains(VECTOR_PUBKEY),
            "keys/release.pub is minisign's public test vector — anyone can sign \
             releases with the matching secret key, which is also published"
        );
    }

    /// The two ways there can be no key are not the same thing: a file that says it is
    /// unconfigured is a decision, and a file with a mangled key is a mistake that
    /// would otherwise look identical at the call site.
    #[test]
    fn an_unconfigured_or_mangled_key_file_yields_no_key() {
        assert!(parse_key("untrusted comment: NOT YET CONFIGURED\n# nothing here\n").is_none());
        assert!(parse_key("").is_none());
        // Right shape, wrong content — must not parse into a key that verifies nothing.
        assert!(parse_key("RW0000000000000000000000000000000000000000000000000000000").is_none());
        // And a real one still does.
        assert!(parse_key(&format!("untrusted comment: x\n{VECTOR_PUBKEY}\n")).is_some());
    }

    /// This check decides whether we replace the running binary, so it gets a known
    /// vector rather than a round trip.
    #[test]
    fn a_correct_checksum_verifies() {
        let d = tmp("ok");
        let file = d.join("payload");
        std::fs::write(&file, b"hello").unwrap();
        let sum = d.join("payload.sha256");
        std::fs::write(
            &sum,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824  payload\n",
        )
        .unwrap();
        verify_sha256(&file, &sum).expect("the published hash of \"hello\" must verify");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A body larger than the read buffer, to prove the chunked hashing loop (digest
    /// 0.11 dropped `io::Write` on hashers) consumes every chunk. A loop that stopped
    /// after the first read would happily accept a truncated download.
    #[test]
    fn hashing_spans_more_than_one_buffer() {
        let d = tmp("big");
        let file = d.join("payload");
        let body: Vec<u8> = (0..200 * 1024).map(|i| (i % 251) as u8).collect();
        std::fs::write(&file, &body).unwrap();

        let mut h = Sha256::new();
        h.update(&body);
        let want: String = h.finalize().iter().map(|b| format!("{b:02x}")).collect();
        let sum = d.join("payload.sha256");
        std::fs::write(&sum, format!("{want}  payload\n")).unwrap();
        verify_sha256(&file, &sum).expect("a multi-chunk file must verify");

        // Truncating the file must now fail against the same checksum.
        std::fs::write(&file, &body[..1024]).unwrap();
        assert!(
            verify_sha256(&file, &sum).is_err(),
            "a truncated download must not pass"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_wrong_or_malformed_checksum_is_refused() {
        let d = tmp("bad");
        let file = d.join("payload");
        std::fs::write(&file, b"x").unwrap();

        let wrong = d.join("wrong.sha256");
        std::fs::write(&wrong, format!("{}  payload\n", "0".repeat(64))).unwrap();
        assert!(verify_sha256(&file, &wrong).is_err());

        let malformed = d.join("malformed.sha256");
        std::fs::write(&malformed, "not-a-hash\n").unwrap();
        assert!(verify_sha256(&file, &malformed).is_err());
        let _ = std::fs::remove_dir_all(&d);
    }
}
