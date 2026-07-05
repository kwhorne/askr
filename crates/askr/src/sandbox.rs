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
}

/// Apply the sandbox to the current process (all threads). Best-effort: failures
/// are logged, not fatal, so a missing kernel feature can't take the server down.
#[cfg(target_os = "linux")]
pub fn apply(cfg: &SandboxConfig) {
    // Landlock is opt-in via write paths — a wrong allowlist would break the app,
    // so we only restrict the filesystem when the operator lists writable dirs.
    if !cfg.write_paths.is_empty() {
        match landlock_restrict(cfg) {
            Ok(status) => {
                tracing::info!(
                    status,
                    writable = cfg.write_paths.len(),
                    "landlock: filesystem restricted"
                )
            }
            Err(e) => tracing::warn!(error = %e, "landlock: not applied"),
        }
    }
    match seccomp_no_exec() {
        Ok(()) => tracing::info!("seccomp: process creation blocked (no execve/ptrace)"),
        Err(e) => tracing::warn!(error = %e, "seccomp: not applied"),
    }
}

#[cfg(not(target_os = "linux"))]
pub fn apply(_cfg: &SandboxConfig) {
    tracing::warn!("--sandbox is only enforced on Linux; ignored on this OS");
}

/// Restrict the filesystem: read+execute everywhere (so PHP, its extensions and
/// the app keep working), but write only under `write_paths`.
#[cfg(target_os = "linux")]
fn landlock_restrict(cfg: &SandboxConfig) -> anyhow::Result<String> {
    use landlock::{
        Access, AccessFs, PathBeneath, PathFd, Ruleset, RulesetAttr, RulesetCreatedAttr, ABI,
    };

    let abi = ABI::V1;
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
