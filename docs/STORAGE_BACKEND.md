# Storage backends: L1 shared memory + L2 SQL Anywhere

Askr already replaces Redis for a **single box**: an in-process, shared-memory
cache (`crates/askr/src/cache.rs`), job queue (`crates/askr/src/squeue.rs`),
response cache (`rcache.rs`) and SSE/Pusher broadcasting — no external broker.

This document describes how those primitives gain a **durable, replicated,
multi-box** tier by layering over SQL Anywhere, so the "Redis-free" story holds
across a fleet and survives restarts. It is the runtime half of epic elyra-2; the
substrate half is the SQL Anywhere **contracts**, which are now
**conformance-tested** (executable specs, not just prose — see
`sql-anywhere/sqlanywhere/tests/contract_conformance.rs`), so these drivers build
against a proven substrate:

- Queue → `sql-anywhere/docs/contracts/QUEUE_CONTRACT.md` (elyra-5) ✓ conformance-tested
- Cache → `sql-anywhere/docs/contracts/CACHE_CONTRACT.md` (elyra-7) ✓ conformance-tested
- Pub/sub → `sql-anywhere/docs/contracts/PUBSUB_CONTRACT.md` (elyra-6) ✓ conformance-tested

## The two tiers

| Tier | Where | Property | Use |
| --- | --- | --- | --- |
| **L1** | anonymous shared mmap, this process tree | fastest, ephemeral, single box | hot cache, counters/locks, in-box queue, SSE fan-out |
| **L2** | SQL Anywhere table (embedded or `sqld`) | durable, transactional, **replicated**, multi-box | durable jobs, shared/edge cache, cross-node pub/sub |

The API PHP sees (`askr_cache_*`, `askr_queue_*`, `askr_broadcast()`) and the
Laravel drivers (elyra-11, elyra-12) are **unchanged**; only the backend differs.
L1 and L2 mirror the same semantics on purpose — that is why the contracts are
written to match `squeue.rs` / `cache.rs`.

## Queue (elyra-9) — implemented

The durable-queue driver (`crates/askr/src/squeue_sql.rs`, feature `sql-backend`)
claims against `askr_jobs` exactly as `squeue.rs::pop` reserves a slot:
`UPDATE … RETURNING` with a subselect, a visibility timeout, `attempts`
incremented at claim, ack = delete, release = re-arm with backoff. It implements
the conformance-tested `QUEUE_CONTRACT.md` SQL verbatim and exposes the *same*
`push`/`pop`/`delete`/`release`/`size` bridge as L1, so `askr_queue_*` and the
Laravel driver are unchanged — only the backend differs.

**Enable it** (build + run):

```bash
cargo build --release -p askr --features sql-backend
ASKR_QUEUE_DB=/var/lib/askr/queue.db ./askr serve ...   # unset => L1 fallback
```

`ASKR_QUEUE_DB` points at an embedded SQL Anywhere file, an embedded replica, or
a `sqld`-managed database. Each worker process opens its own WAL connection
(safe multi-process access via SQLite file locking), so the pre-fork model needs
no shared state; `queue.rs` dispatches L1 vs L2 once, at bridge registration.

Still open (follow-ups): connection pooling knobs, visibility renewal/heartbeat
for long jobs, dead-letter move wired into the worker loop
(`attempts >= max_attempts`), and the Laravel `QUEUE_CONNECTION=sqlanywhere`
surface (elyra-12).

## Autoscaling (elyra-8)

The existing backlog-driven autoscaler (`--queue` … `--queue-max`) reads the
backlog query from the contract instead of the shared-memory ring, and reports
`askr_queue_workers/ready/total/oldest_seconds` for the L2 backend.

## Cache (elyra-10) — implemented

The durable cache driver (`crates/askr/src/cache_sql.rs`, feature `sql-backend`)
implements the conformance-tested `CACHE_CONTRACT.md` verbatim and exposes the
*same* `get`/`set`/`add`/`delete`/`increment`/`touch`/`flush`/`forget_tag` bridge
as L1, so `askr_cache_*`, the Laravel cache store and `Cache::lock()` are
unchanged — only the backend differs. Locks use the contract's atomic `add`
(SETNX with expired-lock steal); counters use the atomic `increment` (a counter
stored as INTEGER still reads back as bytes, so `Cache::get` after `increment`
behaves as PHP expects); tag invalidation removes every key carrying the tag.

**Enable it** with `ASKR_CACHE_DB=/path/to.db` (unset => L1 fallback), building
with `--features sql-backend`. Each worker process opens its own WAL connection.

Still open (follow-ups): write-through L1→L2 with lazy L1 population for a hot
local tier, and instant cross-node tag invalidation signalled over the pub/sub
topic.

## Broadcasting / pub-sub (elyra-13) — implemented

The durable pub/sub driver (`crates/askr/src/broadcast_sql.rs`, feature
`sql-backend`) implements `PUBSUB_CONTRACT.md`: publish = `INSERT` into the
append-only `askr_events` topic, subscribe = tail rows past a cursor. It exposes
the same `publish`/`current_seq`/`read_from` surface and `askr_broadcast()`
bridge as the L1 ring, so the SSE fan-out task and the Pusher endpoint are
unchanged — only the backend differs. Askr tails its **local copy** of
`askr_events` and fans out to connected SSE / Pusher-compatible WebSocket clients
on that node, so a publish on the primary reaches Echo clients on any node via
the replication log — no Redis pub/sub.

**Enable it** with `ASKR_BROADCAST_DB=/path/to.db` (unset => L1 ring), building
with `--features sql-backend`.

Still open (follow-ups): replace the 50 ms tail poll with an update-hook /
replication-apply wakeup, and durable subscriber cursors (`askr_subscribers`) for
resume-after-restart.

## Status

Contracts (elyra-5/6/7) are **stable and conformance-tested** on the substrate
side (`sql-anywhere/sqlanywhere/tests/contract_conformance.rs`). The
**durable-queue driver (elyra-9) is implemented** behind the `sql-backend`
feature (`squeue_sql.rs`), built against that proven contract. The remaining
drivers (elyra-8 autoscaling on L2, elyra-10 cache, elyra-13 broadcast) and the
Laravel surface (elyra-11/12) are pending; see epic elyra-2 for the plan and order.
