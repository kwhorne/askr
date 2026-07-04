# CoW template mode (experimental)

Normally each worker boots the app itself (~110 ms of framework bootstrap on a
respawn). **CoW mode** boots the app **once** in a template process and forks the
workers from it — they inherit the warm, booted heap via copy-on-write. The
payoff:

- **~ms warm respawn** — a recycled or crashed worker is re-forked from the
  still-booted template instead of cold-booting (measured ~35 ms vs ~300 ms).
- **Shared memory** — opcache, class tables and booted providers are physically
  shared between workers (copy-on-write) until written.

This is the "refork" model (pitchfork, in the Ruby world) applied to PHP.

> **Experimental.** CoW is new and best validated under load on Linux. Some
> extensions with persistent resources (e.g. `pconnect`, certain PDO drivers)
> behave badly across `fork` — test yours, and fall back to normal worker mode if
> needed. The admin plane and queue/scheduler sidecars aren't available in CoW
> mode yet.

## Enabling it

```bash
ASKR_APP_BASE=/var/www/app askr serve \
  --root /var/www/app/public \
  --worker-script examples/laravel-worker.php \
  --workers 8 --cow
```

`--cow` requires `--worker-script`. The worker script must call
`askr_cow_ready()` after booting the app and before its serving loop —
`examples/laravel-worker.php` already does (it's a no-op outside CoW mode):

```php
$app = /* boot once */;
$kernel = $app->make(Kernel::class);

if (function_exists('askr_cow_ready')) {
    askr_cow_ready();   // template forks the workers here
}

while (askr_handle_request($handler)) { /* serve the warm app */ }
```

## How it works

1. The template process boots the interpreter and runs the worker script up to
   `askr_cow_ready()`. At that point the app is fully booted.
2. `askr_cow_ready()` forks N workers. **The template is single-threaded at this
   moment** (tokio starts only in the children), so the fork is safe — no
   multi-threaded-fork hazard.
3. Each forked worker inherits the warm heap, sets up its own tokio runtime +
   accept loop, and returns into the serving loop — now serving the CoW app.
4. The template supervises: when a worker exits (recycle/crash), it **re-forks a
   fresh warm worker** in milliseconds.

## Reloading code

Because the template holds the booted app in memory, re-forking a worker yields
the *same* (old) code. To pick up new code, **restart the process**
(`systemctl restart askr`) so the template re-boots. `SIGINT`/`SIGTERM`/`SIGHUP`
all shut the template down gracefully (drain workers, then exit), so a systemd
restart is a clean reload.

For SIGHUP-style rolling/canary reloads that pick up new code without a full
restart, use normal worker mode (without `--cow`).

## When to use it

- **CoW mode:** you recycle workers often (low `--max-requests`) and want cheap
  respawns + memory sharing, and your extensions are fork-safe.
- **Normal worker mode:** you want zero-downtime rolling/canary reloads
  (`SIGHUP`), the admin plane, or queue/scheduler sidecars in the same binary.
