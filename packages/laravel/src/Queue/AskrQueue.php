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
