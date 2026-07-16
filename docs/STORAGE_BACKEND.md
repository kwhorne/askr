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

## Queue (elyra-9)

The durable-queue driver claims against `askr_jobs` exactly as `squeue.rs::pop`
reserves a slot: `UPDATE … RETURNING` with a subselect, a visibility timeout,
`attempts` incremented at claim, ack = delete, release = re-arm with backoff,
dead-letter on `attempts >= max_attempts`. See the queue contract for the SQL.

- Connection pooling to SQL Anywhere (embedded API or server mode).
- Visibility renewal (heartbeat) for long jobs; graceful drain on `SIGTERM`.
- Choice of backend is config: keep L1 for ephemeral/low-latency; use L2 for
  durable/replicated (`QUEUE_CONNECTION=sqlanywhere`, elyra-12).

## Autoscaling (elyra-8)

The existing backlog-driven autoscaler (`--queue` … `--queue-max`) reads the
backlog query from the contract instead of the shared-memory ring, and reports
`askr_queue_workers/ready/total/oldest_seconds` for the L2 backend.

## Cache (elyra-10)

Write-through L1→L2 with lazy L1 population on read. Locks (`Cache::lock()`) and
atomic counters use the contract's `add` (SETNX) and `increment` statements. Tag
invalidation propagates across workers/boxes via the pub/sub topic when instant
cross-node invalidation is required.

## Broadcasting / pub-sub (elyra-13)

Askr tails the `askr_events` topic (local copy, woken by replication apply or the
SQLite update hook) and fans out to connected SSE / Pusher-compatible WebSocket
clients on that node — so a publish on the primary reaches Echo clients on any
node, with no Redis pub/sub.

## Status

Contracts (elyra-5/6/7) are **stable and conformance-tested** on the substrate
side (`sql-anywhere/sqlanywhere/tests/contract_conformance.rs`). Drivers
(elyra-8/9/10/13) and the Laravel surface (elyra-11/12) are pending; see epic
elyra-2 for the plan and order. Because the contract SQL is now proven, the
durable-queue driver (elyra-9) can be implemented directly against
`QUEUE_CONTRACT.md` §Operations with confidence that claim/ack/release/dead-letter
behave as specified.
