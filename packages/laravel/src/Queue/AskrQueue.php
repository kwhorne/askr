<?php

declare(strict_types=1);

namespace Askr\Laravel\Queue;

use Illuminate\Contracts\Queue\Queue as QueueContract;
use Illuminate\Queue\Queue as BaseQueue;

/**
 * A Laravel queue backed by Askr's shared-memory job queue (`askr_queue_*`) —
 * Redis-free queues on a single box, in the Askr binary. Enable it with
 * `askr serve --queue-slots N` and run workers with `askr serve --queue N`.
 */
final class AskrQueue extends BaseQueue implements QueueContract
{
    public function __construct(private string $default = 'default', private int $retryAfter = 90)
    {
    }

    public function size($queue = null): int
    {
        return askr_queue_size($this->queueName($queue));
    }

    /**
     * Jobs available to run right now.
     *
     * Laravel 13 added four introspection methods to the Queue contract, and a class that
     * doesn't implement them is a **fatal error the moment it loads** — which in practice
     * meant a 502 on any page that touched the queue (sending mail, for one) rather than
     * a graceful failure. Same value as {@see size()}: Askr counts a job as available when
     * its delay has elapsed and no worker holds a reservation.
     */
    public function pendingSize($queue = null): int
    {
        return askr_queue_size($this->queueName($queue));
    }

    /**
     * Jobs waiting for their delay to elapse.
     *
     * Needs Askr 1.4.10 or newer, which exposes `askr_queue_stats()`. Against an older
     * server there is no way to know, and 0 is a deliberate understatement rather than a
     * guess — see {@see stats()}.
     */
    public function delayedSize($queue = null): int
    {
        return $this->stats($queue)['delayed'];
    }

    /**
     * Jobs currently held by a worker whose visibility window hasn't lapsed.
     * Needs Askr 1.4.10 or newer — see {@see delayedSize()}.
     */
    public function reservedSize($queue = null): int
    {
        return $this->stats($queue)['reserved'];
    }

    /**
     * When the oldest job that is available right now was pushed, as a Unix timestamp.
     *
     * `null` means "unknown", which is what the contract documents and what an older
     * server can honestly say. It does **not** mean the queue is empty.
     */
    public function creationTimeOfOldestPendingJob($queue = null): ?int
    {
        $ms = $this->stats($queue)['oldest_pending_created_ms'];

        return $ms > 0 ? intdiv($ms, 1000) : null;
    }

    /**
     * All four counters from a single pass over the shared-memory slot table.
     *
     * Reading them together matters: with separate calls a job that becomes available
     * between two of them can be counted twice or not at all, and a dashboard built on
     * numbers that don't add up is worse than no dashboard.
     *
     * Askr before 1.4.10 exposed only `askr_queue_size()`, so the three counters it
     * couldn't answer stay at their "unknown" values instead of being invented. The
     * package supports servers older than itself, so the check is a runtime one.
     *
     * @return array{pending:int, delayed:int, reserved:int, oldest_pending_created_ms:int}
     */
    protected function stats($queue = null): array
    {
        if (function_exists('askr_queue_stats')) {
            return askr_queue_stats($this->queueName($queue));
        }

        return [
            'pending' => askr_queue_size($this->queueName($queue)),
            'delayed' => 0,
            'reserved' => 0,
            'oldest_pending_created_ms' => 0,
        ];
    }

    public function push($job, $data = '', $queue = null)
    {
        return $this->pushRaw($this->createPayload($job, $this->queueName($queue), $data), $queue);
    }

    public function pushRaw($payload, $queue = null, array $options = [])
    {
        return askr_queue_push($this->queueName($queue), $payload, 0);
    }

    public function later($delay, $job, $data = '', $queue = null)
    {
        return askr_queue_push(
            $this->queueName($queue),
            $this->createPayload($job, $this->queueName($queue), $data),
            $this->secondsUntil($delay)
        );
    }

    public function pop($queue = null)
    {
        $q = $this->queueName($queue);
        $res = askr_queue_pop($q, $this->retryAfter);
        if ($res === null) {
            return null;
        }

        return new AskrJob(
            $this->container,
            $res['payload'],
            (int) $res['id'],
            (int) $res['attempts'],
            $this->connectionName,
            $q
        );
    }

    private function queueName($queue): string
    {
        return $queue ?: $this->default;
    }
}
