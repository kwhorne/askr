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
     * Askr's shared-memory queue tracks this per entry (`available_at`), but does not yet
     * expose it to PHP — `askr_queue_size()` is the only counter there is. Returning 0 is
     * a deliberate understatement rather than a guess: `queue:monitor` and friends will
     * see no delayed backlog on this driver. Tracked in the Askr issue tracker; when the
     * server grows a stats function this will use it.
     */
    public function delayedSize($queue = null): int
    {
        return 0;
    }

    /**
     * Jobs currently reserved by a worker. Not yet exposed to PHP — see
     * {@see delayedSize()}.
     */
    public function reservedSize($queue = null): int
    {
        return 0;
    }

    /**
     * When the oldest available job was created, as a Unix timestamp.
     *
     * Askr's queue entries carry an id and an availability time, not a creation time, so
     * there is nothing honest to return here. `null` is a documented value in the
     * contract and means "unknown" rather than "no jobs".
     */
    public function creationTimeOfOldestPendingJob($queue = null): ?int
    {
        return null;
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
