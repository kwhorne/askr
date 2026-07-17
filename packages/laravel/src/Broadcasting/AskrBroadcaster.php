<?php

declare(strict_types=1);

namespace Askr\Laravel\Broadcasting;

use Illuminate\Broadcasting\Broadcasters\Broadcaster;
use Illuminate\Support\Arr;
use Symfony\Component\HttpKernel\Exception\AccessDeniedHttpException;

/**
 * A Laravel broadcaster backed by Askr's in-binary pub/sub (`askr_broadcast()`).
 *
 * `broadcast()` publishes a Pusher-shaped frame (`{"event","data"}`) on each
 * channel; Askr's SSE / Pusher-compatible WebSocket fan-out delivers it to
 * connected Laravel Echo clients — so `BROADCAST_CONNECTION=askr` replaces
 * Redis + a WebSocket server with no external broker.
 *
 * With the durable L2 backend enabled on the server (`ASKR_BROADCAST_DB`, built
 * with `--features sql-backend`), a publish on the primary reaches Echo clients
 * on *any* node via the replication log — no code change here; only the server
 * backend differs.
 *
 * Public channels work fully. Private/presence auth follows Laravel's standard
 * channel authorization (mirroring the Redis broadcaster).
 */
final class AskrBroadcaster extends Broadcaster
{
    public function auth($request)
    {
        $channelName = $this->normalizeChannelName($request->channel_name);

        if ($this->isGuardedChannel($request->channel_name)
            && ! $this->retrieveUser($request, $channelName)) {
            throw new AccessDeniedHttpException(
                'User not authenticated for the requested channel.'
            );
        }

        return parent::verifyUserCanAccessChannel($request, $channelName);
    }

    public function validAuthenticationResponse($request, $result)
    {
        if (is_bool($result)) {
            return json_encode($result);
        }

        $channelName = $this->normalizeChannelName($request->channel_name);
        $user = $this->retrieveUser($request, $channelName);

        $broadcastIdentifier = method_exists($user, 'getAuthIdentifierForBroadcasting')
            ? $user->getAuthIdentifierForBroadcasting()
            : $user->getAuthIdentifier();

        return json_encode(['channel_data' => [
            'user_id' => $broadcastIdentifier,
            'user_info' => $result,
        ]]);
    }

    public function broadcast(array $channels, $event, array $payload = [])
    {
        if (! function_exists('askr_broadcast')) {
            // Not running under the Askr server (e.g. artisan on the CLI).
            return;
        }

        $socket = Arr::pull($payload, 'socket');
        $message = json_encode([
            'event' => $event,
            'data' => $payload,
            'socket' => $socket,
        ], JSON_UNESCAPED_UNICODE | JSON_UNESCAPED_SLASHES);

        foreach ($this->formatChannels($channels) as $channel) {
            askr_broadcast($channel, $message);
        }
    }
}
