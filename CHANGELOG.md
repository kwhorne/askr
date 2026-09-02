# Changelog

All notable changes to Askr. From 1.0, the project follows [Semantic Versioning](https://semver.org)
and the compatibility contract in [docs/STABILITY.md](docs/STABILITY.md).

## Unreleased

### Security

- **The Pusher HTTP trigger accepted unsigned requests from anyone.** `POST
  /apps/{id}/events` published whatever it was sent into whatever channels it named —
  `private-` and `presence-` included — with no authentication at all, while the
  subscription side HMAC-verified exactly those channels. Anyone who could reach the port
  could forge a server event that every subscribed Echo client would treat as genuine,
  and could flood the broadcast ring fast enough that legitimate events fell off it.

  The trigger now requires Pusher's own request signature when a secret is configured:
  `auth_key`, `auth_timestamp`, `auth_version`, `body_md5` and `auth_signature` (an
  HMAC-SHA256 over the method, path and sorted query), which is what `pusher-php-server`
  — and therefore Laravel's broadcaster — sends on every call, so a correctly configured
  app needs no change. `body_md5` pins a signature to its body, the timestamp window is
  Pusher's ten minutes, and both comparisons are constant-time. Verified against the
  worked example in Pusher's HTTP API documentation rather than a round trip, so the
  test catches the string-to-sign being assembled wrong.

  With no secret configured the trigger is still accepted, matching the subscription
  side's documented development mode — and now logs a warning the first time it does,
  because on the write side "development mode" means anyone can publish.

  Also fixed alongside: the subscription signature was compared with
  `eq_ignore_ascii_case`, which returns at the first differing byte. Both checks in
  `pusher.rs` now go through one constant-time comparison.

### Fixed

- **Over HTTP/2, PHP saw one of the browser's cookies, not all of them.** RFC 9113
  lets a client send one `cookie` field per cookie, and Chrome and Firefox do exactly
  that. Hyper hands them over as separate values, and `cgi.rs` read the first
  (`.get()`) for the request's cookie string while the `$_SERVER` loop pushed each as a
  separate `HTTP_COOKIE` entry, of which the PHP array kept the last. A browser sending
  `laravel_session` and `XSRF-TOKEN` as two fields reached Laravel with one of them
  missing — a 419 on the form, or an anonymous request from a logged-in user, depending
  on which field came first that time.

  The cookie string is now every `Cookie` field joined with `"; "`, as the RFC requires
  the server to do, and any other repeated field joins with `", "` per RFC 9110 §5.3.
  It survived because, as the 1.4.7 entry already noted, every test client in this
  repository speaks HTTP/1.1; the new test builds the two-field request directly.

- **A bearer-authenticated request was "anonymous" to the response cache.** Anonymity
  was defined as "no cookie that isn't on the ignore list", so a `GET /api/me` with
  `Authorization: Bearer …` and no cookies qualified. The app has to opt a response in
  with `Askr-Cache` — but a `[[cache.rule]]` is the operator opting in on the app's
  behalf, "cache policy for apps you can't edit", and a rule on `/api/*` cached one
  user's response and served it to the next. `Authorization` and `Proxy-Authorization`
  now count as identity, exactly as a session cookie does; Varnish passes them by
  default for the same reason. A rule's `force` still overrides, as it did for cookies.

- **`--record-errors` wrote credentials and bodies world-readable.** A 5xx on a login
  form persisted the form body — the password — plus the session cookie and any bearer
  token, with `std::fs::write` at umask mode, which under 022 is readable by every local
  user. The directory is now created `0700`, each file `0600` with `O_EXCL|O_NOFOLLOW`
  (an operator-named directory must not have its files planted by someone else), and
  `Cookie`, `Authorization` and `Proxy-Authorization` are replaced with `[redacted]` in
  the envelope before it is written — the key stays, so a replay knows the header was
  there. The body is kept as sent, because it is what makes a replay a replay; that is
  why the files are private and why the docs say the directory is sensitive. An
  auth-dependent failure therefore replays unauthenticated. That is the trade.

- **A WebSocket client could hold 64 MiB per message and unbounded subscriptions.**
  `FragmentCollector` was left at fastwebsockets' default `max_message_size` — 64 MiB,
  buffered per connection, for anyone who opened one — and the per-connection
  subscription set had no bound, so a loop could hold millions of channel names as
  `String`s for the life of the socket. Messages are capped at 64 KiB (Pusher frames are
  a few hundred bytes), subscriptions at 256, channel names at Pusher's own 164.
  The SSE bridge had the same gap on the subscribe side — `CHAN_MAX` bounded what could
  be *published*, not what a subscriber could ask to hold — and now refuses a name over
  it. And the `subscription_error` frame interpolated the client's channel name into a
  JSON string with `format!`; it is built with `serde_json` now, so a quote in the name
  is a quote and not a broken frame.

- **Shared memory is now partitioned per application.** The KV cache, the job queue
  and the response-cache tag table were one region per instance. With `[[site]]`
  hosting several applications, any one of them could read every other's cache and
  sessions by key, log all of them out with one `askr_cache_flush()`, pop or acknowledge
  another application's jobs, and invalidate its cached pages by tag name. Two sites
  deployed from one codebase under one `APP_KEY` shared sessions across domains. None
  of it was documented.

  Every key, queue name and tag now carries a namespace derived from the application's
  **docroot** — canonicalised and hashed, sixteen hex digits — applied as the request is
  handed to PHP and once at boot for sidecars. Docroot rather than host, because two
  domains serving one docroot are one application and should share, while two docroots
  are two applications and must not; that makes it automatic, with nothing to configure
  and nothing to get wrong. `askr_cache_flush()` sweeps only the caller's prefix.
  `askr_queue_delete`/`release` refuse a job whose queue carries another namespace, so
  ids stay global but acks do not cross. Single-application instances see one namespace
  and notice nothing, except that the effective maximum key length is 17 bytes shorter.

  Not partitioned, and now documented as such in HOSTING.md: broadcasting (one Pusher
  secret per instance means one application's realtime), and the response cache's whole
  flush. Sidecars belong to the application at the top-level `root`; a second
  application's jobs land in their own namespace and nothing pops them — correct, since
  they would otherwise run inside the wrong codebase, and a reason unrelated
  applications want their own instance.

  One migration note for `[cache] persist` users: response-cache entries restored from
  a pre-1.5.1 file carry un-namespaced tag hashes, so `forget_tag` will not match them
  until they expire on their own TTL. A one-time, bounded cost.

- **Four small ones from the same review.** `askr_cache_increment` added its PHP
  integer with `+`, which wraps in a release build, so two increments by `PHP_INT_MAX`
  turned a counter negative and a rate limit keyed on it reopened; it saturates now. The
  queue's `delay * 1000` and `visibility * 1000` overflowed on a large value — a panic
  in debug, and in release a wrapped, tiny `reserved_until` that made a job somebody was
  running poppable again at once; all three time computations saturate, so an absurd
  visibility means "never", which is what it meant. The access log was created at umask
  mode, world-readable under 022 with client IPs in it; it is `0640` on creation now, and
  an existing file keeps whatever mode the operator gave it. And ESI fragments, fetched
  directly, went round the rate limiter: a page with 32 includes cost one token and ran
  PHP 33 times. A fragment is checked in the same place a request is, so a
  `[[ratelimit]]` rule on `/_esi/*` means what it says, and a refused fragment is left
  empty like any other fragment that failed.

- **A worker whose lease lapsed can no longer acknowledge or release a job another
  worker has since taken.** `pop` reserved a job by moving `reserved_until` and left the
  id alone, so once a lease lapsed and a second worker claimed the job, the first
  worker's `delete(id)` acked the job the second was still running — and its
  `release(id, delay)` put the job back for a third run while two were already on it.
  Nothing detected the stale ack because there was nothing to detect it with.

  This was written down as needing a signature change through three layers — `shim.c`,
  the Rust bridge and the Laravel driver all take the id alone. It does not: the thing
  PHP holds can *be* the lease. `pop` now hands back a per-reservation lease as the job's
  id; `delete` and `release` look the job up by lease, and a lapsed worker's token names a
  reservation that no longer exists. The Laravel driver stores the value and hands it
  back, which is all it ever did with the id, so nothing above the ring changes.

  The shared-memory ring keeps a global lease counter so a lease is never reused by
  another slot while a stale holder could present it. The SQL backend (`ASKR_QUEUE_DB`)
  had the same gap and gets the same property without a schema change: its token packs
  the row id with the attempt number that claimed it, and `delete`/`release` require the
  row to still be on that attempt. Both backends carry a test that lets a lease lapse,
  claims the job again, and asserts the first token is refused.

  One visible consequence: a job's id, as Laravel sees it, now changes across retries —
  it is the lease. Anything correlating log lines by job id across attempts should use
  the payload's uuid, which is what Laravel's own `failed_jobs` machinery does.

- **`/api/status` now reports what the sandbox achieved, not what was configured.** A
  `sandbox` object carries `configured` and `required` beside `workers`, `seccomp`,
  `landlock` and `landlock_abi`, the last four counted by the workers themselves as they
  apply it. A fleet where `workers` exceeds `seccomp` or `landlock` is serving partly
  unhardened — the condition that, without `sandbox_required`, used to log a warning and
  otherwise look identical to success from every vantage point an operator has.

### Changed

- **Every GitHub Action is pinned to a commit SHA.** Ten actions across five workflows
  were referenced by major tag — `actions/checkout@v7`, `softprops/action-gh-release@v3`
  and so on — and a tag can be moved. Since 1.5.0 the release job holds the signing key,
  so an action whose tag was moved to malicious code would sign whatever it built with
  the real key; that was the one supply-chain path signing did not cover. Each `uses:`
  now names a SHA with the version in a trailing comment, and dependabot's
  `github-actions` ecosystem is enabled so the pins move forward under review rather
  than rot.

## 1.5.0 — 2026-08-31

A release about failing safely. Three security fixes where the old behaviour was to warn
and carry on, a shared-memory correctness pass, a self-update that now verifies who
produced what it installs, and cache reads that no longer serialise the fleet.

Nothing in the documented surface changed incompatibly; [STABILITY.md](docs/STABILITY.md)
still holds. Two things are worth adopting deliberately rather than by upgrading:
`--sandbox-required` and, if you run behind a local reverse proxy, `ASKR_ADMIN_TOKEN`
(see [Upgrading](docs/UPGRADING.md#to-150)).

### Security

- **`h2` upgraded to 0.4.19** ([RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258),
  unbounded empty DATA frames). A remote peer could hold an HTTP/2 connection open
  sending empty DATA frames without bound — a denial of service against the transport
  Askr serves on by default. Also picked up `chacha20` 0.10.2, replacing a yanked
  release. `cargo audit` is clean again.

- **The scheduler sidecar no longer dies on an error from `schedule:run`, and says what
  happened.** A `TypeError` has been seen escaping it in production — six times over three
  days, always in the second after a scheduled job ran, then nothing for 22 hours across two
  restarts and an upgrade. The events had already run: the failure is on the return path,
  not the work, so nothing was lost.

  The cause is **not known**, and the investigation is worth reading before anyone starts
  again: every command in the application declares `: int`, no `handle()` has an early
  return, there is no `bindMethod` anywhere in the tree, the framework version is identical
  to local, and it does not reproduce. `Kernel::call()` is declared `: int`, so the
  TypeError is thrown inside Laravel as it returns and the offending value never reaches the
  caller — which is why this logs the exception rather than the return value. That is all
  that can be observed from outside.

  Catching earns its place regardless of the mystery: uncaught, this ended the process, the
  supervisor respawned it, and the scheduler missed the boundary it had been sleeping for. A
  cosmetic error should not cost a tick. The message now carries class, file, line, and the
  sentence that would have saved the most time — that scheduled events had already run, so
  the next person does not begin by hunting for lost work.


### Added

- **Cache reads no longer take a lock.** `Cache::get` sampled a per-slot spinlock
  *exclusively*, so every worker reading the same hot key — a session, a shared config
  blob, anything Laravel touches on each request — queued behind the others. Reads now
  use a seqlock: sample a per-slot version counter, copy, sample again; a change means a
  writer overlapped the copy, so retry, and after a bounded number of attempts take the
  lock. Writes are unchanged and still serialised on the spinlock.

  Measured before touching the lock, because a claimed speedup is not a speedup. Both
  paths are kept and benchmarked against each other in one process
  (`cargo test --release -p askr --bins cache_read_scaling -- --ignored --nocapture`),
  reading one key on a 12-performance-core machine:

  | readers | locked | lock-free |
  | --- | --- | --- |
  | 1 | 22.1M/s | 22.9M/s |
  | 2 | 7.3M/s | 10.2M/s |
  | 4 | 6.0M/s | 75.0M/s |
  | 8 | 2.5M/s | 48.5M/s |
  | 12 | 1.5M/s | 19.1M/s |

  The locked column is the finding: throughput *falls* as readers are added, because a
  spinlock under contention burns cycles instead of waiting. Peak aggregate read
  throughput on one key went from ~22M/s to ~75M/s, and the shape changed from
  collapsing to scaling. The lock-free numbers are noisy above 8 threads on a laptop;
  the locked collapse is monotonic and reproduces in every value size.

  Three things the work turned up that are worth recording:

  - The benchmark caught a regression it was not looking for. The first version of the
    read path allocated with `vec![0u8; len]`, which zeroes the buffer before the copy
    — a 4 KB value read by a single thread came out at 0.8× of the locked path it
    replaced, purely from the extra pass over the page. Fixed with an uninitialised
    allocation; single-reader throughput is now 1.0× at every size.
  - The counter cannot share the `lock` word, because shmlock reclaims a slot from a
    dead holder by recognising the PID stored there.
  - `Writing::begin` forces the counter odd rather than incrementing it. A writer killed
    mid-update leaves it odd forever, and an increment would then make it *even* during
    the following write — a reader would sample a stable-looking counter in the middle of
    a copy. Forcing odd on entry and even-and-greater on exit repairs the slot instead.
    There is a test for exactly that: a slot left mid-write is readable via the lock and
    back in phase after the next write.

  `get` also no longer tombstones an expired entry it happens to find, on the lock-free
  path — it takes the lock, re-checks, and then reclaims. Expiry is rare, so this keeps
  the fast path read-only.

  The torn-read test is the one that matters, and it was verified against a defeated
  seqlock: with the two counter samples removed, 584 533 of 2 145 847 reads came back
  half-written. With them, zero.



- **Releases are signed, and `askr upgrade` refuses one that is not.** The trust chain
  used to end at GitHub: the tarball and its `.sha256` came from the same release, so the
  checksum proved the download arrived intact and nothing about who produced it — a
  compromised release, account or CI token serves a matching pair. For a command that
  runs as root and replaces the binary systemd starts, that was the whole of it.

  The release workflow now signs every tarball with minisign and publishes the
  `.minisig` beside it; `upgrade` verifies it against a public key compiled into the
  binary (`keys/release.pub`, `include_str!`'d), streamed so a tarball is never held in
  memory to check. A bad signature, a signature from another key, or no signature at all
  is a refusal, not a warning. Prehashed signatures only — modern minisign produces those
  by default, and refusing legacy mode removes a variant nobody should still be using.

  Releases also carry a SLSA build-provenance attestation (`actions/attest-build-provenance`),
  which answers a different question: minisign says the key holder made this, provenance
  says this workflow built it from this commit. `gh attestation verify` checks the second;
  the binary checks the first, with no network and no GitHub involved.

  **The key is generated by whoever holds it, deliberately not automated.**
  `rsign generate -W -c "askr release signing key" -p keys/release.pub -s
  ~/.askr/askr-release.key` — public half committed, secret half in the repository secret
  `MINISIGN_SECRET_KEY`. A key generated anywhere it could be observed is compromised the
  moment it exists. `docs/RELEASING.md` step 0 has the procedure, including the two parts
  worth reading twice: `-p` is what saves the public key at all (without it rsign prints
  it once and there is no way to get it back), and losing the secret key locks out every
  future release, because installs built against the old key refuse the new tarballs.

  Signing uses rsign2 rather than the minisign C tool. They implement the same format and
  are interoperable — either verifies a release either produced — but rsign2 is what the
  key was generated with, `rsign sign -W` is non-interactive without depending on how a
  passwordless key gets prompted for, and it always prehashes, which is what the verifier
  requires.

  A build with no key committed still upgrades, and says on every run that it checked a
  checksum and not a provenance — an unconfigured build must not be a build that cannot
  upgrade. This one is configured: `keys/release.pub` holds key `5AD94F1DEEDF89FD`, and a
  test asserts it parses, because losing it would quietly return upgrades to
  checksum-only trust and nothing would fail.

  The verification path is tested against minisign's published prehashed test vector
  rather than a round trip, so a mis-parsed format is caught and not just our own code
  agreeing with itself. A second test fails if `keys/release.pub` ever contains that
  vector's *public* key, whose secret half is also published and would look like a
  working signing setup.

  `docs/INSTALL.md` and `docs/UBUNTU.md` now verify the signature before unpacking, which
  is the one install `upgrade` cannot check for you. `SECURITY.md` documents both checks.

- **`--sandbox-required` / `[server] sandbox_required` — fail closed.** The sandbox was
  advisory: a kernel without Landlock or a container without the seccomp capability logged
  a warning and the worker served traffic looking exactly like one that hardened. That
  default stays, because an upgrade that started refusing to boot would be worse than the
  warning, but it can now be opted out of. A worker that cannot fully harden exits 78, and
  the crash-loop guard turns a fleet-wide failure into one clear "giving up".

  It requires `sandbox_write`, and refuses to start without it. Seccomp alone blocks
  `execve`, which is not how a webshell runs here: Askr interprets PHP in-process, so a
  `.php` written into the docroot needs no process creation at all. A "required" sandbox
  without Landlock write rules would be a promise the sandbox cannot keep, so that
  combination is rejected before anything forks rather than discovered per worker.

  The policy is deliberately not Linux-gated — `sandbox::shortfall` decides what a report
  fails to deliver, and is unit-tested on every platform even though only Linux can apply
  anything.


- **A reload is now held to "every worker was replaced"** by a regression test
  (`a_reload_replaces_every_worker`), rather than to `rollout: idle`. It records every PID,
  sends SIGHUP, and polls until no pre-reload PID remains — with sidecars in the fleet,
  since a queue worker recycling on its own is the event most likely to compete with the
  roll. A reload that leaves a worker on the old code serves the previous release from a
  fraction of requests and reports success.

### Fixed

- **A panic in an `extern "C"` entry point no longer takes the worker with it.** There
  were 35 of them across `cache`, `cache_sql`, `squeue`, `squeue_sql`, `broadcast`,
  `broadcast_sql` and the PHP request trampolines, and no `catch_unwind` anywhere in the
  workspace — so a panic at the boundary aborted the process mid-request, and what it was
  about was reported nowhere a person looks. Each now runs inside `ffi::guard`, which
  answers the caller's failure value (a cache miss, a refused push, a 502) and logs the
  entry point by name.

  Shared-memory state survives it because the things protecting it are RAII and run while
  unwinding: `Slot` releases the spinlock. `Writing` needed a change to be safe here —
  its `Drop` marked the slot settled, so catching a panic mid-`write` would have
  advertised a half-updated slot as stable to the new lock-free readers. It now leaves the
  counter odd when dropped during a panic, which sends readers to the lock and lets the
  next writer repair it.

  Not wrapped, deliberately: the two signal handlers in `supervisor.rs` (`catch_unwind` is
  not async-signal-safe, and they touch nothing but atomics), and `cow_ready_trampoline`,
  which forks the fleet — a panic there is a startup failure that has to stay loud.

- **A deleted cache key could come back, so a logged-out session could log itself back
  in.** The probe loop in `Cache::set` holds one slot lock at a time, so two concurrent
  `set`s of the same key can pick different targets — one finds a slot empty that the
  other has since filled, or the two disagree about which live entry is oldest because
  `written_at` moved underneath them. The key then exists in two slots, and `delete`
  returned at the first match: it tombstoned one copy and left the other live, so the
  next lookup found it. For a session key that is a user who logged out and is logged
  in again.

  The comment sitting at the end of `set` claimed the target was re-validated under the
  lock. It was not — the write was unconditional, and the comment described the fix
  that was missing. `delete` now tombstones every match in the chain, and `set` sweeps
  the chain afterwards so duplicates converge to one entry instead of accumulating. The
  sweep takes slot locks in the same ascending order as the probe, so it cannot deadlock
  against one.

  The race is not reproducible on demand, so the regression test plants the duplicate
  directly and asserts the consequence: after `delete`, `get` must return nothing. It
  fails against the old `delete`.

- **A full queue ring dropped jobs in silence.** The no-ring branch of `squeue::push`
  carries the argument for why that is unacceptable — returning 0 is all the PHP API can
  express, Laravel does not check it, so from the application side a lost job looks like
  a job that ran — and then the full-ring branch a few lines down was a bare `0` with no
  log at all. It now reports the queue name and slot count, throttled to once every 30 s
  because a full ring recurs and clears and an operator needs to see each occurrence,
  not just the first since boot.

- **A crash mid-`push` leaked a queue slot permanently.** `id` was the *first* field
  written, and `id != 0` is what makes a slot occupied — so a process that died anywhere
  in the writes that followed left a slot claimed by a job that does not exist: `pop`
  could hand out the previous occupant's payload under the new id, and nothing ever
  frees it. One slot lost per crash until the ring is full of them. `id` is now written
  last as the commit marker, which is the discipline `broadcast::publish` already
  follows with its `seq`; a crash mid-push now leaves `id == 0` and the slot simply
  still free.

- **A recycled PID could wedge a shared-memory region for good.** `shmlock` steals a
  slot lock only from a holder the kernel confirms is dead, which is the right rule and
  rests on `kill(pid, 0)` answering a question it does not answer: it says whether *a*
  process has that number, not whether it is the one that took the lock. A holder can
  die while `pid_max` wraps — minutes on a fork-heavy box — and the number be reused by
  something long-lived, after which every waiter sees a live holder that will never
  release. Unbounded wait, no log.

  A holder whose PID has not changed for ten seconds is now stolen from as well, with an
  error naming it. That is the same steal the module was written to remove, four orders
  of magnitude further out: the old scheme stole after 100–200 µs, shorter than a
  scheduler slice, which is why it corrupted state. A ≤64 KB copy that has not finished
  in ten seconds is not preempted. Tracked in the waiter's own stack, so no extra word
  in the slot and the same behaviour on Linux and macOS.

- **`[server] force_https` in TOML was ignored when validating `http_redirect`.** The
  address was read from the flag *or* the config, and then the guard checked only the
  CLI flag — so a perfectly good TOML setting both keys was refused at startup, by an
  error message that named the config key it was ignoring. The ACME front a few lines
  above already makes this exact distinction, with a comment explaining why.

- **`Host: [::1]:8080` became `"["`.** The port was stripped with
  `authority.split(':').next()`, which truncates an IPv6 literal at its first colon.
  That string became `SERVER_NAME`, the virtual-host routing key, and a field of the
  response-cache key. One helper now does it correctly for all three call sites — and
  for the new admin `Host` check, which had grown a fourth copy of the same logic. Only
  a client addressing the server by IPv6 literal reached it, which is why it survived:
  every test client in this repo uses a name or an IPv4 address.

- **The rate limiter discarded its refill remainder.** `last_ms` jumped to `now`
  whether or not the integer division produced anything, so a client arriving faster
  than one token's worth of milliseconds refilled zero on every call and stayed blocked
  however long it had actually been waiting. `limit < window` makes that ordinary — 10
  per 60 s is 6 s per token, so any polling faster than every 6 ms starved. `last_ms`
  now advances only by the time the refill accounts for. Low severity, and a real
  regression test: `consume` takes `now`, so the test drives 7 000 fabricated
  milliseconds instead of sleeping for six seconds.

- **`Accept-Encoding: br;q=0` was served brotli.** `q=0` is not a weak preference, it
  is a refusal, and `starts_with("br")` matched `br;q=0` exactly as happily as `br`.
  Tokens are now matched exactly — `starts_with` also accepted `brotli`, which nobody
  serves — and a `q=0` token is treated as not offered. Ranking is unchanged: br before
  gzip, other q-values still ignored.

- **Shutting down with jobs still in the queue is now on the record.** The ring is an
  anonymous shared mapping: it lives as long as the process tree and has no persist
  path, so a restart — `askr upgrade` included — comes up empty and the jobs that were
  in it never run. Nothing in the application sees an error, which is how this reads as
  "Laravel lost the mail". The master now logs an error naming the number of jobs being
  lost, counted after every worker is reaped so the region is quiescent and the count is
  exact. `docs/MAINTENANCE.md` gains a drain procedure and `docs/UPGRADING.md` gains it
  as a step in the upgrade sequence.

- **`PURGE`/`BAN` were open to the internet behind a local reverse proxy.** With no
  `ASKR_ADMIN_TOKEN` set, cache invalidation was accepted from loopback peers — which
  is a sound rule for a server that is its own front door and no rule at all behind
  nginx or Caddy on 127.0.0.1, where *every* request arrives from loopback. Anyone
  could then send `BAN` with `X-Ban-Url: /*` and empty the cache on demand.
  `trusted_proxies` is the operator stating in writing that loopback is where the
  proxy sits, so once it is set the loopback fallback no longer applies and a token is
  required.

- **The SSE bridge subscribed to `private-` and `presence-` channels without
  authenticating them.** `pusher.rs` HMAC-verifies a subscription to those prefixes
  before adding it to a socket; `GET /askr/events?channel=private-orders` did no such
  thing and streamed everything published on the channel to whoever asked. Two
  transports for one channel namespace, one of them enforcing the rule.

  The SSE path has no socket id and no signature to verify one against, so it cannot
  honour the same check — it now refuses those prefixes with `403` instead. Public
  channels are unaffected. Signed SSE subscriptions would be the feature; declining to
  be the hole in the meantime is the fix.

- **The admin plane accepted DNS-rebound reads and cross-site reloads.** Neither
  needed the token to be unset to work, which is why both checks now apply whether or
  not one is configured.

  `Host` was never looked at. A page on an attacker's domain re-resolves its own
  hostname to 127.0.0.1; the browser then treats `http://evil.test:9000/api/status` as
  same-origin and hands the response — PIDs, RSS, error records — to the attacker's
  script. Nothing in that request looks cross-site, because to the browser it isn't:
  the only thing that gives it away is `Host: evil.test` naming a listener that is not
  called that. A loopback-bound plane now requires a `Host` that names it, with
  `ASKR_ADMIN_HOSTS` for a proxy that forwards its own.

  And `POST /api/reload` is a CORS "simple request" — no custom headers, so no
  preflight, so CORS never got a say and any web page could roll the fleet. Requests a
  browser reports as cross-site (`Sec-Fetch-Site`, or an `Origin` that disagrees with
  `Host`) are refused. `curl` and deploy scripts send neither header and keep working:
  this refuses what identifies itself as cross-site rather than demanding proof of not
  being a browser.

- **The admin reload and an ACME renewal bypassed the canary gate.** `[reload] canary`
  was honoured by the SIGHUP handler and by nothing else: `trigger_reload()` — the
  admin API, a renewed certificate, the cert-mtime watcher — called `roll_next()`
  directly and rolled the whole fleet with no health check. Those are exactly the
  reloads that happen with nobody watching, so they are the ones that needed the gate
  most. Both paths now enter through it.

- **The response cache ignored the application's `Vary`, and kept no scheme in the
  key.** Three defects with one cause: the key can express negotiated encoding and
  device class, and everything else the response said about its own variance was
  dropped.

  `Vary: Accept-Language` from a localised Laravel app meant the first visitor's
  language was cached and served to everyone. Such responses are now not cached at
  all. That costs hit rate on exactly the responses that were being served wrong, and
  honouring `Vary` properly needs a two-level lookup in `rcache` — a variant list per
  primary key — which is a design change, not a patch. Recorded under known issues.

  Scheme is now part of the key. Without it one entry was shared by http and https, so
  with `force_https` off a page holding absolute URLs (`url()`, `asset()`, a canonical
  tag) could be rendered over http and then served to https clients with http links
  baked in. It is appended last, because `rcache::key_parts` reads the first three
  fields — `PURGE` and `BAN` keep working and stay scheme-agnostic, which is what
  anyone purging a URL means.

  And a response the app had gzipped itself was cached as garbage: `storable_header`
  drops `Content-Encoding`, and `compress::maybe` returns an already-compressed body
  unchanged because re-compressing it comes out larger — so the entry held gzip bytes
  with nothing declaring them, and every hit sent binary to the browser. Those are
  refused too.

- **Upload temp files could be streamed into a directory another local user owned.**
  `$TMPDIR/askr-uploads` is a fixed, world-known path, and on a shared host somebody
  else can create it first. Every result that would have revealed it was discarded:
  with `recursive(true)` an existing directory is not an error, `set_permissions()` on
  a directory owned by another user fails with EPERM, and both were `let _ =`. Uploads
  — whatever people type into forms — then landed somewhere readable by its owner, who
  could also substitute an entry with a symlink between our create and PHP's read.

  The directory is verified now rather than assumed: `lstat`, owned by this process,
  no access for anybody else, and a chmod that is checked instead of trusted. When the
  shared path isn't ours the server uses a private `askr-uploads-<uid>-<pid>` beside it
  and says so, because an image that pre-creates `/tmp/askr-uploads` as root and then
  drops to www-data is a legitimate setup and should not take uploads down. Files are
  created `O_CREAT|O_EXCL|O_NOFOLLOW` at 0600.

- **`X_Forwarded_For` and `X-Forwarded-For` became the same `$_SERVER` key.** Header
  names were upper-cased with dashes replaced by underscores, so both spellings
  collapsed to `HTTP_X_FORWARDED_FOR` and which one PHP saw depended on header
  iteration order. Anything filtering the dashed spelling — a WAF, a proxy that
  rewrites the header, Laravel's `TrustProxies` reading `$_SERVER` — was bypassed by
  sending the underscored one. An underscore in a header name is now dropped rather
  than merged, which is the same default nginx ships as `underscores_in_headers off`.

- **The crash-loop guard did not count a worker killed by a signal.** It tested
  `WIFEXITED && WEXITSTATUS != 0`, so a worker that segfaulted on boot — the loudest
  form of the thing the guard exists to stop — respawned forever. Worse than not
  counting: it took the healthy branch and *cleared* the streak, so a fleet mixing
  fatals and faults never accumulated one either. Fault signals now count
  (`SIGSEGV`/`BUS`/`ILL`/`FPE`/`ABRT`/`SYS`/`TRAP`); `SIGTERM` deliberately does not,
  because every intentional termination in `supervisor.rs` uses it and a rolling reload
  must not look like a crash-loop. `SIGSYS` is in the list because a seccomp filter
  killing the worker is a boot loop like any other.

- **ACME wrote the key and the certificate in place, one after the other.** A worker
  spawning or reloading in between read a new key against the old certificate and
  failed to start; the window was the length of two file writes. Both are staged and
  renamed now, key first and certificate last — the cert-mtime watcher keys on the
  certificate, so by the time anything notices a change the matching key is already
  there. The key's temp file carries 0600 from creation rather than being tightened
  afterwards.

- **A stale read offset could have handed heap memory back as a request body.** In
  worker mode the offset `php://input` reads from was never reset between requests:
  `askr_req_reset()` freed the body and zeroed its length and never touched the third
  variable, which was declared thirty lines away beside the SAPI callback that consumed
  it rather than beside the state it belongs to. A shorter body arriving after a longer
  one then evaluated `w_body_len - w_body_off_read` in `size_t` — which does not go
  negative, it underflows to something near `SIZE_MAX` — so `n` became whatever PHP
  asked for and the `memcpy` read past the end of the allocation. It is a read, so
  nothing crashes: it returns the process's own heap as the body of a request, and an
  application will echo it.

  Not exploitable as shipped, and the reason matters. PHP only calls the post reader
  when `SG(request_info).content_length` is set, and the worker path never sets it — the
  body reaches the worker script as `$request['body']`, and `examples/laravel-worker.php`
  builds the Request from that. So the worker branch of `askr_read_post` is unreached
  today. It was one assignment from live: setting `content_length` is the first thing
  anyone making `php://input` work natively in worker mode would do.

  Fixed in three places, because the one-line version is the one that comes back. The
  offset now sits with `w_body` and `w_body_len` where `askr_req_reset()` can see it; it
  is reset there and in `askr_req_set_body()`, matching what the one-shot path has always
  done, where `g_req.body_off = 0` sits on the line after `g_req.body_len = body_len`;
  and both branches now refuse an offset at or past the length rather than trusting the
  subtraction. No regression test, deliberately stated: the branch cannot be driven from
  a request, so a test would have to make it reachable first.

- **`askr upgrade` extracted the release tarball with the archive's ownership.**
  Extraction runs as root — the install prefix is root-owned, so the command needs sudo —
  and GNU tar as root restores the uid, gid and mode bits recorded in the archive instead
  of the extracting user's. Releases are packaged by a CI runner, so the recorded owner
  was that runner's uid, commonly 1001. On any machine where a local account holds uid
  1001, `/opt/askr/askr` was installed owned by that account, which could then rewrite
  the binary systemd starts as root — local privilege escalation through the one path
  whose whole job is to be trusted.

  Extraction now passes `--no-same-owner --no-same-permissions`, so everything is created
  as root with the umask applied. The permissions half closes the same hole by the other
  route: a mode recorded world-writable, or carrying a setuid bit, was reproduced
  faithfully. No `chown` afterwards — with `--no-same-owner` tar has already created
  everything as the effective uid.

  `scripts/package-release.sh` now records `root:root` in the archive too. The installer
  no longer depends on it, but a published tarball carrying a CI runner's uid is a trap
  for anyone who extracts it by hand as root.

### Changed

- **The release now fails if Packagist isn't serving the version.** Verification stopped at
  "the tag exists in the split repo", which is not the same as installable — Packagist is
  what a user's `composer require` actually talks to. It now polls the public
  `repo.packagist.org` endpoint for up to five minutes and fails with an actionable message.

  No credentials, deliberately. A check that depends on a secret is a check that silently
  skips when the secret is missing, and that is exactly how the split reported green while
  publishing nothing for four months. The `Notify Packagist` step still skips without
  `PACKAGIST_TOKEN` — it is an optimisation — but the *verification* no longer can.

  `scripts/publish-laravel-package.sh` reports the same thing, and adds the one piece of
  context that decides whether anyone is affected: whether the tag points at the same commit
  as earlier tags, in which case the package is byte-identical and anyone on a `^1.x`
  constraint already has the code — an older version *number*, not older code.

  Worth recording a wrong turn. The first version of that note asserted a mechanism: that a
  tag pushed onto an existing commit fires no webhook, so Packagist is never told. It reads
  well and it is contradicted by the evidence — `v1.4.10` through `v1.4.13` all point at that
  same commit and all reached Packagist. Stating it would have sent the next person down a
  path I had already ruled out without noticing. The note now says what is observable and
  calls the cause most likely lag.

### Known issues

- **Shared-memory regions do not survive `exec`.** Every region is `MAP_ANON|MAP_SHARED`
  created before fork, so it is shared across the process tree and gone on restart. For
  the response cache and the rate limiter that is correct — the cache has a persist path
  anyway. For pending jobs it is data loss, logged at shutdown with a count and documented
  with a drain procedure, but still loss. Real persistence means a named mapping
  (`shm_open`) with a `{magic, version, geometry}` header so a re-attached region can be
  validated or rejected. Adding the header alone was considered and skipped: an unused
  version field freezes a layout whose migration has not been designed, which is a worse
  position than having no header. The durable L2 backend (`ASKR_QUEUE_DB`) is the answer
  available today.

- **The sandbox's Landlock ABI is still pinned to V1.** Fail-closed and attestation are
  done (see Added and Fixed); this is the last piece. `landlock_restrict` asks for
  `ABI::V1`, below what current kernels offer (V2 file re-parenting, V3 truncate, V4
  network, V5 ioctl). Negotiating the newest supported ABI needs a Linux build in the
  loop — landlock is a Linux-only dependency and the C shim needs a Linux `cc` to
  cross-check — and guessing at a crate API in a security path is how you ship a build
  break. `/api/status` reports the ABI actually in force, so when this lands it will be
  visible that it did.

- **The response cache refuses what it cannot vary on, rather than varying on it.** A
  response carrying a `Vary` the key cannot express is not cached. Doing it properly means
  a variant list per primary key in `rcache` and a two-phase lookup — the primary key
  cannot be computed from the request alone once the response gets a say in it. Until
  then, `Vary: Accept-Language` costs hit rate instead of correctness.

- **`ASKR_ADMIN_TOKEN` is still opt-in.** The plane warns at startup when it is bound
  off-box without one, and the `Host` and cross-site checks apply with or without it, but
  an unset token still means an open reload trigger to anything that can reach the
  socket. Making it mandatory would break every existing deployment that relies on
  loopback isolation, so it stays a decision rather than a default.

- **`SIGHUP` may leave a worker on old code** ([Askr-51]) — measured once on a live
  deployment, **not reproduced**: the regression test passes 12/12 against the same fleet
  shape. The reasoned diagnosis in that issue is withdrawn, and the mixed content
  observed alongside it is unexplained rather than explained. The deployment that saw it
  recreates its containers on every deploy until it is understood, at the cost of logging
  everyone out.

  Worth recording how close that came to being reported as confirmed: 3 of 10 runs failed
  while writing the test, and the panic was in the **test client**, which dies on a
  connection that goes away mid-read — polling admin during a roll is exactly when that
  happens. A harness that crashes under the conditions it exists to observe reports a
  product failure that isn't one. It makes the reload less trustworthy than it should
  be; the two `doctor --app` faults found in the same pass were fixed in 1.4.14, and this
  one stays open because nobody can say what happened.

- **The e2e suite is not deterministic** ([Askr-53]), and it was measured again today.
  Thirty-two full runs on one machine, same code: 31 green, 1 red, with the failing test
  not captured. Run times are not noise around a mean — they are **tri-modal**:

  ```
  ~5 s   × 11      ~15 s  × 14      80–90 s  × 7
  ```

  Three of the slow runs finished in **90.15 s to the hundredth**, which is not what
  randomness looks like; it is a constant. The e2e client sets a 15-second read timeout
  (`tests/e2e.rs`, `set_read_timeout`), and 90 s is six of those in a row, 15 s is one,
  80 s is five and change. So the working hypothesis is that some test's reads stall
  and the harness waits them out — a slow *pass* that is really the client absorbing a
  failure — and the most likely place is admin polling during a rolling reload, which is
  the exact condition Askr-51 already found the client fragile under. Two things would
  turn this from a hypothesis into a diagnosis: per-test wall times (`--test-threads=1`
  with timestamps), and a client read timeout short enough that a stall fails the test
  instead of slowing it. Recorded here with the numbers because the changelog for 1.4.13
  was right about the habit this teaches: at one in twenty, red pipelines get a re-run and
  a shrug — and today it also got the re-run.

## 1.5.0 — 2026-08-31

A release about failing safely. Three security fixes where the old behaviour was to warn
and carry on, a shared-memory correctness pass, a self-update that now verifies who
produced what it installs, and cache reads that no longer serialise the fleet.

Nothing in the documented surface changed incompatibly; [STABILITY.md](docs/STABILITY.md)
still holds. Two things are worth adopting deliberately rather than by upgrading:
`--sandbox-required` and, if you run behind a local reverse proxy, `ASKR_ADMIN_TOKEN`
(see [Upgrading](docs/UPGRADING.md#to-150)).

### Security

- **`h2` upgraded to 0.4.19** ([RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258),
  unbounded empty DATA frames). A remote peer could hold an HTTP/2 connection open
  sending empty DATA frames without bound — a denial of service against the transport
  Askr serves on by default. Also picked up `chacha20` 0.10.2, replacing a yanked
  release. `cargo audit` is clean again.

- **The scheduler sidecar no longer dies on an error from `schedule:run`, and says what
  happened.** A `TypeError` has been seen escaping it in production — six times over three
  days, always in the second after a scheduled job ran, then nothing for 22 hours across two
  restarts and an upgrade. The events had already run: the failure is on the return path,
  not the work, so nothing was lost.

  The cause is **not known**, and the investigation is worth reading before anyone starts
  again: every command in the application declares `: int`, no `handle()` has an early
  return, there is no `bindMethod` anywhere in the tree, the framework version is identical
  to local, and it does not reproduce. `Kernel::call()` is declared `: int`, so the
  TypeError is thrown inside Laravel as it returns and the offending value never reaches the
  caller — which is why this logs the exception rather than the return value. That is all
  that can be observed from outside.

  Catching earns its place regardless of the mystery: uncaught, this ended the process, the
  supervisor respawned it, and the scheduler missed the boundary it had been sleeping for. A
  cosmetic error should not cost a tick. The message now carries class, file, line, and the
  sentence that would have saved the most time — that scheduled events had already run, so
  the next person does not begin by hunting for lost work.


### Added

- **Cache reads no longer take a lock.** `Cache::get` sampled a per-slot spinlock
  *exclusively*, so every worker reading the same hot key — a session, a shared config
  blob, anything Laravel touches on each request — queued behind the others. Reads now
  use a seqlock: sample a per-slot version counter, copy, sample again; a change means a
  writer overlapped the copy, so retry, and after a bounded number of attempts take the
  lock. Writes are unchanged and still serialised on the spinlock.

  Measured before touching the lock, because a claimed speedup is not a speedup. Both
  paths are kept and benchmarked against each other in one process
  (`cargo test --release -p askr --bins cache_read_scaling -- --ignored --nocapture`),
  reading one key on a 12-performance-core machine:

  | readers | locked | lock-free |
  | --- | --- | --- |
  | 1 | 22.1M/s | 22.9M/s |
  | 2 | 7.3M/s | 10.2M/s |
  | 4 | 6.0M/s | 75.0M/s |
  | 8 | 2.5M/s | 48.5M/s |
  | 12 | 1.5M/s | 19.1M/s |

  The locked column is the finding: throughput *falls* as readers are added, because a
  spinlock under contention burns cycles instead of waiting. Peak aggregate read
  throughput on one key went from ~22M/s to ~75M/s, and the shape changed from
  collapsing to scaling. The lock-free numbers are noisy above 8 threads on a laptop;
  the locked collapse is monotonic and reproduces in every value size.

  Three things the work turned up that are worth recording:

  - The benchmark caught a regression it was not looking for. The first version of the
    read path allocated with `vec![0u8; len]`, which zeroes the buffer before the copy
    — a 4 KB value read by a single thread came out at 0.8× of the locked path it
    replaced, purely from the extra pass over the page. Fixed with an uninitialised
    allocation; single-reader throughput is now 1.0× at every size.
  - The counter cannot share the `lock` word, because shmlock reclaims a slot from a
    dead holder by recognising the PID stored there.
  - `Writing::begin` forces the counter odd rather than incrementing it. A writer killed
    mid-update leaves it odd forever, and an increment would then make it *even* during
    the following write — a reader would sample a stable-looking counter in the middle of
    a copy. Forcing odd on entry and even-and-greater on exit repairs the slot instead.
    There is a test for exactly that: a slot left mid-write is readable via the lock and
    back in phase after the next write.

  `get` also no longer tombstones an expired entry it happens to find, on the lock-free
  path — it takes the lock, re-checks, and then reclaims. Expiry is rare, so this keeps
  the fast path read-only.

  The torn-read test is the one that matters, and it was verified against a defeated
  seqlock: with the two counter samples removed, 584 533 of 2 145 847 reads came back
  half-written. With them, zero.



- **Releases are signed, and `askr upgrade` refuses one that is not.** The trust chain
  used to end at GitHub: the tarball and its `.sha256` came from the same release, so the
  checksum proved the download arrived intact and nothing about who produced it — a
  compromised release, account or CI token serves a matching pair. For a command that
  runs as root and replaces the binary systemd starts, that was the whole of it.

  The release workflow now signs every tarball with minisign and publishes the
  `.minisig` beside it; `upgrade` verifies it against a public key compiled into the
  binary (`keys/release.pub`, `include_str!`'d), streamed so a tarball is never held in
  memory to check. A bad signature, a signature from another key, or no signature at all
  is a refusal, not a warning. Prehashed signatures only — modern minisign produces those
  by default, and refusing legacy mode removes a variant nobody should still be using.

  Releases also carry a SLSA build-provenance attestation (`actions/attest-build-provenance`),
  which answers a different question: minisign says the key holder made this, provenance
  says this workflow built it from this commit. `gh attestation verify` checks the second;
  the binary checks the first, with no network and no GitHub involved.

  **The key is generated by whoever holds it, deliberately not automated.**
  `rsign generate -W -c "askr release signing key" -p keys/release.pub -s
  ~/.askr/askr-release.key` — public half committed, secret half in the repository secret
  `MINISIGN_SECRET_KEY`. A key generated anywhere it could be observed is compromised the
  moment it exists. `docs/RELEASING.md` step 0 has the procedure, including the two parts
  worth reading twice: `-p` is what saves the public key at all (without it rsign prints
  it once and there is no way to get it back), and losing the secret key locks out every
  future release, because installs built against the old key refuse the new tarballs.

  Signing uses rsign2 rather than the minisign C tool. They implement the same format and
  are interoperable — either verifies a release either produced — but rsign2 is what the
  key was generated with, `rsign sign -W` is non-interactive without depending on how a
  passwordless key gets prompted for, and it always prehashes, which is what the verifier
  requires.

  A build with no key committed still upgrades, and says on every run that it checked a
  checksum and not a provenance — an unconfigured build must not be a build that cannot
  upgrade. This one is configured: `keys/release.pub` holds key `5AD94F1DEEDF89FD`, and a
  test asserts it parses, because losing it would quietly return upgrades to
  checksum-only trust and nothing would fail.

  The verification path is tested against minisign's published prehashed test vector
  rather than a round trip, so a mis-parsed format is caught and not just our own code
  agreeing with itself. A second test fails if `keys/release.pub` ever contains that
  vector's *public* key, whose secret half is also published and would look like a
  working signing setup.

  `docs/INSTALL.md` and `docs/UBUNTU.md` now verify the signature before unpacking, which
  is the one install `upgrade` cannot check for you. `SECURITY.md` documents both checks.

- **`--sandbox-required` / `[server] sandbox_required` — fail closed.** The sandbox was
  advisory: a kernel without Landlock or a container without the seccomp capability logged
  a warning and the worker served traffic looking exactly like one that hardened. That
  default stays, because an upgrade that started refusing to boot would be worse than the
  warning, but it can now be opted out of. A worker that cannot fully harden exits 78, and
  the crash-loop guard turns a fleet-wide failure into one clear "giving up".

  It requires `sandbox_write`, and refuses to start without it. Seccomp alone blocks
  `execve`, which is not how a webshell runs here: Askr interprets PHP in-process, so a
  `.php` written into the docroot needs no process creation at all. A "required" sandbox
  without Landlock write rules would be a promise the sandbox cannot keep, so that
  combination is rejected before anything forks rather than discovered per worker.

  The policy is deliberately not Linux-gated — `sandbox::shortfall` decides what a report
  fails to deliver, and is unit-tested on every platform even though only Linux can apply
  anything.


- **A reload is now held to "every worker was replaced"** by a regression test
  (`a_reload_replaces_every_worker`), rather than to `rollout: idle`. It records every PID,
  sends SIGHUP, and polls until no pre-reload PID remains — with sidecars in the fleet,
  since a queue worker recycling on its own is the event most likely to compete with the
  roll. A reload that leaves a worker on the old code serves the previous release from a
  fraction of requests and reports success.

### Fixed

- **A panic in an `extern "C"` entry point no longer takes the worker with it.** There
  were 35 of them across `cache`, `cache_sql`, `squeue`, `squeue_sql`, `broadcast`,
  `broadcast_sql` and the PHP request trampolines, and no `catch_unwind` anywhere in the
  workspace — so a panic at the boundary aborted the process mid-request, and what it was
  about was reported nowhere a person looks. Each now runs inside `ffi::guard`, which
  answers the caller's failure value (a cache miss, a refused push, a 502) and logs the
  entry point by name.

  Shared-memory state survives it because the things protecting it are RAII and run while
  unwinding: `Slot` releases the spinlock. `Writing` needed a change to be safe here —
  its `Drop` marked the slot settled, so catching a panic mid-`write` would have
  advertised a half-updated slot as stable to the new lock-free readers. It now leaves the
  counter odd when dropped during a panic, which sends readers to the lock and lets the
  next writer repair it.

  Not wrapped, deliberately: the two signal handlers in `supervisor.rs` (`catch_unwind` is
  not async-signal-safe, and they touch nothing but atomics), and `cow_ready_trampoline`,
  which forks the fleet — a panic there is a startup failure that has to stay loud.

- **A deleted cache key could come back, so a logged-out session could log itself back
  in.** The probe loop in `Cache::set` holds one slot lock at a time, so two concurrent
  `set`s of the same key can pick different targets — one finds a slot empty that the
  other has since filled, or the two disagree about which live entry is oldest because
  `written_at` moved underneath them. The key then exists in two slots, and `delete`
  returned at the first match: it tombstoned one copy and left the other live, so the
  next lookup found it. For a session key that is a user who logged out and is logged
  in again.

  The comment sitting at the end of `set` claimed the target was re-validated under the
  lock. It was not — the write was unconditional, and the comment described the fix
  that was missing. `delete` now tombstones every match in the chain, and `set` sweeps
  the chain afterwards so duplicates converge to one entry instead of accumulating. The
  sweep takes slot locks in the same ascending order as the probe, so it cannot deadlock
  against one.

  The race is not reproducible on demand, so the regression test plants the duplicate
  directly and asserts the consequence: after `delete`, `get` must return nothing. It
  fails against the old `delete`.

- **A full queue ring dropped jobs in silence.** The no-ring branch of `squeue::push`
  carries the argument for why that is unacceptable — returning 0 is all the PHP API can
  express, Laravel does not check it, so from the application side a lost job looks like
  a job that ran — and then the full-ring branch a few lines down was a bare `0` with no
  log at all. It now reports the queue name and slot count, throttled to once every 30 s
  because a full ring recurs and clears and an operator needs to see each occurrence,
  not just the first since boot.

- **A crash mid-`push` leaked a queue slot permanently.** `id` was the *first* field
  written, and `id != 0` is what makes a slot occupied — so a process that died anywhere
  in the writes that followed left a slot claimed by a job that does not exist: `pop`
  could hand out the previous occupant's payload under the new id, and nothing ever
  frees it. One slot lost per crash until the ring is full of them. `id` is now written
  last as the commit marker, which is the discipline `broadcast::publish` already
  follows with its `seq`; a crash mid-push now leaves `id == 0` and the slot simply
  still free.

- **A recycled PID could wedge a shared-memory region for good.** `shmlock` steals a
  slot lock only from a holder the kernel confirms is dead, which is the right rule and
  rests on `kill(pid, 0)` answering a question it does not answer: it says whether *a*
  process has that number, not whether it is the one that took the lock. A holder can
  die while `pid_max` wraps — minutes on a fork-heavy box — and the number be reused by
  something long-lived, after which every waiter sees a live holder that will never
  release. Unbounded wait, no log.

  A holder whose PID has not changed for ten seconds is now stolen from as well, with an
  error naming it. That is the same steal the module was written to remove, four orders
  of magnitude further out: the old scheme stole after 100–200 µs, shorter than a
  scheduler slice, which is why it corrupted state. A ≤64 KB copy that has not finished
  in ten seconds is not preempted. Tracked in the waiter's own stack, so no extra word
  in the slot and the same behaviour on Linux and macOS.

- **`[server] force_https` in TOML was ignored when validating `http_redirect`.** The
  address was read from the flag *or* the config, and then the guard checked only the
  CLI flag — so a perfectly good TOML setting both keys was refused at startup, by an
  error message that named the config key it was ignoring. The ACME front a few lines
  above already makes this exact distinction, with a comment explaining why.

- **`Host: [::1]:8080` became `"["`.** The port was stripped with
  `authority.split(':').next()`, which truncates an IPv6 literal at its first colon.
  That string became `SERVER_NAME`, the virtual-host routing key, and a field of the
  response-cache key. One helper now does it correctly for all three call sites — and
  for the new admin `Host` check, which had grown a fourth copy of the same logic. Only
  a client addressing the server by IPv6 literal reached it, which is why it survived:
  every test client in this repo uses a name or an IPv4 address.

- **The rate limiter discarded its refill remainder.** `last_ms` jumped to `now`
  whether or not the integer division produced anything, so a client arriving faster
  than one token's worth of milliseconds refilled zero on every call and stayed blocked
  however long it had actually been waiting. `limit < window` makes that ordinary — 10
  per 60 s is 6 s per token, so any polling faster than every 6 ms starved. `last_ms`
  now advances only by the time the refill accounts for. Low severity, and a real
  regression test: `consume` takes `now`, so the test drives 7 000 fabricated
  milliseconds instead of sleeping for six seconds.

- **`Accept-Encoding: br;q=0` was served brotli.** `q=0` is not a weak preference, it
  is a refusal, and `starts_with("br")` matched `br;q=0` exactly as happily as `br`.
  Tokens are now matched exactly — `starts_with` also accepted `brotli`, which nobody
  serves — and a `q=0` token is treated as not offered. Ranking is unchanged: br before
  gzip, other q-values still ignored.

- **Shutting down with jobs still in the queue is now on the record.** The ring is an
  anonymous shared mapping: it lives as long as the process tree and has no persist
  path, so a restart — `askr upgrade` included — comes up empty and the jobs that were
  in it never run. Nothing in the application sees an error, which is how this reads as
  "Laravel lost the mail". The master now logs an error naming the number of jobs being
  lost, counted after every worker is reaped so the region is quiescent and the count is
  exact. `docs/MAINTENANCE.md` gains a drain procedure and `docs/UPGRADING.md` gains it
  as a step in the upgrade sequence.

- **`PURGE`/`BAN` were open to the internet behind a local reverse proxy.** With no
  `ASKR_ADMIN_TOKEN` set, cache invalidation was accepted from loopback peers — which
  is a sound rule for a server that is its own front door and no rule at all behind
  nginx or Caddy on 127.0.0.1, where *every* request arrives from loopback. Anyone
  could then send `BAN` with `X-Ban-Url: /*` and empty the cache on demand.
  `trusted_proxies` is the operator stating in writing that loopback is where the
  proxy sits, so once it is set the loopback fallback no longer applies and a token is
  required.

- **The SSE bridge subscribed to `private-` and `presence-` channels without
  authenticating them.** `pusher.rs` HMAC-verifies a subscription to those prefixes
  before adding it to a socket; `GET /askr/events?channel=private-orders` did no such
  thing and streamed everything published on the channel to whoever asked. Two
  transports for one channel namespace, one of them enforcing the rule.

  The SSE path has no socket id and no signature to verify one against, so it cannot
  honour the same check — it now refuses those prefixes with `403` instead. Public
  channels are unaffected. Signed SSE subscriptions would be the feature; declining to
  be the hole in the meantime is the fix.

- **The admin plane accepted DNS-rebound reads and cross-site reloads.** Neither
  needed the token to be unset to work, which is why both checks now apply whether or
  not one is configured.

  `Host` was never looked at. A page on an attacker's domain re-resolves its own
  hostname to 127.0.0.1; the browser then treats `http://evil.test:9000/api/status` as
  same-origin and hands the response — PIDs, RSS, error records — to the attacker's
  script. Nothing in that request looks cross-site, because to the browser it isn't:
  the only thing that gives it away is `Host: evil.test` naming a listener that is not
  called that. A loopback-bound plane now requires a `Host` that names it, with
  `ASKR_ADMIN_HOSTS` for a proxy that forwards its own.

  And `POST /api/reload` is a CORS "simple request" — no custom headers, so no
  preflight, so CORS never got a say and any web page could roll the fleet. Requests a
  browser reports as cross-site (`Sec-Fetch-Site`, or an `Origin` that disagrees with
  `Host`) are refused. `curl` and deploy scripts send neither header and keep working:
  this refuses what identifies itself as cross-site rather than demanding proof of not
  being a browser.

- **The admin reload and an ACME renewal bypassed the canary gate.** `[reload] canary`
  was honoured by the SIGHUP handler and by nothing else: `trigger_reload()` — the
  admin API, a renewed certificate, the cert-mtime watcher — called `roll_next()`
  directly and rolled the whole fleet with no health check. Those are exactly the
  reloads that happen with nobody watching, so they are the ones that needed the gate
  most. Both paths now enter through it.

- **The response cache ignored the application's `Vary`, and kept no scheme in the
  key.** Three defects with one cause: the key can express negotiated encoding and
  device class, and everything else the response said about its own variance was
  dropped.

  `Vary: Accept-Language` from a localised Laravel app meant the first visitor's
  language was cached and served to everyone. Such responses are now not cached at
  all. That costs hit rate on exactly the responses that were being served wrong, and
  honouring `Vary` properly needs a two-level lookup in `rcache` — a variant list per
  primary key — which is a design change, not a patch. Recorded under known issues.

  Scheme is now part of the key. Without it one entry was shared by http and https, so
  with `force_https` off a page holding absolute URLs (`url()`, `asset()`, a canonical
  tag) could be rendered over http and then served to https clients with http links
  baked in. It is appended last, because `rcache::key_parts` reads the first three
  fields — `PURGE` and `BAN` keep working and stay scheme-agnostic, which is what
  anyone purging a URL means.

  And a response the app had gzipped itself was cached as garbage: `storable_header`
  drops `Content-Encoding`, and `compress::maybe` returns an already-compressed body
  unchanged because re-compressing it comes out larger — so the entry held gzip bytes
  with nothing declaring them, and every hit sent binary to the browser. Those are
  refused too.

- **Upload temp files could be streamed into a directory another local user owned.**
  `$TMPDIR/askr-uploads` is a fixed, world-known path, and on a shared host somebody
  else can create it first. Every result that would have revealed it was discarded:
  with `recursive(true)` an existing directory is not an error, `set_permissions()` on
  a directory owned by another user fails with EPERM, and both were `let _ =`. Uploads
  — whatever people type into forms — then landed somewhere readable by its owner, who
  could also substitute an entry with a symlink between our create and PHP's read.

  The directory is verified now rather than assumed: `lstat`, owned by this process,
  no access for anybody else, and a chmod that is checked instead of trusted. When the
  shared path isn't ours the server uses a private `askr-uploads-<uid>-<pid>` beside it
  and says so, because an image that pre-creates `/tmp/askr-uploads` as root and then
  drops to www-data is a legitimate setup and should not take uploads down. Files are
  created `O_CREAT|O_EXCL|O_NOFOLLOW` at 0600.

- **`X_Forwarded_For` and `X-Forwarded-For` became the same `$_SERVER` key.** Header
  names were upper-cased with dashes replaced by underscores, so both spellings
  collapsed to `HTTP_X_FORWARDED_FOR` and which one PHP saw depended on header
  iteration order. Anything filtering the dashed spelling — a WAF, a proxy that
  rewrites the header, Laravel's `TrustProxies` reading `$_SERVER` — was bypassed by
  sending the underscored one. An underscore in a header name is now dropped rather
  than merged, which is the same default nginx ships as `underscores_in_headers off`.

- **The crash-loop guard did not count a worker killed by a signal.** It tested
  `WIFEXITED && WEXITSTATUS != 0`, so a worker that segfaulted on boot — the loudest
  form of the thing the guard exists to stop — respawned forever. Worse than not
  counting: it took the healthy branch and *cleared* the streak, so a fleet mixing
  fatals and faults never accumulated one either. Fault signals now count
  (`SIGSEGV`/`BUS`/`ILL`/`FPE`/`ABRT`/`SYS`/`TRAP`); `SIGTERM` deliberately does not,
  because every intentional termination in `supervisor.rs` uses it and a rolling reload
  must not look like a crash-loop. `SIGSYS` is in the list because a seccomp filter
  killing the worker is a boot loop like any other.

- **ACME wrote the key and the certificate in place, one after the other.** A worker
  spawning or reloading in between read a new key against the old certificate and
  failed to start; the window was the length of two file writes. Both are staged and
  renamed now, key first and certificate last — the cert-mtime watcher keys on the
  certificate, so by the time anything notices a change the matching key is already
  there. The key's temp file carries 0600 from creation rather than being tightened
  afterwards.

- **A stale read offset could have handed heap memory back as a request body.** In
  worker mode the offset `php://input` reads from was never reset between requests:
  `askr_req_reset()` freed the body and zeroed its length and never touched the third
  variable, which was declared thirty lines away beside the SAPI callback that consumed
  it rather than beside the state it belongs to. A shorter body arriving after a longer
  one then evaluated `w_body_len - w_body_off_read` in `size_t` — which does not go
  negative, it underflows to something near `SIZE_MAX` — so `n` became whatever PHP
  asked for and the `memcpy` read past the end of the allocation. It is a read, so
  nothing crashes: it returns the process's own heap as the body of a request, and an
  application will echo it.

  Not exploitable as shipped, and the reason matters. PHP only calls the post reader
  when `SG(request_info).content_length` is set, and the worker path never sets it — the
  body reaches the worker script as `$request['body']`, and `examples/laravel-worker.php`
  builds the Request from that. So the worker branch of `askr_read_post` is unreached
  today. It was one assignment from live: setting `content_length` is the first thing
  anyone making `php://input` work natively in worker mode would do.

  Fixed in three places, because the one-line version is the one that comes back. The
  offset now sits with `w_body` and `w_body_len` where `askr_req_reset()` can see it; it
  is reset there and in `askr_req_set_body()`, matching what the one-shot path has always
  done, where `g_req.body_off = 0` sits on the line after `g_req.body_len = body_len`;
  and both branches now refuse an offset at or past the length rather than trusting the
  subtraction. No regression test, deliberately stated: the branch cannot be driven from
  a request, so a test would have to make it reachable first.

- **`askr upgrade` extracted the release tarball with the archive's ownership.**
  Extraction runs as root — the install prefix is root-owned, so the command needs sudo —
  and GNU tar as root restores the uid, gid and mode bits recorded in the archive instead
  of the extracting user's. Releases are packaged by a CI runner, so the recorded owner
  was that runner's uid, commonly 1001. On any machine where a local account holds uid
  1001, `/opt/askr/askr` was installed owned by that account, which could then rewrite
  the binary systemd starts as root — local privilege escalation through the one path
  whose whole job is to be trusted.

  Extraction now passes `--no-same-owner --no-same-permissions`, so everything is created
  as root with the umask applied. The permissions half closes the same hole by the other
  route: a mode recorded world-writable, or carrying a setuid bit, was reproduced
  faithfully. No `chown` afterwards — with `--no-same-owner` tar has already created
  everything as the effective uid.

  `scripts/package-release.sh` now records `root:root` in the archive too. The installer
  no longer depends on it, but a published tarball carrying a CI runner's uid is a trap
  for anyone who extracts it by hand as root.

### Changed

- **The release now fails if Packagist isn't serving the version.** Verification stopped at
  "the tag exists in the split repo", which is not the same as installable — Packagist is
  what a user's `composer require` actually talks to. It now polls the public
  `repo.packagist.org` endpoint for up to five minutes and fails with an actionable message.

  No credentials, deliberately. A check that depends on a secret is a check that silently
  skips when the secret is missing, and that is exactly how the split reported green while
  publishing nothing for four months. The `Notify Packagist` step still skips without
  `PACKAGIST_TOKEN` — it is an optimisation — but the *verification* no longer can.

  `scripts/publish-laravel-package.sh` reports the same thing, and adds the one piece of
  context that decides whether anyone is affected: whether the tag points at the same commit
  as earlier tags, in which case the package is byte-identical and anyone on a `^1.x`
  constraint already has the code — an older version *number*, not older code.

  Worth recording a wrong turn. The first version of that note asserted a mechanism: that a
  tag pushed onto an existing commit fires no webhook, so Packagist is never told. It reads
  well and it is contradicted by the evidence — `v1.4.10` through `v1.4.13` all point at that
  same commit and all reached Packagist. Stating it would have sent the next person down a
  path I had already ruled out without noticing. The note now says what is observable and
  calls the cause most likely lag.

### Known issues

- **`squeue` `delete`/`release` are unfenced against an expired lease.** `pop` reserves
  a job by moving `reserved_until` and leaves `id` alone, so once a lease lapses another
  worker takes the same job under the same id. A delayed first worker then calls
  `delete(id)` and acks the job the second one is still running, or `release(id, delay)`
  and makes it immediately poppable while two are already on it. Nothing detects the
  stale ack because there is nothing to detect it with.

  The fix is a lease generation in the slot, carried in `Reserved` and required by
  `delete`/`release`. That crosses the FFI: `askr_queue_delete_fn(long id)` and
  `askr_queue_release_fn(long id, long delay)` in `shim.c`, the Rust bridge, and the
  Laravel queue driver in `packages/laravel` all take the id alone. A signature change
  through three layers is not something to bury in a batch of small fixes, so it is
  written down rather than done.

- **Shared-memory regions do not survive `exec`.** Every region is `MAP_ANON|MAP_SHARED`
  created before fork, so it is shared across the process tree and gone on restart. For
  the response cache and the rate limiter that is correct — the cache has a persist path
  anyway. For pending jobs it is data loss, now logged and documented (see Fixed) but
  still loss. Real persistence means a named mapping (`shm_open`) with a
  `{magic, version, geometry}` header so a re-attached region can be validated or
  rejected. Adding the header alone was considered and skipped: an unused version field
  freezes a layout whose migration has not been designed, which is a worse position than
  having no header. The durable L2 backend (`ASKR_QUEUE_DB`) is the answer available
  today.

- **The sandbox's Landlock ABI is still pinned, and its applied state is not
  attested.** Fail-closed is done (see Added), and the two remaining pieces of that
  known issue are not. `landlock_restrict` still asks for `ABI::V1`, below what current
  kernels offer (V2 file re-parenting, V3 truncate, V4 network, V5 ioctl); negotiating
  the newest supported ABI needs a Linux build in the loop, because landlock is a
  Linux-only dependency and the C shim needs a Linux `cc` to cross-check — guessing at
  a crate API in a security path is how you ship a build break. And `/api/status` still
  reports the sandbox's *intent* from config rather than what the workers achieved,
  which is what an operator would need to tell a hardened fleet from a partly hardened
  one.
- **The response cache refuses what it cannot vary on, rather than varying on it.** A
  response carrying a `Vary` the key cannot express is not cached (see Fixed). Doing it
  properly means a variant list per primary key in `rcache` and a two-phase lookup —
  the primary key cannot be computed from the request alone once the response gets a
  say in it. Until then, `Vary: Accept-Language` costs hit rate instead of correctness.

- **`ASKR_ADMIN_TOKEN` is still opt-in.** The plane warns at startup when it is bound
  off-box without one, and the `Host` and cross-site checks now apply with or without
  it, but an unset token still means an open reload trigger to anything that can reach
  the socket. Making it mandatory would break every existing deployment that relies on
  loopback isolation, so it stays a decision rather than a default.

- **`SIGHUP` may leave a worker on old code** ([Askr-51]) — measured once on a live
  deployment, **not reproduced**: the new test passes 12/12 against the same fleet shape.
  The reasoned diagnosis in that issue is withdrawn, and the mixed content observed
  alongside it is unexplained rather than explained. `scripts/deploy.sh` uses
  `--force-recreate` until it is understood, at the cost of logging everyone out per deploy.

  Worth recording how close that came to being reported as confirmed: 3 of 10 runs failed
  while writing the test, and the panic was in the **test client**, which dies on a
  connection that goes away mid-read — polling admin during a roll is exactly when that
  happens. A harness that crashes under the conditions it exists to observe reports a
  product failure that isn't one.

  It makes the reload less trustworthy than it should be. The two `doctor --app` faults
  listed here alongside it were fixed in 1.4.14; this one is still open because nobody
  can say what happened.

## 1.4.14 — 2026-08-14

### Fixed

- **`doctor --app` reads the environment the application will actually see** ([Askr-52]).
  It parsed `.env` from disk, which in any container deployment is the source that *loses*:
  Laravel's Dotenv skips a variable that already exists, so real environment variables win.
  Values now resolve the same way the app resolves them, and every report names its source
  (`environment` or `.env`) so the reader can tell whether doctor is looking at the same
  thing the workers are.

  A precision that had to be corrected mid-implementation: an **empty** real variable does
  not fall back to `.env`. Dotenv will not overwrite it, so the application sees the empty
  string. Reporting the `.env` value would describe something the app never uses — and that
  exact shape (`TOKEN: ${TOKEN}` in a compose file with an empty entry in its own `.env`) is
  how an admin plane ended up unauthenticated while the file it was configured from looked
  populated. Now a test.

- **The scheduler check no longer matches any method named `command()`** ([Askr-52]). It
  looked for `->command(` or `::command(` anywhere, and on the application it was written
  against the only match was `Artisan::command()` — which *defines* a console command and
  schedules nothing. It reported that scheduled tasks would fail on an app that scheduled
  none. The conclusion happened to be true for other reasons, which is worse than being
  wrong: right answer, false evidence, and it looks verified.

  Now anchored on `Schedule::command(` and `schedule->command(`, and it also reads
  `bootstrap/app.php`, where Laravel 11+ puts scheduling inside `withSchedule()`.

  **This is a trade, not a free improvement.** The old pattern cried wolf; the new one will
  miss scheduling registered from a service provider or a package. It is the right trade
  because a check that cries wolf gets skimmed, and then the real findings beside it are
  skimmed too — but it is a narrower check than it was.

### Withdrawn

- The claim that Askr's admin-plane warning asserts protection it has not verified
  ([Askr-54]) was **wrong**, and is retracted. `admin.rs` already filters an empty token to
  `None` and already has a separate, correctly-worded warning for the unauthenticated case.
  Verified by running it both ways.

  The mistake was measurement, not reasoning: the token's length was read with
  `printf %s "${ASKR_ADMIN_TOKEN:-<TOM>}" | wc -c`, and `<TOM>` is five characters — a
  fallback string mistaken for a five-character token. The log line quoted as evidence had
  been captured *after* the token was set, so it was accurate. The exposure it was found
  alongside was real and is fixed; the diagnosis of why Askr had not warned was not, because
  Askr had warned, in the exact words the issue asked for, and nobody read it.

## 1.4.13 — 2026-08-14

**Queue workers with no slots discarded every job in silence.** If you run queue workers,
check that `[queue] slots` (or `--queue-slots`) is set — Askr now refuses to start without
it rather than letting mail disappear.

### Fixed

- **`queue.workers` without `queue.slots` is now refused.** The ring is only mapped when
  slots are configured. Without it `askr_queue_push()` returns 0, Laravel does not check the
  return value, and every queued job — password resets, invitations, all outgoing mail — was
  discarded with no exception, no log line, and nothing in the queue to age. Queue workers
  ran happily, polling a ring that did not exist.

  This is the mirror of a bug fixed a week ago from the other side, where slots were
  configured and no worker consumed them. Both are now handled: workers without slots is an
  error naming the consequence; slots without a worker stays legal (something outside the
  instance may consume them) but warns, because far more often it is the same mistake.

  The CLI gets the same check: `--queue-script` without `--queue-slots` refuses to start.

- **A discarded push now says so.** `push()` into an unmapped ring logs an error naming the
  queue, once per process. Returning 0 is all the PHP API can express and the framework
  ignores it, so from the application side the loss was invisible. A job that goes nowhere
  must not be quieter than one that fails.

  The backlog watchdog added in 1.4.11 could not help here — it warns about jobs that are
  *waiting*, and these never got far enough to wait. Worth noting for anyone relying on it:
  an empty queue means either nothing to do or nothing arriving, and until now those looked
  identical.

### Also fixed

- **The `squeue` unit tests shared the job ring with no serialization.** `init()` maps the
  slot table once and every push lands in it, so unique queue names kept the counts apart but
  not the table — and `by_queue()` walks all of it. `cache.rs` has had a `TEST_GUARD` mutex
  for exactly this reason; squeue did not, and passed for weeks on scheduling luck.

  Found while checking two Dependabot bumps. The suite failed 3 of 8 runs with both merged,
  and the obvious conclusion was that a bump had broken something. The control says
  otherwise: `main` clean 8/8, each PR clean 8/8 alone, and the crates involved are `cc`,
  `clap`, `rusqlite`, `rcgen` — nothing that touches the async runtime or IO. The bumps
  changed compile output, which reshuffled test timing, which made a latent problem visible.
  **A dependency bump exposed it; it did not cause it.**

### Known issues

- **The e2e suite is not deterministic** ([Askr-53]) — roughly one run in twenty, spread
  across four timing-sensitive tests now named in that issue, with run times varying from 8
  to 90 seconds on the same machine. Recorded as a bug with a measured rate rather than the
  vague caveat it has been, because at one in twenty the habit it teaches is to re-run red
  pipelines, and once that habit exists a real regression gets one re-run and a shrug.

## 1.4.12 — 2026-08-07

### Fixed

- **`iconv` is now compiled in.** `bacon/bacon-qr-code`, which Laravel Fortify uses to draw
  two-factor QR codes, declares `ext-iconv` and calls `@iconv()`. A missing function is a
  fatal `Error` in PHP 8 and `@` does not suppress it, so the whole page answered 500:

  ```
  production.ERROR: Call to undefined function BaconQrCode\Encoder\iconv()
  ```

  It hit both enrolment screens, including the forced one after the two-factor grace period
  expires — the one screen a user cannot get past. `--without-iconv` was in the configure
  line with no comment beside it, unlike its neighbours, so it looks like it came along with
  the `--disable-all` sweep rather than being a decision.

  No new dependency on Linux: glibc has iconv in libc, and the runtime image is
  `ubuntu:24.04`. **macOS is not so simple**, which the one-line change would have missed —
  a bare `--with-iconv` fails there with *"Please specify the install prefix of iconv"*,
  because the header and libiconv live under the SDK rather than `/usr`. The flag is now
  OS-dependent, and falls back to building without iconv (with a warning naming the
  consequence) if the SDK header is absent, since that target is dev and test rather than
  what ships.

  Verified by building and calling it, not by trusting configure: `function_exists('iconv')`
  is `true` and `iconv('UTF-8', 'ASCII//TRANSLIT', 'æøå')` transliterates. `askr doctor`
  reports `✓ ext-iconv (recommended)` — recommended, not required, because an app with no QR
  codes does without.

- **`PROFILE=minimal` was broken on macOS.** `"${DEP_FLAGS[@]}"` expands an empty array,
  which bash 3.2 — the version macOS ships — treats as an unbound variable under `set -u`:
  `DEP_FLAGS[@]: unbound variable`. The profile the test suite uses could not build there.
  Found by rebuilding to verify the change above, which is the only reason it surfaced.

## 1.4.11 — 2026-08-05

**Breaking silence.** Every failure worth an afternoon on this project has been silent: a
queue with no consumer, a worker polling the wrong queue name, a mailer configured under the
vendor's variable name instead of Laravel's, a scheduler shelling out to a binary the image
does not contain, a stylesheet that 404s behind a year-long browser cache. None of them
produced an error. The server held every number needed to know, and said nothing. This
release is a watchdog that speaks up, a pre-flight check that refuses, per-queue numbers
that can't be misread, and a smoke test that looks for the absence of the unexpected.

### Added

- **Backlog watchdog.** The master now warns when jobs sit available and unclaimed for more
  than 30 seconds, **naming the queue**:

  ```
  WARN queue backlog is not being consumed queue=mail pending=1 oldest_secs=144 queue_workers=2
       — no worker is taking jobs from this queue. Check that a queue worker is running
       (--queue with --queue-script) and that it polls this queue name (ASKR_QUEUE).
  ```

  This is the failure that prompted it: an app queued its password-reset and invitation
  mail to `onQueue('mail')` while the only worker polled `default`. Mail stopped. No
  exception, no log line, and a worker asleep in `nanosleep` — the diagnosis came from
  `/proc/<pid>/wchan`, which is not where anyone should have to look.

  Naming the queue required storing the name in the ring; only the hash was there, which
  routes jobs perfectly and diagnoses nothing. The aggregate count was actively misleading:
  "1 job ready" was true and said nothing about *which* queue. Warns once a minute per
  queue, and forgets a queue as soon as it drains so a recurrence is reported immediately.
  Runs regardless of autoscaling — a fixed-size pool is exactly where this goes unnoticed.

- **Per-queue counts in `/api/status`.** A `queues` array with `pending`, `delayed`,
  `reserved` and `oldest_pending_secs` for every queue holding a job. The aggregate
  `queue_ready` is what hid the failure above: "1 job ready" is true whether the job is on a
  queue a worker polls or one nobody listens to. Queue names come from the application, so
  they are the only field in that document that isn't machine-generated — they are escaped,
  with a test, because an app may name a queue anything it likes.

- **`scripts/smoke.sh <url> [admin-url] [token]`** — a post-deploy check whose every entry is
  a failure that shipped: an empty 200, a page that worked once per worker, a form that lost
  its fields, `localhost` in URLs over HTTP/2, an asset referencing a build that no longer
  exists, a stale queue backlog. Exits with the number of failures so CI can gate on it.

  **It found a real fault the first time it ran against production**: the home page
  referenced a stylesheet that 404s. Invisible in a browser, because every browser still had
  the file cached under `immutable, max-age=1 year`. The cause is worth knowing — Laravel
  caches Vite's `manifest.json` per process, so workers that booted before `npm run build`
  serve the previous build's filenames indefinitely. A reload fixed it; it is now in the
  worker-mode symptom index and in the deploy order.

  Writing it produced its own lesson. The asset check originally tested only the first
  reference it found, so it caught the 404 on one run and missed it on the next. A check that
  intermittently notices a real failure is barely better than none; it now checks every
  referenced asset.

- **`askr doctor --app <path>`** checks the application against the environment it will run
  in, and exits non-zero so it can gate a deploy. It greps `app/` for `onQueue()` and
  `$queue =` and compares them with `ASKR_QUEUE`; flags `SESSION_DRIVER=askr` without slots
  (which loses sessions quietly and surfaces as 419 on every form); catches
  `MAIL_MAILER=resend` with neither `RESEND_KEY` nor `RESEND_API_KEY`; and warns that
  scheduled `->command()` tasks shell out to a `php` binary the image does not have.

  Verified against a real 235-file application: it found `imports, mail, webhooks` and would
  have failed this morning's deploy with the exact fix in the message.

  Output distinguishes `•` observation from `✓` verified from `✗` failure. A tick on "this
  needs `--cache-large-slots`" claimed something was confirmed when nothing was — doctor
  cannot see the flags a later `serve` will get, and a tick that means "noted" teaches you
  to skim ticks.


- **Documented that WebSocket requires HTTP/1.1** ([Askr-49]). HTTP/2 forbids the
  `Connection` and `Upgrade` headers, so the upgrade check never matches, the request falls
  through to the front controller, and Laravel answers a perfectly correct **404** with
  nothing in the log. Since TLS negotiates h2 by default via ALPN, that is the *default*
  path for any client that doesn't ask for 1.1 — found while verifying a deployment over h2
  on purpose, because h2 has hidden a bug here before.

  Browsers are unaffected: Echo and pusher-js use the browser's WebSocket API, which does
  1.1 for this regardless of the page's protocol. What it breaks is test clients, which then
  conclude the endpoint doesn't exist.

  The fix is RFC 8441 extended CONNECT, tracked in the issue. Not the tempting shortcut of
  answering 426 when `/app/…` is requested over h2 without upgrade headers: `/app` is an
  extremely common Laravel route namespace — the deployment this was found on serves its
  whole dashboard there — and the only thing separating a WebSocket attempt from a real page
  is the header h2 doesn't send. That heuristic would turn working pages into errors.


- **`docs/MAINTENANCE.md`** — what to do after the server is running, which no existing page
  covered: the 30-second check and which `/api/status` fields actually matter (`respawns`
  climbing by itself is the most informative number on the box), reload-not-restart and why
  (shared memory holds the sessions, cache and queue), the three independent caches and the
  browser one that isn't on the server, certificate checks, backups of the three things that
  matter, capacity starting points, and a monthly ten-minute list whose normal outcome is
  "nothing to do".

  Two entries are there because they cost real time: **access and traffic logs never
  rotate** — append-only descriptors that will fill a disk, so the logrotate recipe needs
  `copytruncate` or a renamed file goes on receiving lines invisibly — and a "things not to
  do" table where every row has actually gone wrong.

- **Fixed a stale liveness recommendation in `docs/UBUNTU.md`.** It told you to probe
  `/api/status`, which 1.4.2 gated behind `ASKR_ADMIN_TOKEN` — so following the docs and
  then setting a token made every orchestrator declare a healthy server unhealthy. That is
  the exact failure `/healthz` was added to remove, and the page still pointed at the wrong
  endpoint.

## 1.4.10 — 2026-08-05

Two configurations that were unreachable rather than merely awkward.

### Added

- **`[acme]` — auto-TLS from a config file** ([Askr-47]). ACME was flags-only, and since
  1.4.6 `--config` is the whole configuration rather than a set of defaults, so auto-TLS
  and a config file were mutually exclusive. That made real setups impossible to express:
  `trusted_proxies` has never had a flag, so "auto-TLS behind a proxy" could not be
  written down at all. Every flag now has a key — `enabled`, `domains`, `email`, `dir`,
  `staging`, `directory_url`, `http`, `ca_root`.

  The section refuses to start on the mistakes that would otherwise end with a site
  quietly serving plain HTTP: `domains` without `enabled` (TOML defaults a missing bool to
  false, so the file *looks* like it asked for TLS), `enabled` without `domains`, `[acme]`
  alongside `[tls]`, and a wildcard domain — HTTP-01 cannot validate one, and finding that
  out from a rate-limited Let's Encrypt rejection is a poor way to learn it.

  Two things fell out of writing it. The redirect front was started with the *CLI*
  `--force-https` flag, which is empty on the config path — auto-TLS from a file would have
  silently stopped redirecting HTTP. And the resolved config reported `https = false` with
  ACME enabled, because `https` was only implied by a certificate on disk; anything reading
  it before the ACME step ran, logging and admin status included, said plain HTTP. Both
  found by tests written for the feature rather than by review.

  `--config` still refuses to run alongside flags it would ignore. The difference is that
  the error is now actionable: there is somewhere to move them to.

- **Real queue introspection: `askr_queue_stats()`** ([Askr-48]). Laravel 13's `Queue`
  contract asks for pending, delayed and reserved counts plus the oldest pending job's age.
  1.4.9 could only answer the first, so `queue:monitor` saw no delayed backlog at all —
  honest, but still an operator watching a flat line that meant nothing.

  Queue entries gained a `created_at`, and one pass over the slot table buckets every job.
  Reading all four together is the point: with separate calls, a job that becomes available
  in between can be counted twice or not at all, and a dashboard built on numbers that
  don't add up is worse than no dashboard. The test asserts the sum invariant, not just the
  individual counts.

  The package uses it when the server offers it and keeps 1.4.9's honest fallbacks
  otherwise, since it supports servers older than itself. `size()` and `pendingSize()`
  remain identical, so existing `queue:monitor` thresholds keep their meaning.

  The `--features sql-backend` build caught what review didn't: the L2 SQLite queue
  registers the same bridge and needed its own implementation. It has both backends
  answering to one invariant test now — and its table stores seconds where shared memory
  stores milliseconds, so the test asserts the unit rather than trusting it.

### Also in this release

- **CI now tests the Laravel package against every Laravel major it claims to support.**
  Nothing ever had. `packages/laravel/composer.json` has declared
  `illuminate/*: ^11 || ^12 || ^13` since 1.4.0, and the first thing to check any of it was
  a production 502: Laravel 13 added four methods to the `Queue` contract and `AskrQueue`
  didn't have them.

  `packages/laravel/tests/contracts.php` loads every class in the package — which is the
  whole test, since PHP raises a fatal at *link* time when a concrete class is missing an
  abstract method — and names the interface and method when something is absent. A CI
  matrix runs it under Laravel 11, 12 and 13. Verified by deleting the 1.4.9 fix and
  watching it fail.

- **`docs/WORKER_MODE.md` gained a symptom → cause index.** Every worker-mode bug shipped
  and fixed this week, keyed by what you actually see: interactivity that dies after the
  first page load, an anonymous visitor served as somebody else, 419 on every form, empty
  file downloads, `localhost` in generated URLs. They share a shape — state the framework
  expects to be thrown away survives, and the failure is silent — so the index ends with
  the two lessons that cost the most time: a clean console doesn't mean working
  JavaScript, and a 200 doesn't mean a body.

- **`docs/FEATURES.md` documents pointing a Reverb-scaffolded app at Askr's WebSocket**
  (`REVERB_*` env → `--pusher`), including that `VITE_REVERB_*` is baked into the bundle at
  build time, so `.env` has to be right *before* `npm run build`. Both learned on a real
  deployment; the WebSocket handshake is verified.

## 1.4.9 — 2026-08-05

**`kwhorne/askr-laravel` was broken on Laravel 13.** Fatal on any page that touched the
queue — sending mail, most visibly. Upgrade the package: `composer update
kwhorne/askr-laravel`. No server change.

### Fixed

- **`AskrQueue` now implements Laravel 13's full `Queue` contract.** Laravel 13 added
  `pendingSize()`, `delayedSize()`, `reservedSize()` and
  `creationTimeOfOldestPendingJob()`. A class missing an abstract method is a **fatal at
  load time**, so the queue driver killed the worker the moment anything resolved it —
  surfacing as `askr: php worker died mid-request` and a 502, not a graceful error. The
  package claimed `illuminate/queue: ^13` support since 1.4.0 and didn't have it.

  `pendingSize()` maps exactly onto what Askr counts (available now, delay elapsed, no
  live reservation). The other three are **deliberately understated**: Askr's
  shared-memory queue tracks delay and reservation per entry but exposes only
  `askr_queue_size()` to PHP, so `delayedSize()`/`reservedSize()` return 0 and
  `creationTimeOfOldestPendingJob()` returns `null` — the contract's documented "unknown".
  Returning invented numbers to `queue:monitor` would be worse than returning none.
  Proper introspection needs a new server-side function; filed separately.

### Worth noting

The 502 was informative rather than mysterious, and that was the point of 1.4.4's
diagnosis work: the log named the class, the missing methods, the file and the line. Six
releases ago the same failure would have printed "fatal/OOM?" and sent someone looking at
memory limits.

## 1.4.8 — 2026-08-05

**Livewire's JavaScript vanished after the first request per worker.** If you run Livewire
(or Flux, or anything depending on Alpine) in worker mode, upgrade — and take the new
worker script with it.

### Fixed

- **`examples/laravel-worker.php` now calls `Livewire::flushState()` between requests.**
  Livewire tracks in a container singleton whether it has already emitted its
  `<script>` tag. In a long-lived worker that flag stayed set, so **only the first
  response from each worker included `livewire.js`** — and since Alpine ships inside that
  bundle, every later page had `x-data`, `x-show` and `wire:` silently doing nothing.

  The console stayed clean, which is what made it hard: nothing failed, the script simply
  wasn't there. With four workers the site appeared to work for the first few page loads
  and then stopped — reported as "it works for a brief moment". What finally gave it away
  was a Flux dark-mode toggle rendering **both** its sun and moon icons at once: two
  `x-show` directives, neither evaluated.

  Livewire already knows how to reset this — `flushState()` fires the `flush-state` hook
  that resets the flag, the same mechanism Octane relies on. Askr's worker script just
  never asked. Verified on a real deployment: the script tag went from 0 of 10 page loads
  to 10 of 10.

## 1.4.7 — 2026-08-05

**Upgrade if you serve HTTPS.** HTTP/2 requests lost the host they were addressed to, and
three separate things went wrong as a result. ALPN negotiates h2 by default over TLS, so
this affected every TLS deployment.

### Fixed

- **The request host is now read from `:authority` when there is no `Host` header.**
  HTTP/2 and HTTP/3 don't send `Host`; the authority arrives as a pseudo-header, which
  hyper exposes on the URI. Askr read only `Host`, so over h2 it fell back — differently,
  and wrongly, in three places:

  - **`HTTP_HOST`/`SERVER_NAME` became `localhost`.** Laravel builds URLs from the
    request, so every redirect and generated link pointed at `https://localhost/…`. On a
    real deployment, logging in landed the user on `https://localhost/two-factor/setup`.
  - **Virtual-host matching saw an empty host** and fell through to the default site. With
    `[[site]]` vhosts, an h2 request for one domain could be served another's application.
  - **The response-cache key had an empty host component**, so two domains could share
    cache entries.

  One helper (`cgi::effective_host`) now serves all three, with unit tests for the h1, h2,
  empty-header and neither-present cases.

### Why it took this long to find

Every test client in this repo speaks HTTP/1.1 — the e2e suite's own client, and the curl
invocations in every previous verification. The multi-domain soak that drove 23 million
requests through two hostnames with zero mis-routes ran entirely over h1. The bug needed
TLS *and* a check of something derived from the host, and until Askr terminated TLS itself
on a real server, nothing had asked for both at once.

## 1.4.6 — 2026-08-05

Everything here came from deploying to a real server for the first time. Four traps, each
of which cost real time, and each invisible on a development Mac.

### Fixed

- **`--config` no longer ignores your other flags — it refuses to start.** The file and the
  command line were an either/or, not a merge, so `--config x.toml --workers 4
  --worker-script …` silently ran with the file's defaults: 20 per-request workers, with
  nothing in the log to explain it. Askr now names the flags that would have been dropped
  and points at the file. Correct usage is unaffected.

### Documented

- **Bind-mounting an app on Linux** ([DOCKER.md](docs/DOCKER.md)): the image runs as uid
  999, a bind mount keeps the host's ownership, and Laravel can't write `storage/`. The
  symptom is precise and misleading — **every PHP route 500s while static files serve
  fine** — because Monolog fails during bootstrap. Fix: `user: "1000:1000"`. macOS hides
  this entirely, so a working laptop compose file can fail on a server for this reason
  alone. Same for a bind-mounted database: a named volume inherits the image's ownership
  and works; a bind mount doesn't.

- **No PHP CLI in the image** ([DOCKER.md](docs/DOCKER.md)): PHP is compiled into the
  binary, so `exec askr php artisan` cannot work. Documented the sidecar-container
  recipe — including why it must pass `SESSION_DRIVER=array` (the `askr` drivers live in
  the running server's shared memory, not in a detached container).

- **Running behind nginx** ([HOSTING.md](docs/HOSTING.md)): a verified vhost, plus the two
  settings that are easy to get subtly wrong — `https = true` (or Laravel builds `http://`
  URLs and loops) and `trusted_proxies` pointing at the **Docker network gateway**, not
  `127.0.0.1`, or `X-Forwarded-For` is ignored and every visitor looks like one IP to the
  rate limiter. Also: pass everything through, don't duplicate `try_files`/`fastcgi` —
  Askr already serves static files, compresses, and refuses dotfiles.

- **`Driver [askr] not supported`** ([package README](packages/laravel/README.md)): the
  Laravel package wasn't installed. Now findable by searching the exact message.

## 1.4.5 — 2026-08-05

**Askr-46 is fixed at the root**, and two whole failure classes went with it. In a real
Laravel + Flux app, every file response after a worker's first killed that worker —
`flux.js` failed two requests in three, which also broke dark mode and all Flux
interactivity. Now 12 of 12, zero respawns, with the standard Flux/Livewire setup.

### Fixed

- **The output layer is reset between worker requests.** PHP's output layer keeps a
  per-request "sent" flag that, in a worker's one eternal PHP request, never cleared. When
  `header('Content-Length: …')` later tried to disable zlib output compression, ext-zlib
  checked that flag and warned "headers already sent" — under Laravel, the global error
  handler turns that warning into an `ErrorException` outside the kernel's try, and the
  worker died. Only file responses set Content-Length, which is why only they triggered
  it; builds without ext-zlib never saw it at all, which is why it passed on the
  development machine and failed in the container. Each iteration now gets the fresh
  output state a real request gets from `php_request_startup()`.

- **`exit()`/`die()` ends the request, not the worker.** Since PHP 8.0, exit is an
  internal "unwind exit", not a bailout: it unwinds *cleanly*, so the worker script
  "completed normally" — rc=0, no error anywhere — and every hypothesis that assumed a
  crash or a closed channel was wrong. The unwind is now cleared at the loop boundary,
  FPM-style: the request gets whatever output it produced before exiting, the worker
  keeps serving, and the log says so.

- **An uncaught exception escaping the handler fails the request (500), not the worker.**
  Thrown outside `$kernel->handle()`'s try — during request reconstruction, or by a
  global error handler converting a warning — it used to unwind the whole worker loop
  silently. Now it is named in the log, class and message, which is exactly how the zlib
  culprit above was finally identified.

### Tests

- e2e: `exit()` mid-request answers with its partial output and the worker survives five
  subsequent requests (fails against the previous binary).
- Hardened the no-traffic canary test: under machine load the admin plane binds late, an
  empty status matched "neither rolling nor idle", and the poll asserted against a
  rollout that had barely begun.

## 1.4.4 — 2026-08-04

Diagnosis and blast radius, from chasing one real bug for a day. **No behaviour change for
a healthy app** — this release is about what happens when something goes wrong, and about
Askr telling you the truth about it.

### Fixed

- **A failed `accept()` no longer kills a worker.** One `?` meant a single accept error
  ended a process that was serving other requests — silently, since nothing logged it, and
  the PHP side then reported the tear-down as "fatal/OOM?". Accept errors are now logged
  and the loop continues, with `EMFILE`/`ENFILE` called out by name ("raise the open-file
  limit") and a short backoff so it can't spin.

- **A worker that dies mid-request answers 502, not an empty 200.** The reply channel was
  simply dropped, so a request in flight could be answered with whatever was in the output
  buffer and whatever status was left over — usually **200 with an empty body**. That is
  the worst possible answer: caches store it, browsers render it, and monitoring calls it
  healthy.

- **A worker that dies mid-*stream* aborts the response body.** Once the first `flush()`
  has put status and headers on the wire a 502 is no longer possible, so the body is now
  failed rather than closed cleanly — the client sees a truncated transfer it can detect
  instead of a complete-looking empty response.

### Changed

- **The "fatal/OOM?" message is gone.** It was a guess presented as a diagnosis, and it
  cost a day of looking for a memory problem that did not exist. Askr now distinguishes
  the cases it can actually tell apart: the request channel closing without draining (the
  server side went away) versus the worker script leaving its loop (an `exit()`/`die()` in
  the app does this, and so does a PHP fatal). The interpreter also reports `rc`,
  `exit_status` and PHP's last error whenever the loop ends — previously only on a
  non-zero code, which hid the exact case being chased.

### Known issue

[Askr-46](https://github.com/kwhorne/askr/issues) is **not** fixed. In one real
application the request following a `BinaryFileResponse` ends the worker's loop, costing
roughly one request in three with a single worker. What this release adds is the ability to
see it: Askr is exonerated for the transport (619 KB through `echo`, `readfile` and static
serving is clean, and a from-source Linux build reproduces only with the app in the
picture), and the remaining contradiction — PHP reporting a normal completion while the
Rust side says it never stopped handing over work — is recorded in the issue.

## 1.4.3 — 2026-08-04

Everything here was found by putting a real application on Askr — Laravel 13 with Flux
Pro, ElyraSQL as the database, in Docker — rather than by reading the code. **If you run
Laravel in worker mode, upgrade: two of these are correctness bugs that affect every
app.**

### Fixed — `examples/laravel-worker.php`

- **Authenticated state leaked between visitors.** After one login, anonymous requests
  with no cookie at all were served as that user — measured at 6 of 6 on the worker that
  handled the login. `forgetGuards()` and `forgetDrivers()` were already there and were
  not enough: **`session.store` is a separate singleton binding** holding the loaded
  Store, and `SessionGuard` is constructed from it, so a brand-new guard built from a
  brand-new driver still resolved the previous visitor's session. Now forgotten with the
  rest, along with queued cookies (which would otherwise attach one visitor's
  `Set-Cookie` to another's response), shared view state including the `$errors` bag,
  the locale and per-request log context. Verified after the fix: 12 of 12 requests with
  the cookie stay logged in, 12 of 12 without it are strangers.

- **Every classic HTML form post lost its fields.** Askr parses multipart bodies itself
  but passes `application/x-www-form-urlencoded` through untouched, and Symfony's
  `Request::create()` fills the POST bag from its `$parameters` argument only — never
  from the body. So `_token` was missing and every submit answered **419**, which looks
  like a CSRF bug and is really an empty request. Isolated with one decisive experiment:
  the token in the body failed, the same token in the `X-CSRF-TOKEN` header succeeded.
  Only urlencoded bodies are parsed; multipart is already done, JSON is decoded by
  Laravel on demand, and anything else stays byte-for-byte so a webhook signature still
  verifies.

- **File and streamed responses were empty.** `getContent()` returns `false` for
  `BinaryFileResponse` and `StreamedResponse`, and `echo false` prints nothing, so
  `response()->file()`, `->stream()`, `->streamDownload()` and `Storage::download()`
  answered 200 with no body. In the test app that meant **Flux UI's `/flux/flux.js`
  arrived as 0 bytes, which silently killed dark mode** and every other piece of Flux
  interactivity. The body is now produced with `sendContent()` when there is no string
  to echo. 0 → 619 302 bytes.

### Fixed — `kwhorne/askr-laravel`

- **`CACHE_STORE=askr` failed with "Cache store [askr] is not defined".** Registering a
  driver with `extend()` isn't enough; the managers look the *name* up in config first.
  The provider now supplies `cache.stores.askr` and `queue.connections.askr` unless the
  application defines its own, so the environment variable is all you need. The old
  failure only appeared on the first request that touched the cache, so the app looked
  fine until it suddenly wasn't — in the test app, `/login` was a 500 while `/` was fine.

### Tests

- An e2e test pins the contract the form-post fix depends on: a urlencoded body reaches
  PHP byte-for-byte with its `CONTENT_TYPE`.
- Fixed a flaky test of our own: the `/healthz` test went straight at the admin plane,
  which binds on its own thread slightly after the request listener, so it failed on
  roughly one run in ten with "Connection refused". The harness now waits for it.

## 1.4.2 — 2026-08-04

A review pass over the whole codebase, plus the port-80 gap it kept pointing at. Nine
findings were checked against the source: **six were real and are fixed**, three were
already handled and are documented rather than "fixed".

### Added

- **Port 80 answers now, and redirects (Askr-45).** `force_https` could never redirect
  someone who typed `http://` while Askr terminated TLS itself: a TLS listener never sees
  a plain-HTTP request, and the ACME challenge server was bound only *during* an issuance
  and torn down afterwards. So the recommended setup — auto-TLS — was the one that
  couldn't have the redirect every nginx config has.

  The plain-HTTP listener now lives for the whole process and does both jobs: HTTP-01
  challenges, and a **308** to the same host, path and query for everything else. One
  listener, so nothing fights over the port, and a challenge always wins over the
  redirect — otherwise a domain could never get its first certificate. Automatic with
  `--acme`; `--http-redirect 0.0.0.0:80` (or `[server] http_redirect`) for anyone using
  their own certificate. A failed bind warns and keeps serving HTTPS.

- **`/healthz` on the admin plane** — unauthenticated, and two words long. 200 while a
  worker can serve, 503 otherwise.

### Fixed

- **The container healthcheck failed as soon as you set `ASKR_ADMIN_TOKEN`.** It polled
  `/api/status`, which returns PIDs and memory figures and is therefore gated — so
  switching the token on made Docker, Kubernetes and Swarm declare a healthy container
  unhealthy and restart it. The image now polls `/healthz`. A probe that needs a
  credential is a probe that will eventually be wrong.

- **The admin plane now denies by default.** Protection was a list of exact paths. There
  was no bypass — anything unmatched 404s before reaching data, which we verified — but it
  meant a new endpoint was unauthenticated until someone remembered to add it to the list,
  and "remember to edit this list" is not an access-control policy. Everything except the
  dashboard shell, its icon and `/healthz` is now gated.

- **ACME private keys were written with the process umask (typically 0644).** The TLS
  private key and the ACME account credentials were readable by every local user. Created
  0600 now, and an existing file is tightened on write so upgrading fixes a key that's
  already on disk.

- **The Docker build verifies the release tarball's SHA-256 before unpacking.** A
  `.sha256` is published next to every tarball, so this was a supply-chain step left on
  the table for no reason.

- **The CoW supervisor reaped one exited worker per pass.** That loop sleeps between
  iterations, so a batch of workers dying together (a reload, an OOM sweep) left the rest
  as zombies with their slots empty for one sleep each. It now reaps everything per pass,
  like the main supervisor already did.

- **FFI entry points no longer build slices from possibly-null pointers.**
  `slice::from_raw_parts` requires a non-null pointer *even for length zero*; a null there
  is undefined behaviour, not an empty slice. PHP can't produce one today
  (`Z_PARAM_STRING` never yields null), but these are `extern "C"` and the check costs a
  branch that is never taken. All 34 sites go through one helper.

### Checked and deliberately not changed

- **`std::thread::sleep` in `shmlock::acquire`** was flagged as blocking a Tokio worker.
  It's reached only after 40 000 spin iterations and 64 `yield_now()`s, and is capped at
  200 µs — the code already reasons about exactly this. The suggested fix is also
  impossible here: the lock lives in shared memory *across processes*, and an async mutex
  is per-process.

- **`libc::signal` instead of `sigaction`** was flagged for SysV handler-reset semantics.
  On glibc and macOS — the platforms Askr supports — `signal` is BSD semantics with
  `SA_RESTART`. Verified empirically: three consecutive `SIGHUP`s, master alive and
  serving 200 after each.

- **Hand-built JSON in the admin plane** was flagged as an injection risk. Every
  interpolated value is machine-generated: record ids are `{secs}-{pid}-{seq}`, the rest
  are numbers, a socket address and compile-time constants. Rather than rewrite working
  code, a test now asserts each endpoint emits valid JSON, so the day someone interpolates
  something else, it fails.

## 1.4.1 — 2026-08-04

A security-relevant patch: **PHP diagnostics went to the visitor instead of the log.**
Found while writing a docker-compose example — the published 1.4.0 image served
filesystem paths to anyone requesting the homepage of a stock Laravel app. If you run the
Docker image or the tarball, upgrade.

- **Fixed: PHP diagnostics were written into HTTP responses instead of the log.** Askr's
  built-in defaults were `display_errors=1` and `log_errors=0`, which is backwards for a
  server: the visitor saw absolute filesystem paths, and the operator got no record at
  all. Verified against the published 1.4.0 image, which served
  `Deprecated: … in /app/vendor/laravel/framework/config/database.php` to anyone
  requesting the homepage of a stock Laravel 12 app.

  A framework masks this only once its own error handler is installed, and config files
  are parsed before that — so anything tripping a deprecation during boot goes straight
  to the client. In worker mode it's worse than cosmetic: the output precedes the headers
  and truncates the page (626 bytes instead of 81 675 in testing).

  Defaults are now `display_errors=0` + `log_errors=1`, with `error_reporting=E_ALL`
  unchanged, so nothing is hidden — it's logged rather than served. With no `error_log`
  set PHP writes to stderr, which is where Askr's log already goes. Developers who want
  diagnostics in the browser can opt back in with `ASKR_PHP_INI="display_errors=1"`.

  Covered by an e2e test that was checked against the old behaviour: it fails without the
  fix.

- **`examples/docker/quickstart.yml` — `docker compose up` for an app you already have.**
  The existing compose file builds an image from a `Dockerfile`, which is right for
  production and heavy for trying something. This one bind-mounts your project and runs
  the published image, with worker mode, a response cache and a 30-second
  `stop_grace_period` so `down` drains instead of cutting requests off. Documented in
  [INSTALL.md](docs/INSTALL.md).

- **A step-by-step install guide** ([`docs/INSTALL.md`](docs/INSTALL.md)), including how
  to put a site on HTTPS three different ways — and the honest limitation that
  `force_https` cannot redirect port 80 while Askr terminates TLS itself, because nothing
  listens there ([Askr-45](https://github.com/kwhorne/askr/issues)).

- **A release checklist** ([`docs/RELEASING.md`](docs/RELEASING.md)) and
  `scripts/publish-laravel-package.sh`, after the Laravel package silently stopped
  reaching Packagist for three weeks while every workflow reported success.

- **`scripts/check-docs.py`** validates every Markdown link *and* `#anchor`; CI runs it.
  Broken anchors are invisible on GitHub — the link renders fine and lands at the top of
  the page.

## 1.4.0 — 2026-08-04

Two halves of one idea: **find out what's safe to cache, then cache it correctly
without maintaining anything.**

`askr cache-report` watches real traffic and tells you which routes would win from
caching — and, more importantly, which are genuinely byte-identical for every visitor.
The `askr.cache` middleware then caches them with tags derived from the models the page
read, so invalidation needs no bookkeeping. Neither exists elsewhere, because both need
the server and the interpreter to be the same process.

- **Automatic cache tagging for Laravel (`askr.cache` middleware).** Page caching is
  rare in Laravel not because it's hard to switch on, but because keeping the tags
  right is a job nobody wants — one forgotten dependency serves stale content, so
  teams turn it off. Now there's no tag list:

  ```php
  Route::get('/products/{product}', ProductController::class)
      ->middleware('askr.cache:300');
  ```

  The middleware records every Eloquent model the response read (the `retrieved` event)
  and tags the cached page with them, so `$product->save()` clears exactly the pages
  that showed that product, across every worker, immediately.

  Precision while it's cheap, safety when it isn't: a response that read a few models
  is tagged per instance (`products:42`); one that read many degrades to class tags
  (`products`); one that touched more classes than an entry can hold isn't cached at
  all. `create()` clears the class tag too, since a new row has no page of its own to
  invalidate but the listing that should now include it does.

  The middleware declines to cache anything it can tell isn't shared: authenticated
  requests, responses that set a cookie, and sessions holding more than their own
  bookkeeping.

  Verified against a real Laravel 12 app, not just compiled: precise invalidation
  (renaming one model cleared its page and left its neighbour's alone), class-level
  degradation with 13 models, a `create()` clearing the listing while leaving the
  per-instance page cached, and the session/cookie guards refusing to cache.

- **Fixed: a response with more tags than an entry can hold is now refused, not
  truncated.** `store()` silently kept the first 8 and dropped the rest, so
  `forget_tag` could never reach the dropped ones — stale content served until the TTL
  expired, which is the worst failure a cache has. It now declines to cache, warns
  once, and counts `askr_cache_tag_overflow_total`. This is the hazard the automatic
  tagging above deliberately degrades to avoid.

- **`askr cache-report` — the cache oracle.** Measure what caching would buy before
  caching anything:

  ```bash
  askr serve --traffic-log /tmp/traffic.jsonl    # run for an hour
  askr cache-report /tmp/traffic.jsonl
  ```

  ```
  pattern            ttl    hit PHP saved  safety
  /products/*        60s    94%    1.48 s/m  ✓ identical for every visitor
  /dashboard         60s    94%    0.11 s/m  ✗ unsafe: 15 responses differed for the same URL
  /login             60s    88%    0.06 s/m  ✗ unsafe: 8 responses set a cookie
  ```

  The reason full-page caching is rare in PHP isn't performance, it's uncertainty —
  nobody knows how much a rule would win, or whether it would serve one visitor's page
  to everyone. Askr sees every request *and* every response body, so it can answer both
  from real traffic without changing a byte of what it serves.

  The decisive check isn't the hit rate, it's this: **did the same URL ever return
  different bytes inside the TTL window?** If so the page is personalised, caching it
  would be a bug, and it's excluded from the config the report offers you to paste.
  `Set-Cookie` disqualifies too; cookies on the *request* are a warning rather than a
  refusal, since they may be analytics only.

  `--traffic-log` records one line per request that ran PHP — so it describes the work
  still being done, not what the cache already absorbed — at the cost of one `write`
  per request. URLs are grouped into patterns (`/products/1421` → `/products/*`) so the
  output is the handful of rules an operator would actually write.

  The report is explicit about its own limits: a sample shorter than a minute is flagged
  as too short to extrapolate, and "identical for every visitor" means during the
  sample, not forever.

## 1.3.0 — 2026-08-04

The foundation release. No new features — instead, the thing that was missing under
all of them: **CI now starts the server and drives it over HTTP**, dependency
advisories are scanned, and every dependency is current including four major bumps.

New guide: **[UPGRADING.md](docs/UPGRADING.md)** — how to upgrade, roll back, what to
adopt at each version, and an honest list of what can actually bite you.

- **An end-to-end test suite, and it found a bug immediately.** `cargo test` had 46
  unit tests and CI never started the server or sent a single request — so every
  feature in 1.0.1–1.2.0 was verified by hand, and nothing prevented those behaviours
  from regressing. `crates/askr/tests/e2e.rs` now starts the real binary against a
  small PHP app and asserts over real HTTP: caching (`HIT`/`MISS`/`PASS`),
  `PURGE`/`BAN`, ESI assembly, cache rules, fleet-wide rate limiting, the
  `X-Forwarded-For` bypass, source/dotfile disclosure, cache persistence across a
  restart, virtual hosts, and all three canary verdicts (`ok`, `aborted`,
  `inconclusive`). No new dependencies; processes and temp dirs are cleaned up even
  when a test fails.

  Within minutes of existing it caught a real gap: **`[cache] persist` silently did
  nothing with a single worker.** With one worker and no sidecars Askr runs without a
  supervisor, and the cache dump lived only in the supervisor's shutdown path. Fixed.

- **Tests for the modules that most needed them.** 46 → 89 tests. `config.rs` now has
  validation tests for every rule it rejects — including a regression test for the
  `[reload]` defaults, where a derived `Default` would have meant "abort the rollout on
  any canary error at all" for anyone who never writes that section. `supervisor.rs`
  gained tests for the canary gate, including the specific bug it replaced: a clean
  canary must not be aborted by the *fleet's* errors. Plus `metrics.rs` (per-worker
  counters, status classification), `compress.rs` (negotiation, what's worth
  compressing) and `tune.rs` (its two recommendation rules, extracted so they're
  testable).

- **Dependency advisories are now scanned, and updates proposed.** Nothing warned about
  new RUSTSEC advisories — the unmaintained `rustls-pemfile` dependency dropped in
  0.9.11 was found by a manual review, which is not a process. A separate `Audit`
  workflow scans `Cargo.lock` when dependencies change **and weekly on a schedule**,
  since advisories appear over time rather than when someone commits. It's deliberately
  not part of CI: an advisory in a transitive dependency shouldn't turn the
  build-and-test signal red for a commit that had nothing to do with it.
- Added `.github/dependabot.yml` for the workspace (patch/minor grouped into one PR,
  majors separate), the `scripts/h3bench` tool, and the workflow actions themselves.
- Normalised `actions/checkout` across workflows — it had drifted to three different
  versions (v4, v5 and v7), which is exactly the rot Dependabot now prevents.
  `fetch-depth: 0` is preserved where the Laravel subtree split needs full history.

- **Every dependency is current, including four majors.** `rusqlite` 0.31 → 0.40,
  `brotli` 7 → 8, `sha2` 0.10 → 0.11 (with `hmac` 0.13, which has to move in lockstep),
  `opentelemetry` 0.27 → 0.32 (sdk + otlp together), plus 13 minor/patch updates
  (tokio 1.53, hyper 1.11, rustls 0.23.43, …) and five workflow actions.

  Crypto and integrity code doesn't get to be assumed equivalent after a bump, so both
  got **external vectors** rather than round trips: a published HMAC-SHA256 vector for
  Pusher subscription signing (the existing round-trip test signs and verifies with the
  same code, so it would still pass if the computation changed), and for the release
  verifier the published hash of `hello` plus a 200 KB body that spans several read
  buffers — a hashing loop that stopped after one read would have accepted a truncated
  download. OTel export was re-verified against a live Jaeger, and the L2 SQLite
  backend against a real database file, because compiling isn't working.

- **CI now checks the optional features.** `sql-backend`, `observ`, `otel`, `http3` and
  all of them together are part of the published `-full` build, but CI only ever
  compiled the default build — a feature-gated regression could have shipped.

## 1.2.0 — 2026-07-26

The operations release. 1.1 made Askr fast to *serve*; this one makes it safer to
**run**: refuse abusive traffic before PHP wakes up, stop a bad deploy at one worker
and drain it, keep the cache across a restart, and get a starting config measured
from your own app rather than guessed.

Every addition is a new default-off config key or a new subcommand, so 1.2.0 is a
drop-in for 1.1.x per [STABILITY.md](docs/STABILITY.md).

- **`askr tune`** (Askr-43) — measure the app, then print an `askr.toml` you can paste,
  with one line of reasoning per number:

  ```
  PHP boot              182.4 ms
  Request (mean)        24.9 ms wall, 0.1 ms CPU

    [server]
    workers = 64          # only 1% CPU-bound (waits on I/O) ⇒ more workers than cores
    max_rss_mb = 220      # 2× observed peak; memory grew 0.31 MB/request
  ```

  It runs the front controller in-process and measures boot time, **wall vs CPU time**
  per request — that ratio is what decides whether more workers than cores will help —
  plus memory growth and response size.

  No HTTP load generator, on purpose: Askr's own benchmarks show PHP is ~99.5 % of
  request time, so the interpreter is what's worth measuring. The output ends by
  stating what it *didn't* cover (one route, no cookies, no concurrency), because a
  confidently wrong `max_rss_mb` buys you a recycling storm.

- **The response cache can survive a restart** (Askr-42) — `[cache] persist` writes the
  shared region to disk on graceful shutdown and loads it at boot, so a restart doesn't
  pay a cold-cache stampede:

  ```toml
  [cache]
  persist = "/var/lib/askr/rcache.bin"
  persist_key = "git-sha"   # optional; a deploy then invalidates by construction
  ```

  The first request after a restart is a `HIT` with a byte-identical body, and tag
  invalidation still works on restored entries — the tag generations are saved with
  them, which they must be, or every restored entry would look tag-invalidated.

  This replaces the "predictive cache warming" idea it was filed against: warming
  would need per-URL frequency data, synthetic requests for every key variant, and it
  risks a warm-up storm competing with real traffic right after a deploy — when the
  system is most fragile. Keeping the bytes is simpler and exact.

  Refused unless the build, entry layout and cache size match; refused when the
  application changed (stamped with the front controller's size and mtime, plus
  `persist_key` when set); expired entries dropped on load; slot locks zeroed so a boot
  can't inherit a held lock; only graceful shutdowns write a dump.

- Fixed a shutdown hang introduced with the canary quarantine work: the "refill empty
  worker slots" pass respawned draining workers during shutdown, so the master could
  never exit. It now skips while shutting down.

- **Canary rollouts are now judged against the fleet, and a failed canary is drained**
  (Askr-40). Canary reload already aborted a bad rollout, but the decision compared an
  **absolute, fleet-wide** 5xx count (>3 in 5s) — which charges the canary for errors
  the *old* workers produced, so on any site with a normal error baseline every reload
  aborted, while a canary that served no traffic at all passed.

  Askr now keeps **per-worker counters in shared memory** and compares the canary
  against the rest of the fleet over the same window:

  ```toml
  [reload]
  canary = true
  canary_window = 5
  canary_min_requests = 20       # below this: "inconclusive", roll on with a warning
  canary_max_error_rate = 2.0    # percentage points above the fleet
  canary_max_latency_factor = 3.0
  ```

  ```
  ERROR canary UNHEALTHY — aborting reload
        reason=error rate 63.35% vs fleet 0.00% (allowed +2.00 points)
  ```

- **A failed canary is drained and its slot quarantined**, instead of being left to
  serve a broken deploy from 1/N of the fleet. Respawning it would only boot the same
  bad build, so the slot stays empty until the next reload clears the quarantine and
  refills it. Never below one worker — an empty fleet is worse than a bad one.
- Rollout outcome is exposed in `/api/status` as `rollout`
  (`rolling`/`ok`/`aborted`/`inconclusive`).
- Documented honestly: in **worker mode** the surviving workers hold the previous app
  in memory, so an abort really does keep old code serving; in **per-request mode**
  every worker reads current files from disk, so the gate detects and drains but can't
  roll back code that's no longer on disk.

- **Rate limiting in the Rust layer** (Askr-41) — `[[ratelimit]]` rules refuse abusive
  traffic before PHP is woken, in the same layer that serves cache hits:

  ```toml
  [server]
  trusted_proxies = ["10.0.0.0/8"]

  [[ratelimit]]
  path = "/login"
  limit = 5
  window = 300

  [[ratelimit]]
  path = "/api/*"
  limit = 60
  window = 60
  by = "header:X-Api-Key"
  burst = 20
  ```

  Token buckets live in shared memory mapped before the fork, so **a limit spans the
  whole worker fleet** rather than each process keeping its own count — the thing
  FPM + nginx can't do without Redis. Refused requests get `429` with `Retry-After`,
  `X-RateLimit-Limit` and `X-RateLimit-Remaining`; `askr_ratelimit_blocked_total` is
  exported to Prometheus.

  Count by client IP, a header, or a cookie. First match wins. Reserved `/askr/*`
  endpoints are exempt so a limit can't silently kill SSE or the Pusher WebSocket.
  Under table pressure the limiter **fails open** — wrongly refusing legitimate
  traffic is the worse failure for a web server.

- New `[server] trusted_proxies` (IPs or CIDRs). `X-Forwarded-For` is believed **only**
  when the peer is a trusted proxy, and then the rightmost non-proxy hop wins —
  otherwise anyone could rotate a fake client address and walk past every limit. With
  limits configured and no trusted proxies set, Askr warns at startup (behind a load
  balancer every client would otherwise share one bucket).

## 1.1.0 — 2026-07-25

The Varnish-grade cache release. This one adds **ESI**, **`PURGE`/`BAN`** and
**per-path cache rules**, completing the set that 1.0.1 started with `stale-if-error`
and cache-key normalisation. Askr now does what people reach for Varnish for
**in-process**, with no extra hop — plus tag invalidation Varnish doesn't have.

Every addition is a new default-off `[cache]` key or an opt-in response header, so this
is a drop-in for 1.0.x per [STABILITY.md](docs/STABILITY.md).

- **Declarative cache rules** (Askr-17) — `[[cache.rule]]` sets response-cache policy
  per path from `askr.toml`, for apps you can't edit:

  ```toml
  [[cache.rule]]
  path = "/admin/*"
  action = "pass"          # never cache, whatever the app says

  [[cache.rule]]
  path = "/static/*"
  ttl = 86400
  force = true             # cache even for visitors carrying cookies

  [[cache.rule]]
  path = "/*"
  ttl = 300
  swr = 30
  stale_if_error = 3600
  ```

  First match wins. A rule's `ttl` overrides the app's `Askr-Cache` header but keeps its
  tags, so a rule-cached page is still invalidated by `askr_cache_forget_tag()`.
  Rule-bypassed responses carry `X-Askr-Cache: PASS`, so you can tell "not cacheable"
  from "a rule said no" with curl.

  Patterns are globs, not regexes, because rules run on the request hot path; a
  regex-shaped pattern is rejected at config load, as are unknown actions and a rule
  with neither `pass` nor `ttl` — `askr config-check` reports them.

  **Not implemented, deliberately:** the issue's later phases (embedded Rhai scripting,
  Wasm plugins). They'd put arbitrary code on the cache decision path, add a sandbox to
  secure, and freeze a scripting API under the 1.0 stability contract — to do what these
  rules already do declaratively. Most of VCL's other uses are already config in Askr:
  redirects, `force_https`, cache-key normalisation, PURGE/BAN and ESI.

- **ESI — Edge-Side Includes** (Askr-16). A page can now be cached *with holes* and
  assembled per request, so the one dynamic widget on an otherwise static page stops
  making the whole page uncacheable:

  ```php
  header('Askr-Cache: 3600');
  header('Askr-ESI: on');
  echo '<esi:include src="/_esi/cart"/>';
  ```

  The shell is stored **with its tags intact** and expanded on the way out, so it can
  sit in cache for an hour while `/_esi/cart` — an ordinary request through the front
  controller, with its own `Askr-Cache` header — is rendered per request. Every hole
  gets its own TTL, tags and `PURGE`. `<esi:remove>` fallback blocks are stripped.
  Fragments nest up to 3 passes; up to 32 per request.

  - Opt-in per response: a body without `Askr-ESI: on` is never scanned, so non-ESI
    traffic pays one substring search.
  - A failing fragment (non-200, error, stream attempt) logs a warning and leaves the
    hole empty — it never takes the page down.
  - `src` must be a same-origin absolute path; absolute URLs, protocol-relative
    `//host`, schemes and `..` are refused, so an ESI tag can't become an outbound
    fetch (SSRF).
  - ESI shells are stored uncompressed and the assembled page is compressed on the
    way out. Known limit: a shell is stored once per encoding class clients negotiate.

- **HTTP `PURGE` and `BAN` cache invalidation** (Askr-19) — invalidate by URL, not just
  by tag:

  ```bash
  curl -X PURGE https://example.com/posts/123
  curl -X BAN -H 'X-Ban-Url: /category/tech/*' https://example.com/
  ```

  `PURGE` drops every cached variant of one URL (all encodings and device classes,
  `GET` and `HEAD`); matching stops at a component boundary so `/posts/1` never purges
  `/posts/12`. `BAN` takes a **glob** (`*`, `?`) in `X-Ban-Url`; a regex-looking pattern
  is rejected with a 400 instead of silently matching nothing. Both answer with a count
  (`{"purged":3}`) and are scoped to the requesting `Host`, so one virtual host can't
  wipe another's cache.

  Authenticated with `ASKR_ADMIN_TOKEN` (`Authorization: Bearer …`); with no token set,
  accepted from **loopback only** — an open purge endpoint is a cache-wiping DoS.

  Implemented as an eager scan at invalidation time rather than a shared-memory rule
  list consulted on every lookup, so the request hot path is unchanged. Cache entries
  now retain their key (512 bytes on a ~140 KB entry) to make URL matching possible.
- The response-cache key now uses the **normalised** host (lowercased, port-stripped) —
  the same value used for virtual-host routing. `example.com` and `example.com:443` now
  share one entry instead of two, and `PURGE`/`BAN` match the host a request was routed
  with.

### Security

- **Editor and deploy leftovers are no longer served** (Askr-35). A follow-up probe
  sweep after Askr-34 found the same disclosure class one step over: `index.php.bak`,
  `config.php~`, `db.php.save` and `index.php.orig` were served verbatim — still PHP
  source, just with a suffix. Any filename containing `.php.`, ending in `~`, or
  ending in `.bak`/`.orig`/`.save`/`.swp`/`.swo`/`.old`/`.rej`/`.tmp` now falls through
  to the front controller. Assets that merely *look* similar (`photo.old.png`,
  `vendor.bak.js`) are unaffected.

  nginx and Apache serve these by default too — the difference is that Askr ships with
  no config file to add rules to, so it refuses them itself.

- Documented (and deliberately **not** changed): Askr follows symlinks out of the
  document root, because `php artisan storage:link` creates exactly that and blocking
  it would break uploads for most Laravel apps. This matches nginx
  (`disable_symlinks off`) and Apache (`FollowSymLinks`) defaults. Keep the docroot
  free of symlinks you don't intend to publish.
- Verified as *not* vulnerable in the same sweep: percent-encoded traversal
  (`%2e%2e`, `..%2f`), embedded-NUL paths, trailing-dot paths, and directory listing
  (there is none — directories fall through to the app).

## 1.0.1 — 2026-07-25

### Security

- **Static file serving no longer discloses sources or dotfiles** (Askr-34). A request
  whose path resolved to an existing file was served as static bytes with no
  extension filtering, so:
  - `GET /index.php` returned the **PHP source** instead of running it, and any other
    `.php` under the document root (installers, legacy scripts, files holding
    credentials) could be read the same way;
  - dotfiles were served verbatim — with a document root pointed at an app root
    (a common misconfiguration) `GET /.env` returned `APP_KEY` and database
    credentials, and `/.git/config` was readable.

  Paths ending in `.php`/`.php3-8`/`.phps`/`.pht`/`.phtml`/`.phar`, and any path with
  a dot-component, now fall through to the front controller so the application answers
  (normally a 404). `.well-known/` remains servable for ACME HTTP-01 and
  `security.txt`. Askr still only ever executes the configured front controller — never
  an arbitrary `.php` found on disk — so this path cannot execute an uploaded file
  either. **Upgrade recommended for any deployment whose document root contains PHP
  files beyond the front controller, or is not a dedicated `public/` directory.**

- **stale-if-error & saint mode** (Askr-18) — an app can now survive its own backend
  failing. `header('Askr-Cache: 300, stale-if-error=86400')` (alias `sie=`) keeps the
  entry as a **failure fallback**: never served proactively, but when PHP answers
  `5xx`, times out, or the worker dies, Askr serves the held response with
  `X-Askr-Cache: STALE-ERROR` instead of the error page. The real failure is still
  logged, counted in metrics, and recorded for `askr replay`, so the outage stays
  visible while visitors keep browsing.
- New `[cache] saint_seconds` (default `0` = off): after a `5xx`, the worker treats
  PHP as unhealthy for that long and serves `stale-if-error` entries **without running
  PHP**, giving a struggling database room to recover. Requests with no fallback still
  go through, so recovery is detected on its own.
- The `stale-if-error` window is measured from the fresh deadline and is independent
  of `swr`, so `300, swr=60, stale-if-error=86400` behaves as all three.
- Fixed alongside: the request-coalescing follower path could serve an entry that was
  only alive inside its `stale-if-error` window; followers now ignore those, as they
  must (they're fallbacks, not hits).

- **Smart cache-key normalisation** (Askr-20) — tracking parameters and analytics
  cookies no longer shred the response-cache hit rate. New `[cache]` keys:
  - `strip_query_params` — parameters ignored when building the cache key (trailing
    `*` globs, e.g. `utm_*`), so `/p?id=7`, `/p?id=7&utm_source=fb` and
    `/p?utm_source=x&id=7&gclid=z` share one entry. PHP still receives the full,
    untouched query string.
  - `ignore_cookies` — cookies that don't count as identity (`_ga`, `_gid`, `_fbp`).
    Previously *any* cookie made a request uncacheable, so one analytics cookie made
    a whole audience bypass the cache; now such a visitor is served the same entry as
    a cookie-less one. Unlisted cookies (sessions, auth) still defeat caching.
  - `vary_user_agent` — split the key on a coarse mobile/desktop class and emit
    `Vary: User-Agent`; stale-while-revalidate refreshes forward the original
    `User-Agent` so a refresh renders as the class it's stored under.
- Query parameters are also **order-normalised** (`?a=1&b=2` = `?b=2&a=1`), skipped
  when a name repeats (`a[]=1&a[]=2`) since PHP builds an order-sensitive array there.
- Cached entries now emit a single merged `Vary` header instead of one per concern.

## 1.0.0 — 2026-07-23

**Askr is 1.0.** The stable surface is now **frozen under SemVer** — see
[STABILITY.md](docs/STABILITY.md). There are no functional changes since 0.9.12; this
release promotes the battle-tested 0.9.x line into a stability commitment.

What 1.0 locks down (stable within `1.x`; a breaking change needs `2.0` + a
deprecation cycle):

- **CLI** — subcommands (`serve`/`test`/`replay`/`doctor`/`config-check`/`upgrade`/
  `status`) and their documented flags.
- **Config** — every documented `askr.toml` key.
- **Environment** — the `ASKR_*` variables.
- **PHP bridge** — the `askr_*` functions injected into PHP.
- **HTTP surface** — `/askr/*`, the `Askr-Cache`/`X-Askr-Cache` headers, `Alt-Svc`.
- **Build features** — `sql-backend`, `observ`, `otel`, `http3` (all in the `-full`
  build); the default build stays feature-free.

The 1.0 line is the whole PHP application server in one binary: embedded **PHP 8.5**
(non-ZTS, OPcache + JIT) running real **Laravel 13** with no FPM/FastCGI; process-per-
core prefork or CoW workers; Redis-free sessions/cache/locks/queue/broadcasting;
optional durable **L2** backends and an **observability** sink over the MySQL wire;
**HTTP/1.1 + HTTP/2 + HTTP/3**; auto-TLS (ACME) + cert hot-reload; multi-domain
hosting (virtual hosts + redirects); Linux sandboxing; and self-update.

Validated by a stress campaign in a local OrbStack: tens of millions of requests at
**100 % success** with bounded memory, the observability sink **DB-verified against
ElyraSQL**, and multi-domain routing correct under concurrency.

## 0.9.12 — 2026-07-23

Multi-domain hosting: one Askr instance now serves many domains/apps and redirects
between hostnames. Plus streaming PHP output, a crash-loop guard, and TLS cert
hot-reload. See the new [Hosting guide](docs/HOSTING.md).

- **Feature (routing): virtual hosts — multiple domains/apps in one instance (Askr-32).**
  `[[site]]` entries in `askr.toml` (`hosts`, `root`, `front`) route by the `Host`
  header to per-site document roots, with a `*.suffix` glob and a fallback to
  `[server] root`. Static files are served per-site in any mode; **full dynamic
  dispatch (a different app per host) works in per-request mode** — worker mode still
  serves one booted app (statics per-site), so multi-app worker pools remain future
  work. Verified e2e: three hosts → three apps + per-site static files, glob + default
  fallback. No more one-Askr-per-app on a shared server.
- **Feature (routing): redirect engine — `www`→apex and http→https (Askr-33).**
  Declarative host redirects in `askr.toml` (`[[redirect]] from = "www.x.no", to =
  "https://x.no"`, default 308, path + query preserved, `*.suffix` glob) plus a
  `--force-https` / `[server] force_https` flag that 308s plain HTTP to HTTPS (using
  the connection's TLS state, `https`, or `X-Forwarded-Proto`). Runs before any
  dispatch. Verified e2e: www→apex (308), glob (301), no-rule host passes through,
  force_https redirects while an `X-Forwarded-Proto: https` request is left alone.
- **Feature (worker mode): PHP output streams as it's flushed (Askr-26).** When a
  worker script calls `flush()` mid-request (a Symfony `StreamedResponse`, an SSE
  endpoint, a large `readfile()` export), Askr now streams each chunk to the client
  as PHP produces it — chunked transfer, no `Content-Length` — instead of buffering
  the whole body first. Wired through a new SAPI flush hook in the C shim that sends
  headers once then body chunks, a `Reply::Stream` variant carrying an `mpsc` body
  channel with back-pressure (a slow client pauses PHP, bounded memory), and a
  streaming response in the server. The buffered path is unchanged (a normal response
  never flushes mid-handler), so cache/compression still apply to it. Verified e2e:
  5 chunks arrive ~200 ms apart (chunked, no buffering); buffered responses (status,
  headers, POST) byte-identical.
- **Robustness (supervisor): fail fast on a boot crash-loop (Askr-31).** A worker that
  dies within 3 s of spawn *with a non-zero exit* is a boot failure (an invalid TLS
  cert, bad config, or an app that fatals on the first request) rather than normal
  recycling — which drains and exits 0. If enough pile up (≥ `max(workers×3, 10)`)
  within 30 s the master logs a clear error and exits instead of respawning forever
  and burning a core. Verified: an invalid (X.509 v1) cert now makes the master give
  up in ~1 s, while 200 rapid `--max-requests` recycles don't trip it.
- **Feature (TLS): automatic reload on certificate change (Askr-27).** A watcher polls
  the `--tls-cert`/`--tls-key` mtime and triggers a graceful rolling reload when they
  change on disk (e.g. an external certbot renewal). Respawned workers re-read the
  cert, so an external renewal now hot-reloads with no restart or manual `SIGHUP`.
  On by default when a cert file is configured (not self-signed / not `--acme`).
  Verified e2e: renewing the cert triggers exactly one rolling reload (no restart),
  the server keeps serving.
- **Perf (cache): wider probe window before eviction (Askr-25).** The KV cache probe
  window went 16→32 and the response cache 8→16, so a `set` evicts only at a higher
  fill factor (fewer premature evictions when the table is ~70 % full) — at the cost
  of scanning more slots on a collision. Size tables generously and it rarely bites.

## 0.9.11 — 2026-07-23

Third source-code-review pass (admin/security + hygiene) plus follow-through on the
tracked hygiene items.

- **Deps: drop the unmaintained `rustls-pemfile`.** Cert/key PEM parsing in `tls.rs`
  and `http3.rs` now uses `rustls::pki_types::pem` (`PemObject`) directly, and
  `rustls-pemfile` (RUSTSEC-2025-0134) is removed from the tree. Verified: TLS (h2)
  and HTTP/3 still load certs. *(Askr-28)*
- **Refactor: extract the supervisor into its own module.** `main.rs` dropped from
  ~1762 to ~1026 lines; the prefork/CoW pools, recycling, RSS-based recycling, queue
  autoscaling, canary + rolling reload, and the status/reload surface now live in
  `supervisor.rs`. No behaviour change — verified e2e (4 workers, admin status,
  rolling reload, `--max-requests` recycling). *(Askr-30)*
- **Tests: concurrent shared-memory cache stress test.** Hammers the table from 8
  threads — an atomic-increment invariant (N×M increments total exactly N×M) plus
  set/delete/get churn on a colliding keyspace — to catch torn-write/probe-chain/
  tombstone races. Cache tests are now serialized so they don't race the shared
  global regions. *(Askr-29)*
- **Security (admin plane): optional bearer token + non-loopback warning.** The admin
  plane exposed `POST /api/reload` (a reload trigger) and `/api/status`,`/api/metrics`,
  `/metrics`,`/api/errors` (PIDs, RSS, error records) with no auth beyond "bind to
  localhost". Now: set **`ASKR_ADMIN_TOKEN`** to require `Authorization: Bearer <token>`
  on those endpoints (constant-time compared), and Askr logs a clear warning at
  startup if the admin address isn't loopback (louder still if no token is set). The
  dashboard shell (`GET /`) stays open (no data); default behaviour (no token) is
  unchanged.
- **Fix (metadata): correct the package repository URL.** `Cargo.toml` pointed at
  `github.com/wirelabs/askr`; it's `github.com/kwhorne/askr` (matching README/SECURITY
  and the actual repo published as crate metadata).
- **Fix (docs): SECURITY.md supported-versions table** now reflects the `0.9.x` line
  instead of the stale `0.1.x`.
- **Deps: move off yanked versions.** `cargo update` to `num-bigint` 0.4.8 and `spin`
  0.9.9 (both transitive, previously yanked). `rustls-pemfile` (unmaintained,
  RUSTSEC-2025-0134) is tracked for migration to `rustls-pki-types` separately.

## 0.9.10 — 2026-07-23

Second source-code-review pass. Each finding was verified against the source first;
below are the ones that were real. (Notably *not* real: the "`DefaultHasher` uses
random per-process keys so shared-memory hashing differs across workers" claim —
`DefaultHasher::new()` uses fixed keys and is deterministic across processes, proven
empirically and by the fact that sessions/cache already persist across workers.)

- **Perf (cache hot path): relax shared-memory pointer ordering.** The KV cache and
  response cache published their base pointers with `SeqCst` and re-read them with
  `SeqCst` on every op. The region is mapped once in the master before forking and
  is read-only after, so this is now a `Release` store paired with `Acquire` loads —
  dropping `SeqCst`'s stronger barrier from the per-op read path (a measurable win on
  weak-memory/ARM; a no-op on x86). Verified cross-worker cache sharing intact
  (50/50 hits across 4 workers).
- **Security (uploads): the temp dir is now `0700` on Unix.** `/tmp/askr-uploads`
  was created with default (world-traversable) permissions, so uploaded temp files
  could be read by other local users on a shared host. It's now created `0700` (and
  an already-existing dir is tightened), blocking entry by other users.
- **Robustness (worker): brief flush window before `exit(75)`.** When the PHP
  interpreter dies unexpectedly (fatal/OOM) the worker exits for a supervisor
  respawn; it now waits ~150 ms first so the Tokio runtime can flush the in-flight
  request's 502 (and any concurrently draining response) to the client, instead of
  the abrupt exit turning it into a connection reset. Bounded so respawn isn't
  materially delayed.

## 0.9.9 — 2026-07-23

Follow-through on the deferred performance/robustness items from the 0.9.8
source-code review. All behaviour-preserving; default build/CI unchanged.

- **Perf (worker hot path): reuse NUL-terminated buffers instead of allocating a
  `CString` per request field.** `php.rs` now loads each request's method / URI /
  query / headers / POST fields / file metadata through a per-thread arena of
  reusable buffers, removing hundreds of short-lived heap allocations per second at
  high RPS. The shim copies every pointer before returning, so a buffer only lives
  until its own next reuse. Verified end-to-end (headers, query, POST arrays, 200/200
  requests correct). *(Askr-21)*
- **Perf (shared-memory lock): gentler backoff under contention.** `shmlock` now
  yields far longer (a holder copying a ≤64 KB value resumes in microseconds) before
  falling back to a small, bounded sleep (10 µs → 200 µs cap) — reducing how long a
  Tokio worker thread can be parked waiting on a preempted holder, without touching
  the uncontended fast path. *(Askr-22)*
- **Perf (SSE/broadcast): shard subscribers by channel.** The SSE hub is now a
  `HashMap<channel, Vec<Sender>>`, so delivering an event only touches that channel's
  subscribers (O(subs-on-channel), not O(all-subs)) — relevant when a box fans out to
  thousands of SSE clients across many channels. *(Askr-23)*
- **Observability: count oversized cache drops.** A cache write whose value exceeds
  the largest slot (64 KB) now increments `askr_cache_oversize_total` (on `/metrics`)
  and logs at debug, instead of failing silently — so dropped large sessions/fragments
  are visible. *(Askr-24)*
- **Build (Docker): resilient apt in the image build.** Both `apt-get` layers retry
  the whole update+install (with per-fetch `Acquire::Retries`) so a transient
  `archive`/`security.ubuntu.com` mirror outage on the CI runner no longer fails the
  release image build.

## 0.9.8 — 2026-07-22

Robustness pass from a source-code review — correctness fixes in the shared-memory
caches and the worker request path, all behaviour-preserving for correct inputs.

- **Fix (cache correctness): tombstone deletion in the shared-memory caches.** The
  KV cache (`cache.rs`) and response cache (`rcache.rs`) use linear probing but
  `delete`/expiry/tag-invalidation wrote an *empty* slot (`0`) mid-chain, which
  ended the probe early and hid a colliding key stored later in the chain — a false
  cache miss, and for the atomic-lock path (`add`, used by `Cache::lock`) a possible
  false re-acquire of a still-held lock. Deletes now write a **tombstone** that
  lookups skip but don't stop at; `set`/`add`/`increment` scan the whole chain for
  an existing key before reusing a tombstone (so no duplicates), and re-validate
  under the slot lock. New regression test (`delete_preserves_colliding_chain`).
- **Fix (cache): eviction no longer clobbers a racing write.** `set`'s victim
  selection now prefers a free/tombstoned slot over evicting a live entry, and only
  counts a real eviction; the response cache does the same.
- **Fix (uploads): an empty file input is `UPLOAD_ERR_NO_FILE`, not a 0-byte file.**
  A form submitted with a file field left blank (`filename=""`) previously produced
  a 0-byte temp file with `error=OK`, so `$request->hasFile()` returned true. It now
  matches PHP: the entry has `error=4` and no temp file.
- **Fix (worker): request buffers no longer grow without bound / drop silently.** The
  C shim now warns once per request when a request exceeds the header/POST/file caps
  (raised to 256/1024/128) instead of silently dropping, and reclaims a response
  buffer that a single large response grew past 256 KB (a 50 MB export no longer
  costs a worker 50 MB of C heap for the rest of its life).
- **Feature: configurable slowloris timeouts.** `--tls-handshake-timeout` (default
  10 s) and `--header-read-timeout` (default 15 s), also `[server]` keys — previously
  hard-coded, now tunable for slow/mobile clients.

## 0.9.7 — 2026-07-18

HTTP/3 is now *real and measured*: responses stream over QUIC, the traces show it
per request, and there's an honest under-loss benchmark. Plus the 1.0 compatibility
contract — this is the last feature release before a 1.0 that is a pure freeze.

- **Feature (transport): HTTP/3 responses now stream (`--features http3`).** The
  h3 path streams the response body frame by frame instead of buffering it — so a
  never-ending `/askr/events` SSE stream, a large static file (served in 64 KB
  chunks), or a chunked JSON API all work over HTTP/3 without buffering the whole
  body in memory (and without hanging forever waiting for an SSE stream to "end").
  Verified end-to-end: a 5 MB file arrives byte-exact over multiple QUIC frames,
  and a live SSE broadcast is delivered incrementally over h3.
- **Observability: finer trace spans + fast-path traces (`--features otel`).** Each
  request trace now also carries a **`request.read`** child span (body read/parse)
  alongside `php.execute` and `response.build`, and the root span records
  **`network.protocol.version`** (so you can see h1 vs h2 vs **h3** per request) and
  `url.query`. Cached requests are now traced too: a cache **HIT**/**STALE** and a
  coalesced follower each emit a (phase-less) root span, so the fast paths are
  visible in the trace view — not just the misses that reach PHP.
- **Stability: a compatibility contract (`docs/STABILITY.md`).** Documents exactly
  which surfaces 1.0 will freeze — CLI subcommands/flags, `askr.toml` keys, `ASKR_*`
  env vars, the `askr_*` PHP bridge, the reserved HTTP surface (`/askr/*`,
  `Askr-Cache`/`X-Askr-Cache`, `Alt-Svc`), and the build features — what is
  explicitly *not* stable (internal crates, shared-memory layout, log prose), and
  the add→alias+warn→remove deprecation policy.
- **Docs (benchmarks): honest HTTP/3-vs-HTTP/2-under-loss numbers.** Added a
  `tc netem` sweep ([BENCHMARKS.md](docs/BENCHMARKS.md)) driven by a small native
  Rust load client (`scripts/h3bench`, one connection × 50 multiplexed streams, every
  response validated, `err=0`): on a low-RTT lossy link HTTP/3 is **~40–70× faster**
  than HTTP/2 (TCP's 200 ms RTO floor stalls the whole multiplexed connection while
  QUIC recovers per-stream), narrowing to ~1–2× when base RTT dominates. Reproducible
  via `scripts/h3bench/run-in-docker.sh`.
- **CLI: `--acme-directory` renamed to `--acme-directory-url`** (so it's no longer a
  one-letter typo away from `--acme-dir`, the local cert-cache directory). The old
  spelling still works as a hidden alias — the first application of the deprecation
  policy above.

## 0.9.6 — 2026-07-18

HTTP/3, differentiated OpenTelemetry traces (with a `response.build` child span),
and a metrics rollup — the last big transport gap plus a complete observability
story. All opt-in (feature-gated) and compiled into the `-full` build; the default
build, its behaviour, and CI are unchanged.

- **Feature (transport): HTTP/3 (QUIC) (`--http3`, `--features http3`).** Serve
  HTTP/3 over QUIC on the TLS port alongside HTTP/1.1+HTTP/2, sharing the same
  rustls (ring) certificate and the **same request handler** — so PHP sees
  `SERVER_PROTOCOL=HTTP/3.0` with no app change. TCP responses advertise it via
  `Alt-Svc: h3=":<port>"` so clients upgrade. Built on `quinn` + `h3`, with a
  `SO_REUSEPORT` UDP socket per prefork worker (the kernel steers each QUIC
  connection to one worker). Requires `--tls-cert`/`--tls-key`; off by default and
  behind `--features http3` (included in the `-full` build), so the default build,
  its behaviour, and CI are unchanged. The request handler was made generic over
  the body type to serve both transports. Verified end-to-end with a real HTTP/3
  client (`curl --http3`): `[HTTP-version=3]`. *(Note: SSE/streaming responses over
  h3 are buffered in this first cut; expose a UDP port for the QUIC listener.)*
- **Feature (observability): OpenTelemetry trace export (`--features otel`,
  `ASKR_OTEL_ENDPOINT`).** Askr owns the whole request boundary, so it exports a
  trace that splits the time PHP-FPM/Octane can't see: a root `http.request` span
  (with `http.request.method`, `url.path`, `http.response.status_code`,
  `askr.cache`) and a child **`php.execute`** span timed to the exact PHP window —
  making "PHP is ~99.5 % of the request" visible per request — and a child
  **`response.build`** span (compression) shows where the rest went, so the whole
  request is a small flamegraph. Exported over OTLP/gRPC on a background batch
  processor (never touches request latency); point it at Jaeger/Tempo/the OTel
  Collector. Root span also carries `http.response.body.size`. Off by default and
  behind the feature (so the default build/CI are unchanged); included in the
  `-full` image/tarball. New module `otel.rs`; verified end-to-end against Jaeger.
- **Feature (observability): metrics rollup table.** The observability sink now
  also writes a periodic rollup (one row per `ASKR_OBSERV_METRICS_MS`, default
  10 s) into a `metrics` table — per-window request/error/bytes deltas plus
  windowed p50/p95/p99 latency and inflight — so dashboards needn't scan raw
  `logs`. The shared metrics are global across a box, so **exactly one process**
  writes the rollup, elected via a shared-memory PID (re-elected if it dies) to
  avoid double-counting. Added `ASKR_OBSERV_TLS` (and `?tls=1`) for
  `caching_sha2_password` servers (MySQL 8+/MariaDB 11+); the sink targets
  ElyraSQL and other `mysql_native_password` MySQL-wire databases. Behind
  `--features observ`; default build/CI unchanged.

## 0.9.5 — 2026-07-18

Makes the optional tiers consumable without building from source.

- **Packaging: publish a `-full` build with the optional tiers compiled in.** Every
  release now also ships an `askr-<ver>-linux-<arch>-full.tar.gz` tarball and a
  `ghcr.io/kwhorne/askr:<ver>-full` / `:full` Docker image built with
  `--features "sql-backend observ"` — so the durable **L2 SQL Anywhere** backends
  (`ASKR_*_DB`) and the **observability sink** (`ASKR_OBSERV_DSN`) are usable
  without compiling from source. The default tarball/image are unchanged (features
  inert until the env vars are set). Release/Docker workflows build the variant
  alongside the default; `package-release.sh` gained a `SUFFIX` knob and the
  Dockerfile an `ASKR_VARIANT` build-arg.

## 0.9.4 — 2026-07-17

- **Feature (observability): ship per-request logs to ElyraSQL / any MySQL-wire
  database (`--features observ`, `ASKR_OBSERV_DSN`).** Askr already builds a
  structured access record per request; this streams it to a telemetry database
  over the MySQL wire protocol for SQL querying (in Conductor, a BI tool, or
  `mysql`). A single background task per worker owns the connection; the request
  path only does a non-blocking `try_send` into a bounded queue and **drops (with a
  rate-limited warning) under backpressure**, so telemetry never blocks or fails a
  request. Rows are batched into one multi-row `INSERT` (per `ASKR_OBSERV_BATCH` or
  `ASKR_OBSERV_FLUSH_MS`), the `logs` table is auto-created, and the sink
  reconnects on error and drains on shutdown. Configurable via
  `ASKR_OBSERV_{SERVICE,HOST,BATCH,FLUSH_MS,QUEUE}`. Off by default and behind
  `--features observ` (new optional `mysql_async` dependency), so the standard
  build, its behaviour, and CI are unchanged. New module `observ_sql.rs`;
  `docs/OBSERVABILITY.md`. (Metrics-rollup table + trace/span export shipped in
  0.9.6.)
- **Docs: a thorough Laravel setup guide (`docs/LARAVEL.md`)** — the recommended
  end-to-end path for `composer require kwhorne/askr-laravel`: `.env`, store/
  connection config, runner scripts, dev + production run commands, region sizing,
  queue autoscaling, scheduler, broadcasting/Echo, durable L2, a production
  checklist, verification, a Redis migration table, and troubleshooting.

## 0.9.3 — 2026-07-17

Rounds out the optional durable L2 tier: cache and pub/sub backends over SQL
Anywhere, backlog autoscaling against the L2 queue, L1→L2 write-through, and a
Laravel broadcasting driver — completing the Redis-free Laravel surface (session +
cache + queue + broadcasting). All of it is behind `--features sql-backend` and
opt-in via `ASKR_*_DB`; the default build, its behaviour, and CI are unchanged.

- **Perf (cache): write-through L1→L2 for the durable cache backend (`sql-backend`).**
  When the L1 shared-memory cache is also enabled alongside the L2 SQL Anywhere
  backend, L1 becomes a fast local read tier: reads hit L1 first and lazily
  populate it (with the remaining TTL) on a miss, so hot reads avoid a database
  round-trip entirely; writes go to L2 (the source of truth) and warm or
  invalidate L1. L1 is shared memory, so all worker processes on a box stay
  coherent; cross-box staleness is bounded by TTL. Durable + fast, no app change.
- **Perf (queue/broadcast): `prepare_cached` on the hot polling loops (`sql-backend`).**
  The queue claim (`UPDATE … RETURNING`, run on every worker poll) and the
  broadcast SSE tail query (run every ~50 ms) now cache their compiled statement
  on the per-process connection instead of recompiling each call.
- **Feature (Laravel): broadcasting driver (`BROADCAST_CONNECTION=askr`, elyra-13 surface).**
  The `packages/laravel` integration gains an `AskrBroadcaster` so Laravel Echo
  works over Askr's in-binary pub/sub with no Redis and no separate WebSocket
  server: `broadcast()` publishes a Pusher-shaped frame via `askr_broadcast()`,
  and Askr's SSE / Pusher-compatible fan-out delivers it to Echo clients. Public
  channels work fully; private/presence follow Laravel's standard channel
  authorization. Auto-registered by `AskrServiceProvider`. Transparent across the
  L1 and durable/replicated L2 backends. This completes the Laravel surface
  (session + cache + queue + broadcasting) for the Redis-free stack.
- **Feature (queue): backlog-driven worker autoscaling on the L2 queue (`sql-backend`, elyra-8).**
  The queue-worker autoscaler (`--queue` … `--queue-max`) and the
  `askr_queue_ready/total/oldest_seconds` metrics now read their backlog via a
  backend dispatch (`queue::stats()`): the L2 contract's `FILTER` backlog query
  when `ASKR_QUEUE_DB` is set, or the shared-memory ring otherwise. `balance=auto`
  worker scaling works against the durable L2 queue with no call-site changes
  — the master reads the backlog from the database and forks/drains as before.
  New `squeue_sql::stats()` (unit-tested).
- **Feature (broadcast): L2 durable pub/sub backend over SQL Anywhere (`sql-backend`, elyra-13).**
  An optional durable, replicated pub/sub backend implementing `PUBSUB_CONTRACT.md`:
  publish = `INSERT` into the append-only `askr_events` topic, subscribe = tail
  rows past a cursor. Exposes the same `publish`/`current_seq`/`read_from` surface
  and `askr_broadcast()` bridge as the L1 ring, so the SSE fan-out and the
  Pusher-compatible endpoint are unchanged — only the backend differs. A publish
  on the primary reaches Echo clients on any node via the replication log, with
  no Redis pub/sub. Selected with `ASKR_BROADCAST_DB=/path/to.db` (unset falls
  back to the L1 ring); `broadcast::{publish,current_seq,read_from,register_bridge}`
  dispatch L1/L2. New module `broadcast_sql.rs` (3 unit tests).
- **Feature (cache): L2 durable cache backend over SQL Anywhere (`sql-backend`, elyra-10).**
  An optional durable, replicated cache backend implementing the conformance-tested
  `CACHE_CONTRACT.md`: TTL get/set, atomic `increment` counters, atomic `add`
  (SETNX / `Cache::lock()` with expired-lock steal), `touch`, tag invalidation and
  flush. Exposes the exact same `get`/`set`/`add`/`delete`/`increment`/`touch`/
  `flush`/`forget_tag` bridge as the L1 shared-memory cache, so `askr_cache_*`,
  the Laravel cache store and `Cache::lock()` are unchanged — only the backend
  differs. A counter stored as INTEGER reads back as bytes, so `Cache::get` after
  `increment` behaves as PHP expects. Selected with `ASKR_CACHE_DB=/path/to.db`
  (unset falls back to L1); `cache::register_bridge` dispatches L1/L2. New module
  `cache_sql.rs` (4 unit tests). Built only with `--features sql-backend`.

## 0.9.2 — 2026-07-16

Optional durable L2 queue backend. The default build, its behaviour, and CI are
unchanged — the SQL Anywhere tier is entirely opt-in (`--features sql-backend` +
`ASKR_QUEUE_DB`).

- **Feature (queue): L2 durable queue backend over SQL Anywhere (`sql-backend`, elyra-9).**
  An optional durable, replicated queue backend that implements the conformance-tested
  substrate contract (`sql-anywhere/docs/contracts/QUEUE_CONTRACT.md`) verbatim:
  atomic `UPDATE … RETURNING` claim, at-least-once delivery with a visibility
  timeout, delayed jobs, priority, and a dead-letter table. It exposes the exact
  same `push`/`pop`/`delete`/`release`/`size` bridge as the L1 shared-memory
  queue, so the PHP `askr_queue_*` API and the Laravel driver are unchanged —
  only the backend differs. Selected at runtime with `ASKR_QUEUE_DB=/path/to.db`
  (an embedded SQL Anywhere file, an embedded replica, or a `sqld`-managed file);
  unset falls back to L1. Each process opens its own WAL connection, so the
  pre-fork worker model needs no shared state. Built only with
  `--features sql-backend`, so the standard build and CI are unaffected. New
  module `squeue_sql.rs` (4 unit tests) + `queue.rs` backend dispatch.

## 0.9.1 — 2026-07-16

Native queue-worker autoscaling — the piece that makes Askr's Redis-free stack
(data layer + runtime in one binary) do what Redis + Horizon needs a separate
daemon for.

- **Feature (queue): backlog-driven autoscaling of queue workers (`--queue-max`).**
  The supervisor reads the shared-memory job-queue backlog and scales the
  queue-worker pool between `--queue` (floor) and `--queue-max` (ceiling) — Horizon
  `balance=auto`, but native, with no extra daemon, because Askr owns both the
  queue (shared memory) *and* the worker pool. Scales up to target on a burst
  (~1 worker per 10 ready jobs), drains one worker every ~2 s as the backlog
  clears (graceful `SIGTERM`, not respawned). New `/metrics` gauges:
  `askr_queue_workers`, `askr_queue_ready`, `askr_queue_total`,
  `askr_queue_oldest_seconds` (also in the admin JSON). Verified end-to-end: a
  200-job burst scaled 1→8 workers and drained back to 1.

## 0.9.0 — 2026-07-11

Three power features (stale-while-revalidate, leak-aware recycling, traffic
shadowing) plus response-cache and cache-driver correctness fixes.

- **Feature (deploy validation): traffic shadowing (`--shadow-to <url>`).** Mirror
  a sampled fraction of *safe* (GET/HEAD, cookie-less) requests to a shadow
  upstream — typically a staging deploy of the next version — after serving the
  real response, and compare the shadow's status + body to production. Divergence
  is logged and counted on `/metrics` (`askr_shadow_total`, `askr_shadow_match_total`,
  `askr_shadow_mismatch_total`, `askr_shadow_error_total`). The client's response
  and latency are untouched (the mirror is a fire-and-forget background task), and
  only idempotent, non-user-specific requests are mirrored, so a shadow deploy
  never receives writes or one visitor's session. `--shadow-sample <pct>` controls
  the fraction. Verified end-to-end: identical versions report all-match; a
  diverging shadow version is caught (mismatch counted + logged) with the client
  unaffected.
- **Feature (worker mode): leak-aware, predictive recycling (`--max-rss <MB>`).**
  The supervisor samples each PHP worker's RSS (via `/proc`, Linux) ~once a second
  and, when one exceeds the cap, drains it gracefully and respawns a fresh one
  **before** it hits PHP's `memory_limit` and OOMs. Unlike the 0.8.3 crash-and-
  respawn safety net, this is proactive and zero-error — no `502`s at all. Also
  forces the multi-process supervisor on (like `--max-requests`). Verified in a
  Linux container: under a synthetic leak, RSS stayed bounded at ~230 MB against a
  200 MB cap over 10 000+ requests with **0 OOMs and 0 non-2xx**, where the same
  leak without it OOM-floods.
- **Feature (response cache): stale-while-revalidate + background refresh.** A
  response can now declare a stale window: `header('Askr-Cache: 60, swr=600')`.
  For the first 60s it's served fresh; for the next 600s it's served **stale
  immediately** (`X-Askr-Cache: STALE`) while Askr fires a single, coalesced
  background refresh that re-runs the front controller off the request path and
  repopulates the cache. Clients never wait for PHP on a hot page, and the
  refresh is deduplicated through the existing request-coalescing inflight table.
  Verified end-to-end: a warm page served stale in-place while a background
  render advanced the cached content exactly once.
- **Performance (response cache): cached responses are now compressed once, at
  store time, and served verbatim.** Previously the cache stored the *uncompressed*
  body and every HIT re-ran Brotli/Gzip — so a hot page was recompressed thousands
  of times per second, wasting the CPU the cache was meant to save. The cache key
  now varies on the negotiated `Content-Encoding`, so each encoding caches its
  finished bytes (with `Content-Encoding`/`Vary` set) and a HIT does zero
  compression work. Verified: MISS and HIT return byte-identical compressed
  payloads that decompress to the original.
- **Robustness (`askr` cache driver): atomic `touch()`.** Added a native
  `askr_cache_touch(string $key, int $ttl): bool` builtin that refreshes a key's
  TTL under the slot lock *without* reading and rewriting the value — closing the
  get-then-set race in the Laravel driver's `touch()` (a concurrent writer's value
  could be clobbered with a stale copy). `AskrStore::touch()` uses it, with the
  old get+set only as an out-of-Askr fallback.

## 0.8.4 — 2026-07-10

Security and robustness hardening from a full architecture review.

- **Security (httpoxy): the client `Proxy:` header is now dropped** before headers
  become `$_SERVER` vars, so it can never surface as `HTTP_PROXY`. Left unfiltered,
  many HTTP clients (Guzzle, libcurl) read that to route *outbound* requests,
  letting an attacker hijack server-side calls (CVE-2016-5385 and friends).
- **Robustness (shared-memory corruption): the per-slot spinlock no longer steals
  a lock from a live holder.** The old scheme spun a fixed count (~100–200 µs) then
  stole unconditionally — but a holder merely preempted by the scheduler (10–100 ms
  slice) or mid-copy of a 64 KB value would lose its lock, letting two processes
  into the same critical section and corrupting sessions/cache/queue. The lock now
  records the holder's **PID** and steals *only* from a holder the kernel confirms
  is dead (`kill(pid, 0)` → `ESRCH`); a live holder is waited on (`shmlock`).
- **Robustness (fork safety): the admin plane thread now starts *after* the initial
  workers are forked.** `fork()` clones only the calling thread, so a background
  thread holding an internal lock (malloc arena, tracing writer, stdout) at fork
  time would deadlock the child. Forking the initial workers while the master is
  single-threaded closes that window at startup.
- **Robustness (temp-file DoS): uploaded temp files are now unlinked by an RAII
  guard.** Previously a failed multipart parse, or a client disconnecting while PHP
  ran, leaked files under `/tmp/askr-uploads` — an attacker could fill the disk.
  The guard drops (and unlinks) whether the request completes, errors, or its
  future is cancelled mid-await.
- **Performance (cache stampede): coalesced followers no longer poll the slot
  lock.** While the leader computes, followers now do a cheap atomic `is_inflight`
  check with exponential backoff and take the slot lock (`peek`) at most once, when
  the leader finishes — instead of contending on the spinlock every 2 ms.

## 0.8.3 — 2026-07-06

- **Fix (important): worker mode no longer floods `502 php worker unavailable`
  under high concurrency.** Benchmarking revealed that a long-lived worker whose
  app leaks memory eventually hits PHP's `memory_limit`, and the resulting fatal
  ended the worker's request loop — after which the process kept answering `502`
  for every request instead of recovering. Now the interpreter thread, when it
  exits unexpectedly (a fatal/OOM rather than a graceful drain), **exits the
  process so the supervisor respawns a fresh worker** — no flood, and throughput
  stays clean. A graceful `SIGTERM`/recycle drain is distinguished from a crash
  via a shared `draining` flag, so normal shutdown is unaffected. The shim also
  logs the triggering PHP error (e.g. the exhausted `memory_limit`) so the cause
  is visible in the logs.
- Guidance: prefer **CoW mode** (`--cow`) for leaky apps — its warm re-fork makes
  respawns ~ms instead of a cold boot — and/or set `--max-requests` to recycle
  workers proactively. See docs/BENCHMARKS.md and docs/COW.md.

## 0.8.2 — 2026-07-05

- **PHP 8.5** — upgraded the embedded engine from 8.4.11 to **8.5.8** (latest),
  optimised for Laravel 13:
  - **OPcache is now built into libphp** and auto-registers — no more
    `opcache.so`/`zend_extension` line or API-version path to track. Enable with
    `opcache.enable=1`; **JIT is on by default**. `askr-run.sh`, the sample
    configs and the docs are updated accordingly.
  - **All of Laravel's required extensions** verified present: ctype, curl, dom,
    fileinfo, filter, hash, mbstring, openssl, pcre, pdo, session, tokenizer, xml
    (+ json, libxml, phar), plus the database drivers pdo_sqlite/pdo_mysql/
    pdo_pgsql and intl/gd/zip/exif/bcmath.
  - `askr doctor` now checks the full Laravel-required set, a PHP-version floor
    (>= 8.3 for Laravel 13; recommends 8.5), at least one PDO database driver,
    and OPcache availability.
  - **Fix:** PHP 8.5's `zend_signal` chained with Rust's (tokio/signal-hook)
    SIGTERM handler in an infinite loop → stack overflow on shutdown. Build with
    `--disable-zend-signals` (the host owns signals) and gate the shim's
    `zend_signal_startup()` on `ZEND_SIGNALS`. Shutdown is clean again.
  - Verified in a Linux container: fresh **Laravel 13.18.1** boots and serves
    (per-request + worker mode + OPcache/JIT), 200/200 under load, clean shutdown.

## 0.8.1 — 2026-07-05

- **`askr upgrade`** — self-update the release install in place. Resolves the
  latest GitHub release (or `--version X.Y.Z` to pin / roll back), downloads the
  matching Linux tarball, verifies its `sha256`, and swaps the whole prefix
  (binary + bundled libphp) atomically — the previous version is kept at
  `<prefix>/../askr.old`. `--check` for a dry-run; `--restart` runs `systemctl
  restart askr` after (default just prints the hint). Refuses inside containers
  (pull a new image tag) and when the prefix isn't writable (use sudo). Zero new
  dependencies (curl + sha2 + tar). Verified end-to-end on Linux. See
  docs/CLI.md#askr-upgrade.
- Docs: `--acme`-based TLS in the Ubuntu guide (was certbot), `/var/lib/askr` in
  the hardened unit's `ReadWritePaths`, and an "Upgrading Askr itself" section.

## 0.8.0 — 2026-07-05

- **Hardening / sandbox (Linux)** — `--sandbox` shrinks the blast radius of a
  PHP-level exploit:
  - **seccomp** (all threads): `execve`/`execveat`/`ptrace`/`process_vm_*` return
    `EPERM` — a compromised request can't spawn a shell.
  - **Landlock** (with `--sandbox-write <dir>`, repeatable): read everywhere, but
    write only under the listed paths — can't drop a webshell into the docroot.
  Applied before the PHP/tokio threads spawn (so it covers the thread PHP runs
  on); sidecars are left unsandboxed (jobs may shell out). `[server] sandbox` /
  `sandbox_write` in askr.toml. No effect off Linux; Landlock degrades gracefully.
  See docs/SANDBOX.md.
  - Verified in a Linux container: `shell_exec` → blocked, write to `/tmp` → ok,
    write into the docroot → denied, normal pages unchanged.

## 0.7.0 — 2026-07-05

- **Automatic TLS (ACME / Let's Encrypt)** — the last piece of "single binary, no
  proxy". `--acme --acme-domain example.com --acme-email you@example.com` obtains
  a certificate over **HTTP-01** and renews it automatically. Prefork-safe: the
  **master** answers challenges on `--acme-http` (default `0.0.0.0:80`) and
  obtains the cert *before* forking; workers only serve HTTPS from the cache, and
  a background renewal thread rolls them with zero downtime when the cert renews.
  `--acme-staging` for Let's Encrypt staging; `--acme-directory`/`--acme-ca-root`
  for a private CA / Pebble. See docs/AUTOTLS.md.
  - Uses `instant-acme`; a process-wide ring `CryptoProvider` is pinned (instant-
    acme brings aws-lc-rs alongside our ring stack).
  - Verified end to end against **Pebble**: account → order → finalize →
    certificate issued (by "Pebble Intermediate CA"), and Askr serves HTTPS with
    it; the HTTP-01 challenge server is unit-tested.

## 0.6.1 — 2026-07-05

- **Shared-memory job queue** — the last common Redis use. A fixed-slot job table
  in shared memory (`--queue-slots N` / `[queue] slots`) backs new `askr_queue_*`
  builtins: `push`(delayed), `pop`(reserve with a visibility timeout), `delete`
  (ack), `release`(retry), `size`. Delayed jobs, attempt counting, per-queue
  isolation, and reclaim of jobs whose reserving worker died. `examples/AskrQueue.php`
  is a Laravel queue driver on top; the existing `--queue`/`--queue-script`
  sidecar runs the workers. On a single box, Redis is now replaceable for cache,
  counters, locks, sessions, pub/sub **and queues**.
  - Verified: push/size, FIFO pop by availability, reserve (second pop skips the
    reserved job), release→retry with incremented attempts, delayed jobs not
    popped early, queue isolation. Unit-tested + exercised over HTTP.

## 0.6.0 — 2026-07-05

- **Redis-free sessions, locks and bigger cache values.** The shared cache now
  has two **size classes**: the small region (`--cache-slots`, 4 KB — counters,
  locks, small entries) and an optional large region (`--cache-large-slots` /
  `[cache] large_slots`, 64 KB — sessions, cached fragments, serialized
  collections). `set` routes by size and clears the key from the other region;
  `get`/`delete` check both.
  - New **`askr_cache_add`** — atomic set-if-absent, the primitive behind
    `Cache::add()` and `Cache::lock()`. `AskrCacheStore` now implements Laravel's
    `LockProvider`, so `Cache::lock()` is truly atomic across all workers in
    shared memory.
  - With the large region, Laravel **sessions** run on the cache
    (`SESSION_DRIVER=cache`, `SESSION_STORE=askr`).
  - Internals: `cache.rs` is generic over value size (const generics); eviction
    (oldest-first) + `askr_cache_evictions_total` carried over.
  - So on a single box, Redis is replaceable for cache, counters, locks, sessions
    and pub/sub — queues still use the DB driver. See docs/CACHE.md.

## 0.5.2 — 2026-07-05

- **Supervised external sidecars.** The supervisor can now run arbitrary external
  commands alongside the web/queue/scheduler slots — spawned, respawned if they
  die, and stopped gracefully with the rest (run via `sh -c` in `$ASKR_APP_BASE`).
  Enables **Inertia SSR** (`--sidecar "node bootstrap/ssr/ssr.mjs"` /
  `[[sidecar]] command = …`) and any other helper process in the same container.
  Verified: a node SSR-style server spawns, is respawned on kill, and drains on
  shutdown.

## 0.5.1 — 2026-07-05

- **Fix: empty static files.** A 0-byte static asset was served with
  `Content-Length: 1` and a truncated (empty) body, so the browser saw a broken
  response — which breaks a `<script type="module">` load. This is common with a
  Vite **CSS-only entry** (`resources/js/app.js` is empty, so its built `.js` is
  0 bytes). Empty files are now served correctly (`Content-Length: 0`). Found
  while running a real Livewire Flux app in a container.

## 0.5.0 — 2026-07-05

- **Run any Laravel app, including Filament.** The laravel-profile `libphp` now
  bundles the extensions heavier apps need: **intl** (Filament requires it),
  **gd** (+ jpeg/freetype/webp) + **exif**, **curl**, **zip**, **zlib**, and
  **pdo_mysql** (mysqlnd) / **pdo_pgsql** — on Linux, where the release tarballs
  and Docker image are built. The macOS dev build keeps the core set (its
  static-dependency build is for the test suite). `askr doctor` now reports a
  RECOMMENDED extension set (intl/curl/gd/pdo_mysql/zip).
  - Build deps added (CI + release + docs): `libicu-dev libcurl4-openssl-dev
    libpng-dev libjpeg-dev libfreetype-dev libwebp-dev libzip-dev zlib1g-dev
    libpq-dev`; matching runtime libs in the Docker image / release notes
    (`libicu74 libcurl4 libpng16-16 libjpeg-turbo8 libfreetype6 libwebp7 libzip4
    libpq5 zlib1g`).
  - `examples/docker/` bumped to the `:0.5` base and uses
    `composer install --ignore-platform-reqs` (build PHP ≠ Askr's runtime PHP).

## 0.4.2 — 2026-07-05

- **Docker support** — an official multi-arch image on GHCR
  (`ghcr.io/kwhorne/askr`, `linux/amd64` + `linux/arm64`), packaged from the
  relocatable release tarball on `ubuntu:24.04` (glibc match with CI; not Alpine
  — see docs/DOCKER.md). One container is the whole environment: web workers,
  queue, scheduler, cache and broadcasting in one process tree — replacing the
  usual app+nginx+redis+queue+cron stack. Ships a `HEALTHCHECK` (admin API),
  `STOPSIGNAL SIGTERM` (graceful drain), non-root, `EXPOSE 8000 9000`. New
  `Dockerfile`, `.dockerignore`, `docker.yml` workflow, and `docs/DOCKER.md`
  (compose, signals, read-only + tmpfs, TLS-behind-LB).
- **cgroup-aware workers** — the default worker count now reads the container's
  CPU limit (cgroup v2 `cpu.max`, v1 fallback) instead of the host core count, so
  a `cpus: 2` container forks 2 workers, not `nproc`. Falls back to host cores
  outside a limited cgroup.

## 0.4.1 — 2026-07-05

Server-environment completeness: compression, logging, observability.

- **Response compression** — compressible responses (HTML/JSON/JS/CSS/SVG/…) are
  compressed in the Rust hot path, negotiating `br` (preferred) or `gzip` from
  `Accept-Encoding`; often 5–10× fewer bytes on the wire. Applies to dynamic PHP
  responses, cached responses, and small static files (large files keep
  streaming). Pure-Rust encoders (`flate2` + `brotli`) — the self-contained build
  is unaffected. Adds `Content-Encoding` + `Vary`; compressed static ETags get a
  `-br`/`-gz` suffix and conditional GET tolerates it.
- **Structured access log** — `--access-log <path|->` / `[server] access_log`
  writes one JSON line per request (ts, ip, method, path, status, bytes, dur_ms),
  covering every response path (static, cache, SSE, Pusher, PHP). Off by default.
- **Prometheus `/metrics`** — the admin plane now exposes Prometheus text format
  (requests/errors/bytes, PHP-vs-total seconds, status classes, cache
  hits/misses/coalesced/evictions, in-flight + live-workers gauges, a request
  latency histogram) so Askr is scrapeable by standard tooling.
- **KV cache eviction** — under pressure the cache now evicts an expired entry,
  else the oldest-written one (was: overwrite the primary slot blindly), with a
  new `askr_cache_evictions_total` metric.

## 0.4.0 — 2026-07-05

- **Multipart file uploads (worker mode)** — the last big thing blocking "run any
  Laravel app". `multipart/form-data` is now **streamed**: each file part is
  written straight to a temp file (constant memory regardless of size — a 32 MB
  upload no longer costs 32 MB of RAM), and form fields are parsed to POST
  params. Askr hands PHP the `$_FILES`-shaped metadata (name, type, tmp path,
  size); `examples/laravel-worker.php` rebuilds them as Laravel `UploadedFile`s
  in test mode so `$request->file('avatar')->store(...)` works (the Octane model).
  Temp files are cleaned up after each request; the existing `--max-body-size`
  limit is enforced on the stream (413). New request-contract fields + shim
  setters (`askr_req_add_post`/`askr_req_add_file`).
  - Verified: a 2 MB upload round-trips with a matching SHA-1, POST fields arrive,
    the temp file is removed afterward, and an over-limit upload gets a 413.

## 0.3.2 — 2026-07-05

- **io_uring groundwork** (Linux is where the runtime swap lands):
  - `askr doctor` now *probes* io_uring via `io_uring_setup(2)` instead of only
    guessing from the kernel version — a recent kernel can still have it disabled
    (`kernel.io_uring_disabled`). Non-fatal: Askr falls back to the epoll/tokio path.
  - `scripts/bench.sh` — a benchmark harness (auto-detects oha/wrk/hey/ab) for
    comparing scenarios (tokio vs io_uring, and vs FrankenPHP / php-fpm).
  - `docs/IO-URING.md` — the design & de-risking plan (seam, monoio/tokio-uring
    tradeoffs, Linux+capability gating, phased rollout, benchmark methodology).

## 0.3.1 — 2026-07-05

- **Pusher private/presence auth** — `private-`/`presence-` subscriptions are now
  verified against the app secret (`--pusher-secret` / `$ASKR_PUSHER_SECRET` /
  `[pusher] secret`): a subscription must carry the same
  `HMAC-SHA256(secret, "socket_id:channel[:channel_data]")` token Laravel's
  `/broadcasting/auth` issues, or it's rejected with a `subscription_error`.
  Without a secret configured they're still accepted (dev). Closes the honest gap
  from 0.3.0; private channels are actually private now. Unit-tested end to end.

## 0.3.0 — 2026-07-05

Seven features that fall out of Askr's architecture (shared-memory substrate +
CoW + full request-lifecycle control) — several are things no other PHP server
can do.

### Edge cache
- **Response cache with instant tag invalidation** (`--response-cache <slots>`).
  PHP opts a response in with `header('Askr-Cache: 60, tags=posts,homepage')`;
  matching anonymous `GET`/`HEAD` requests are served straight from Rust,
  bypassing PHP entirely — static-file speed for cacheable pages.
  `askr_cache_forget_tag('posts')` bumps a generation counter in a shared tag
  table, invalidating every entry with that tag across **all** workers at once
  (O(1), no scan). `Set-Cookie` is stripped on store; only cookie-less GET/HEAD
  are cacheable. `X-Askr-Cache: HIT|MISS` + hit-rate on the dashboard.
- **Request coalescing (singleflight)** — when identical cacheable requests hit
  a cold cache together, one runs PHP and the rest wait for the fill. Cache
  stampedes are eliminated across worker processes.

### Real-time
- **Pusher-compatible WebSocket + trigger** (`--pusher`) — a drop-in Reverb:
  WS `/app/{key}` (connect / subscribe / ping) and the HTTP trigger
  `POST /apps/{id}/events` that Laravel's broadcaster calls. Rides the shared
  broadcast ring, so a trigger in any worker reaches subscribers in all of them.
  Laravel Echo works with no frontend config change. (Auth-signature
  verification for private/presence channels is a follow-up.)

### Lifecycle
- **`askr_defer()`** — register work that runs after the response is sent to the
  client, before the worker takes the next request (email, webhooks, logging) —
  Octane-style deferred work with no queue.
- **Elastic worker autoscaling** in CoW mode (`--workers-min`/`--workers-max`).
  The template sizes the pool on a live queue-depth signal, adding warm workers
  (~ms respawn) under load and harvesting them when idle. Process autoscaling has
  never been practical for PHP (~300ms cold boot) — CoW makes it cheap.

### Operations
- **Record & replay** (`--record-errors <dir>`) — a 5xx persists its full CGI
  envelope; `askr replay <id.json>` re-runs the exact request against a fresh
  interpreter. Recent failures are listed on the dashboard.
- **Fork-based parallel test runner** (`askr test`) — boot once, fork a warm,
  isolated process per test file (PHPUnit/Pest via `examples/askr-test.php`).

### Maintenance
- Deps: `rcgen` 0.13 → 0.14 (`CertifiedKey::key_pair` → `signing_key`),
  `toml` 0.8 → 1.1, `thiserror` 1 → 2. CI actions: `actions/checkout` 5 → 7,
  `actions/cache` 4 → 6.
- shim: `run_script` returns `EG(exit_status)` (correct exit(0)=0 handling).

## 0.2.1 — 2026-07-04

Hardening and distribution — no new user-facing features, but a tougher hot path,
deterministic CI, and downloadable releases.

### Server
- **Static files are streamed** in 64 KB chunks (a large file no longer buffers
  entirely in RAM per request), with **ETag** + **Cache-Control** (`immutable`
  for hashed `/build/` assets), **conditional GET** (`304` on `If-None-Match`),
  and single-**Range** (`206`) support.
- **Slowloris hardening** — TLS handshake timeout (10s), HTTP/1 header-read
  timeout (15s), and a per-worker connection cap that sheds load; important since
  Askr is designed to run with no proxy in front.
- `try_files` now stats with async `tokio::fs::metadata` (no blocking syscall on
  the async path); connections are served with upgrades enabled.

### Distribution
- **Self-contained release packages** — `scripts/package-release.sh` + a
  `release.yml` workflow build relocatable tarballs (binary + libphp + opcache +
  examples, rpath fixed to `$ORIGIN/lib`) for **Linux x86_64 and arm64** and
  attach them to the GitHub Release on each tag.
- **Ubuntu production setup guide** — `docs/UBUNTU.md`: recommended hardened
  install (release tarball, non-root systemd on `:443` via capabilities, Let's
  Encrypt via webroot, tuned opcache, canary deploys, recommended settings).

### CI / toolchain
- **Pinned Rust** (`rust-toolchain.toml` → 1.95.0) so a new release can't turn
  `main` red under `clippy -D warnings` without a code change; CI reads the pin.
- **Cached libphp** in CI (keyed on the build script) — skips recompiling PHP on
  a cache hit, the slowest step. Bumped `checkout@v5` / `cache@v4`.

## 0.2.0 — 2026-07-04

Seven differentiators beyond the core server (see the guides in `docs/`):

- **CoW template mode (`--cow`, experimental)** — boot the app once in a template
  process and fork the workers from it (copy-on-write). Workers inherit the warm,
  booted heap: **~ms warm respawn** (measured ~35 ms vs ~300 ms cold) and shared
  opcache/class tables. The template is single-threaded when it forks (tokio
  starts only in children), so the fork is safe. New code is picked up by
  restarting the process. `examples/laravel-worker.php` calls `askr_cow_ready()`.

- **Canary reload (`--canary`)** — a `SIGHUP` reload rolls one worker first and
  health-checks it (alive, no error spike) for a few seconds before rolling the
  rest; a broken deploy aborts the reload and takes down one worker instead of
  the whole fleet. Reuses the shared metrics for the health signal.
- **Broadcasting (SSE)** — push live updates to browsers with no external broker.
  `askr_broadcast($channel, $payload)` from PHP publishes into a shared-memory
  ring; each worker tails it and fans events out to the SSE connections it holds,
  so a publish from any process reaches subscribers on any process. Browsers
  subscribe at `GET /askr/events?channel=NAME` (true streaming body). Enable with
  `--broadcast` / `[broadcast]`. Verified cross-process incl. channel filtering.
- **Shared-memory cache exposed to PHP** — a fixed-slot hash table in an
  anonymous shared mmap (created before fork, shared by all workers) backs
  `askr_cache_get/set/delete/increment/flush`: cache, **atomic counters** (rate
  limiting) and locks in the Askr binary, no Redis for small/mid deployments.
  Per-slot spinlock (stolen if a holder dies), lazy TTL, length-clamped reads
  (memory-safe under races). Enable with `--cache-slots N` / `[cache] slots`.
  Ships a Laravel cache `Store` (`examples/AskrCacheStore.php`). Verified
  cross-process: set on one worker → get on others, 100/100 concurrent
  increments exact, `Cache::remember` computed once and shared.
- **In-process metrics + admin observability** — a shared-memory metrics region
  (mmap'd before fork, so all workers share the same atomic counters, no IPC)
  records throughput, latency (avg, slowest, histogram), status classes, and the
  **PHP-vs-I/O time split** that only an in-process server can measure. Exposed
  at `GET /api/metrics`, with per-worker RSS added to `/api/status` (the leak
  signal), and rendered live on the admin dashboard. Seeds the shared-memory
  substrate for a future cross-process cache/broadcast.
- **Whole Laravel runtime in one binary** — the master now supervises **queue
  workers** (`--queue N --queue-script`, or `[queue]`) and the **scheduler**
  (built-in cron; `--scheduler-script`, or `[scheduler]`) alongside the web
  workers: forked as sidecar processes running `queue:work` / `schedule:run`
  in-process, respawned on exit, drained on shutdown. No separate `php artisan`
  processes, systemd units, or Horizon/crontab needed for basic setups.
  `examples/askr-queue.php`, `examples/askr-scheduler.php`; `Interpreter::run_script`.
- **State-bleed detector (`--paranoid`)** — dev-only worker-mode diagnostic that
  snapshots app state (static properties, `$GLOBALS`, Laravel container
  bindings/instances) after each request's reset and reports anything that keeps
  growing, so Askr can tell you whether your app is worker-safe. Warms up a
  couple of requests to avoid flagging one-time boot drift; verified clean on a
  real Laravel app and catching a deliberate leak.
  `examples/askr-paranoid.php`, `[worker] paranoid`.

## 0.1.0 — 2026-07-03

First tagged release. A complete, deployable PHP application server: embedded
non-ZTS PHP running real Laravel 12 in worker mode (~9× the FPM model),
multi-core, TLS + HTTP/2, graceful recycling and zero-downtime reload, a typed
config and an admin dashboard. See [`docs/`](docs/README.md).

### Server (`askr`)
- **A1** — standalone `askr serve`: serves a real app over HTTP through the
  in-process interpreter (no FastCGI, no FPM).
- **A3** — multi-core scaling: the master forks one worker process per core,
  all accepting on a shared inherited listen socket (portable prefork).
- **A4a** — persistent worker loop: `askr_handle_request($handler)` lets a worker
  boot the app once and serve many requests (Octane model, in-process).
- **A4b** — real Laravel 12 in worker mode via `examples/laravel-worker.php`;
  ~9× the per-request (FPM) model on a Livewire app.
- **A5a** — graceful worker recycling (`--max-requests`) with drain + auto-respawn
  and crash resilience; staggered per worker.
- **A5b** — Octane-style per-request state reset (scoped instances, request, auth
  guards, DB transactions, `Str` caches) — no state bleed between requests.
- **A5c** — TLS via rustls (ring; no OpenSSL/C toolchain) + HTTP/2 (ALPN);
  `askr doctor` pre-flight (non-ZTS, required extensions, io_uring kernel).
- **A5d** — graceful **rolling reload** on `SIGHUP` (zero-downtime code deploys);
  `--tls-self-signed` (rcgen).
- **A2** — request hardening: `--max-body-size` (413 on oversize, incl. chunked),
  HEAD, and verified GET/POST (form + JSON) handling.
- **A6** — typed `askr.toml` config (source of truth for tooling/GUI),
  `askr config-check`, and a built-in **admin dashboard + API** in the master
  (`GET /`, `GET /api/status`, `POST /api/reload`) — the server-appropriate GUI
  for maintaining/configuring a live server.

### Embedded PHP (`askr-php`)
- **M0** — proved PHP embed SAPI runs in-process from Rust (non-ZTS), capturing
  output via a SAPI `ub_write` override.
- **M0+** — full request contract: `$_SERVER` injection, `php://input` body, and
  captured HTTP status + headers + body. Discovered the extension matrix and
  built oniguruma/OpenSSL/libxml2 (statically on macOS) so real Laravel renders.

### Build / platform
- OS-aware `scripts/build-libphp.sh`: system dev libs via pkg-config on Linux
  (`libphp.so`); from-source static deps on macOS (`libphp.dylib`).
- [`docs/UBUNTU.md`](docs/UBUNTU.md): full Ubuntu build + deploy guide (systemd).

### Not yet
- HTTP/3 (QUIC), the per-core io_uring I/O core (Linux), multipart `$_FILES`,
  and the `askr-laravel` composer package.

[Askr-47]: https://wirelabs.youtrack.cloud/issue/Askr-47
[Askr-48]: https://wirelabs.youtrack.cloud/issue/Askr-48
[Askr-49]: https://wirelabs.youtrack.cloud/issue/Askr-49
[Askr-52]: https://wirelabs.youtrack.cloud/issue/Askr-52
[Askr-51]: https://wirelabs.youtrack.cloud/issue/Askr-51
[Askr-53]: https://wirelabs.youtrack.cloud/issue/Askr-53
[Askr-52]: https://wirelabs.youtrack.cloud/issue/Askr-52
[Askr-54]: https://wirelabs.youtrack.cloud/issue/Askr-54
