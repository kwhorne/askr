<?php

declare(strict_types=1);

namespace Askr\Laravel\Queue;

use Illuminate\Contracts\Queue\Queue as QueueContract;
use Illuminate\Queue\Connectors\ConnectorInterface;

final class AskrConnector implements ConnectorInterface
{
    public function connect(array $config): QueueContract
    {
        return new AskrQueue($config['queue'] ?? 'default', (int) ($config['retry_after'] ?? 90));
    }
}
