# Hardening / sandbox (Linux)

`--sandbox` shrinks the blast radius of a PHP-level exploit. Even if an attacker
gets code execution inside PHP, the worker can't spawn a shell or tamper with
your code.

```bash
askr serve … \
  --sandbox \
  --sandbox-write /var/www/app/storage --sandbox-write /tmp
```

Two independent layers (Linux only; no effect elsewhere):

## seccomp — no new processes

`--sandbox` installs a seccomp-BPF filter (all threads) that makes
`execve`/`execveat`/`ptrace`/`process_vm_*` return `EPERM`. So a compromised
request **can't launch a shell** — `shell_exec`/`exec`/`Symfony\Process` just
fail. It's applied before the PHP/tokio threads are created, so it covers the
thread PHP runs on.

> If your app legitimately shells out (some packages do), those calls will fail
> under `--sandbox`. Test first, or don't enable it for such apps.

## Landlock — write only where allowed

Add `--sandbox-write <dir>` (repeatable) to also restrict the filesystem with
[Landlock](https://landlock.io): the worker may **read** everywhere (so PHP,
extensions and templates keep working) but may **write only under the listed
paths**. A path-traversal or upload exploit then **can't drop a webshell into the
docroot** or modify your code — writes outside the allowlist get `EACCES`.

Typical allowlist for a Laravel app:

```
--sandbox-write /var/www/app/storage      # logs, cache, sessions, compiled views
--sandbox-write /var/www/app/bootstrap/cache
--sandbox-write /tmp                       # uploads (streamed) + sqlite temp
```

Landlock degrades gracefully: on kernels without it (or an older ABI) the filter
is best-effort and never prevents startup.

## Fail closed: `--sandbox-required`

By default the sandbox is **advisory**. A kernel without Landlock, a container without
the seccomp capability, or any other missing feature logs a warning and the worker
serves traffic looking exactly like one that hardened successfully. That default is not
changing — an upgrade that started refusing to boot would be worse than the warning —
but you can opt out of it:

```bash
askr serve … --sandbox-write /var/www/app/storage --sandbox-write /tmp --sandbox-required
```

A worker that cannot fully harden then exits (status 78) instead of serving, and the
supervisor's crash-loop guard turns a fleet-wide failure into one clear "giving up".

**It requires `--sandbox-write`**, and refuses to start without it. Seccomp alone blocks
`execve`, which is not how a webshell runs here: Askr *interprets* PHP in-process, so a
`.php` file written into the docroot needs no process creation at all. Landlock write
rules are the control for that, so a "required" sandbox without them would be a promise
the sandbox cannot keep.

## Config file

```toml
[server]
sandbox = true
sandbox_write = ["/var/www/app/storage", "/var/www/app/bootstrap/cache", "/tmp"]
sandbox_required = false   # true = refuse to serve unhardened (needs sandbox_write)
```

## Verified

In a Linux container: with `--sandbox --sandbox-write /tmp`, a request that calls
`shell_exec("id")` returns **EXEC-BLOCKED**, a write to `/tmp` **succeeds**, a
write into the docroot is **DENIED**, and normal pages serve unchanged.

## Notes

- Sidecars (queue/scheduler) are **not** sandboxed — queue jobs may legitimately
  shell out; only the internet-facing web workers are hardened.
- Combine with the non-root systemd unit + capabilities in [UBUNTU.md](UBUNTU.md)
  for defence in depth.
