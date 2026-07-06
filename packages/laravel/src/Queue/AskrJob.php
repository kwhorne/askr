<?php

declare(strict_types=1);

namespace Askr\Laravel\Queue;

use Illuminate\Container\Container;
use Illuminate\Contracts\Queue\Job as JobContract;
use Illuminate\Queue\Jobs\Job as BaseJob;

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
