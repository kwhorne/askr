//! Optional process hardening (Linux): a seccomp syscall filter and a Landlock
//! filesystem sandbox, applied in each serving worker.
//!
//! The point: shrink the blast radius of a PHP-level exploit. Even if an attacker
//! achieves code execution inside PHP, `--sandbox` means the worker **cannot spawn
//! a process** (no shell — `execve`/`execveat` return EPERM) and, with Landlock,
//! **cannot write outside a small allowlist** (no dropping a webshell into the
//! docroot). No effect off Linux (and Landlock degrades gracefully on kernels
//! without it).

use std::path::PathBuf;

/// What the sandbox allows the worker to write to (the app still reads freely so
/// PHP/templates/config keep working).
#[derive(Clone, Default)]
pub struct SandboxConfig {
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub write_paths: Vec<PathBuf>,
    /// Refuse to serve unless the sandbox actually applied.
    ///
    /// Without this the sandbox is advisory: a kernel without Landlock, a container
    /// without the seccomp capability, or a missing feature logs a warning and the
    /// worker serves traffic looking exactly like one that hardened successfully.
    /// That is the wrong default to *change* — an upgrade that started refusing to
    /// boot would be worse than the warning — so it is opt-in, and when it is on the
    /// worker exits rather than serve unprotected.
    pub required: bool,
}

/// What the sandbox actually achieved, as opposed to what was asked for.
///
/// Deliberately not Linux-gated: the policy below is the part worth testing, and it
/// should compile and be tested on every platform even though only Linux can apply
/// anything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    /// The seccomp filter is installed on every thread.
    pub seccomp: bool,
    /// The Landlock ABI actually in force, if the filesystem was restricted.
    pub landlock_abi: Option<u8>,
}

/// What `report` fails to deliver against `cfg` — `None` when the sandbox is whole.
///
/// `required` means both halves, and that is the point. Seccomp alone blocks
/// `execve`, which is not how a webshell runs here: Askr *interprets* PHP in-process,
/// so a `.php` file written into the docroot needs no process creation at all.
/// Landlock write rules are the control for that, so a "required" sandbox without
/// them would be a promise the sandbox cannot keep.
pub fn shortfall(cfg: &SandboxConfig, report: &Report) -> Option<String> {
    let mut missing: Vec<&str> = Vec::new();
    if !report.seccomp {
        missing.push("seccomp (process creation is not blocked)");
    }
    if cfg.write_paths.is_empty() {
        missing.push(
            "sandbox_write (no filesystem restriction was requested, so PHP can \
                      still write a file into the docroot)",
        );
    } else if report.landlock_abi.is_none() {
        missing.push("landlock (the filesystem is not restricted)");
    }
    if missing.is_empty() {
        None
    } else {
        Some(missing.join("; "))
    }
}

/// Apply the sandbox and, when `required`, refuse to serve if it did not take.
///
/// Exits rather than returns: this runs in a freshly forked worker before it accepts
/// anything, so there is nothing to unwind and nobody to report to. The supervisor's
/// crash-loop guard turns a fleet-wide failure into one clear "giving up" rather than
/// an endless respawn.
pub fn apply_or_refuse(cfg: &SandboxConfig) -> Report {
    let report = apply(cfg);
    if cfg.required {
        if let Some(missing) = shortfall(cfg, &report) {
            tracing::error!(
                missing = %missing,
                "sandbox is REQUIRED and did not fully apply — refusing to serve. \
                 Fix the kernel/container capability or drop sandbox_required."
            );
            // EX_CONFIG: the environment cannot satisfy the configuration.
            std::process::exit(78);
        }
        tracing::info!(
            landlock_abi = report.landlock_abi,
            "sandbox is required and fully applied"
        );
    }
    report
}

/// Apply the sandbox to the current process (all threads). Best-effort: failures
/// are logged, not fatal, so a missing kernel feature can't take the server down.
#[cfg(target_os = "linux")]
pub fn apply(cfg: &SandboxConfig) -> Report {
    let mut report = Report::default();
    // Landlock is opt-in via write paths — a wrong allowlist would break the app,
    // so we only restrict the filesystem when the operator lists writable dirs.
    if !cfg.write_paths.is_empty() {
        match landlock_restrict(cfg) {
            Ok(status) => {
                report.landlock_abi = Some(LANDLOCK_ABI);
                tracing::info!(
                    status,
                    abi = LANDLOCK_ABI,
                    writable = cfg.write_paths.len(),
                    "landlock: filesystem restricted"
                )
            }
            Err(e) => tracing::warn!(error = %e, "landlock: not applied"),
        }
    }
    match seccomp_no_exec() {
        Ok(()) => {
            report.seccomp = true;
            tracing::info!("seccomp: process creation blocked (no execve/ptrace)")
        }
        Err(e) => tracing::warn!(error = %e, "seccomp: not applied"),
    }
    report
}

#[cfg(not(target_os = "linux"))]
pub fn apply(_cfg: &SandboxConfig) -> Report {
    tracing::warn!("--sandbox is only enforced on Linux; ignored on this OS");
    Report::default()
}

/// The Landlock ABI this build asks for. See the note in `landlock_restrict`.
#[cfg(target_os = "linux")]
const LANDLOCK_ABI: u8 = 1;

/// Restrict the filesystem: read+execute everywhere (so PHP, its extensions and
/// the app keep working), but write only under `write_paths`.
#[cfg(target_os = "linux")]
fn landlock_restrict(cfg: &SandboxConfig) -> anyhow::Result<String> {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
    };

    let abi = ABI::V1;
    // Still pinned. Negotiating the newest ABI the running kernel supports is the
    // remaining half of this (V2 adds file re-parenting, V3 truncate, V4 network,
    // V5 ioctl), and it is not done here because this file cannot be compiled on the
    // machine it was written on — landlock is a Linux-only dependency, and the C shim
    // needs a Linux cc to cross-check. Guessing at a crate API in a security path is
    // how you ship a build break; it wants a Linux build in the loop.
    let mut ruleset = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))?
        .create()?;

    // Read/execute the whole filesystem.
    ruleset = ruleset.add_rule(PathBeneath::new(
        PathFd::new("/")?,
        AccessFs::from_read(abi),
    ))?;

    // Full access (incl. write) only under the allowlisted paths.
    for p in &cfg.write_paths {
        if let Ok(fd) = PathFd::new(p) {
            ruleset = ruleset.add_rule(PathBeneath::new(fd, AccessFs::from_all(abi)))?;
        }
    }

    let status = ruleset.restrict_self()?;
    Ok(format!("{:?}", status.ruleset))
}

/// Block process creation and debugging syscalls (return EPERM so PHP's exec()
/// fails gracefully rather than killing the worker). TSYNC covers all threads.
#[cfg(target_os = "linux")]
fn seccomp_no_exec() -> anyhow::Result<()> {
    use seccompiler::{apply_filter_all_threads, BpfProgram, SeccompAction, SeccompFilter};
    use std::collections::BTreeMap;

    let denied = [
        libc::SYS_execve,
        libc::SYS_execveat,
        libc::SYS_ptrace,
        libc::SYS_process_vm_readv,
        libc::SYS_process_vm_writev,
    ];
    let mut rules: BTreeMap<i64, Vec<seccompiler::SeccompRule>> = BTreeMap::new();
    for sc in denied {
        rules.insert(sc, vec![]); // empty rule set = always match this syscall
    }

    #[cfg(target_arch = "x86_64")]
    let arch = seccompiler::TargetArch::x86_64;
    #[cfg(target_arch = "aarch64")]
    let arch = seccompiler::TargetArch::aarch64;

    let filter = SeccompFilter::new(
        rules,
        SeccompAction::Allow, // default: allow everything else
        SeccompAction::Errno(libc::EPERM as u32), // denied syscalls → EPERM
        arch,
    )
    .map_err(|e| anyhow::anyhow!("seccomp filter: {e}"))?;
    let prog: BpfProgram = filter
        .try_into()
        .map_err(|e| anyhow::anyhow!("seccomp compile: {e}"))?;
    apply_filter_all_threads(&prog).map_err(|e| anyhow::anyhow!("seccomp apply: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The policy, which is the part worth testing — and testable everywhere, unlike
    /// the syscalls it judges.
    #[test]
    fn a_required_sandbox_is_both_halves_or_nothing() {
        let full = SandboxConfig {
            write_paths: vec![PathBuf::from("/var/www/app/storage")],
            required: true,
        };

        assert_eq!(
            shortfall(
                &full,
                &Report {
                    seccomp: true,
                    landlock_abi: Some(1)
                }
            ),
            None,
            "both halves applied is the only whole sandbox"
        );

        // Seccomp alone is the case the old code reported as a success.
        let missing = shortfall(
            &full,
            &Report {
                seccomp: true,
                landlock_abi: None,
            },
        )
        .expect("landlock missing must be a shortfall");
        assert!(missing.contains("landlock"), "got {missing}");

        let missing = shortfall(
            &full,
            &Report {
                seccomp: false,
                landlock_abi: Some(1),
            },
        )
        .expect("seccomp missing must be a shortfall");
        assert!(missing.contains("seccomp"), "got {missing}");

        // No write paths at all: seccomp blocks execve, which is not how a webshell
        // runs in an in-process interpreter. "Required" cannot mean this.
        let no_paths = SandboxConfig {
            write_paths: vec![],
            required: true,
        };
        let missing = shortfall(
            &no_paths,
            &Report {
                seccomp: true,
                landlock_abi: None,
            },
        )
        .expect("a required sandbox with no write paths is not a sandbox");
        assert!(missing.contains("sandbox_write"), "got {missing}");
    }
}
