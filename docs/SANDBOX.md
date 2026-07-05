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

## Config file

```toml
[server]
sandbox = true
sandbox_write = ["/var/www/app/storage", "/var/www/app/bootstrap/cache", "/tmp"]
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
