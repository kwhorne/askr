<?php

/**
 * A Laravel queue driver backed by Askr's shared-memory job queue
 * (`askr_queue_*`) — Redis-free queues on a single box, in the Askr binary.
 *
 * Enable the queue: `askr serve … --queue-slots 4096` (or `[queue] slots`), and
 * run workers as a sidecar: `--queue 2 --queue-script examples/askr-queue.php`.
 *
 * Register the driver in your worker script (examples/laravel-worker.php) or a
 * service provider, before the worker serves / before queue:work runs:
 *
 *     use Illuminate\Support\Facades\Queue;
 *     require '/opt/askr/examples/AskrQueue.php';
 *     Queue::extend('askr', fn () => new AskrConnector());
 *
 * config/queue.php:
 *     'askr' => ['driver' => 'askr', 'queue' => 'default', 'retry_after' => 90],
 *
 * .env:  QUEUE_CONNECTION=askr
 *
 * Delayed jobs, retries (attempt counting) and per-queue isolation are handled
 * by Askr; a job reserved by a worker that dies becomes available again after
 * `retry_after` seconds (the visibility timeout). Payloads are capped at 32 KB.
 *
 * Status: example driver — the shared-memory queue primitive is tested in Askr;
 * wire this up per your app/Laravel version.
 */

use Illuminate\Container\Container;
use Illuminate\Contracts\Queue\Job as JobContract;
use Illuminate\Contracts\Queue\Queue as QueueContract;
use Illuminate\Queue\Connectors\ConnectorInterface;
use Illuminate\Queue\Jobs\Job as BaseJob;
use Illuminate\Queue\Queue as BaseQueue;

final class AskrConnector implements ConnectorInterface
{
    public function connect(array $config): QueueContract
    {
        return new AskrQueue($config['queue'] ?? 'default', (int) ($config['retry_after'] ?? 90));
    }
}

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

final class AskrJob extends BaseJob implements JobContract
{
    public function __construct(
        Container $container,
        private string $raw,
        private int $id,
        private int $attemptsCount,
        $connectionName,
        $queue
    ) {
        $this->container = $container;
        $this->connectionName = $connectionName;
        $this->queue = $queue;
    }

    public function getJobId(): string
    {
        return (string) $this->id;
    }

    public function getRawBody(): string
    {
        return $this->raw;
    }

    public function attempts(): int
    {
        return $this->attemptsCount;
    }

    public function delete(): void
    {
        parent::delete();
        askr_queue_delete($this->id);
    }

    public function release($delay = 0): void
    {
        parent::release($delay);
        askr_queue_release($this->id, (int) $delay);
    }
}
