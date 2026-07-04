<?php

/**
 * Askr state-bleed detector (`--paranoid`).
 *
 * The #1 reason people distrust the worker/Octane model is fear of state
 * leaking between requests. This detector snapshots your app's mutable state
 * *after* each request's reset and reports anything that keeps growing — so
 * Askr can tell you whether your app is worker-safe.
 *
 * It is framework-agnostic; given a Laravel container it also tracks container
 * bindings/instances. **Dev only** — reflecting over app classes every request
 * is expensive (that's why it's behind `--paranoid`).
 *
 * Signal: it compares each request to the *previous* one and reports counters
 * that increased. A one-time bump when a singleton first resolves is normal and
 * self-limiting; something that grows on *every* request is a leak.
 */
final class AskrParanoid
{
    /** @var array<string,string> previous snapshot: key => fingerprint */
    private array $prev = [];
    /** @var array<string,bool> class name => is it an app (non-vendor) class */
    private array $appClasses = [];
    private int $findingsTotal = 0;

    public function __construct(
        private string $appBase,
        private ?object $app = null,
        private int $warmup = 2,
    ) {
        // Class files come back as realpaths (e.g. /tmp -> /private/tmp on
        // macOS), so canonicalise the base to compare correctly.
        $this->appBase = realpath($appBase) ?: $appBase;
    }

    /** Announce (call once, before serving). */
    public function baseline(): void
    {
        $this->emit(["[askr paranoid] armed — warming up {$this->warmup} requests before watching (dev mode)"]);
    }

    /**
     * Check after a request's reset. The first `warmup` requests establish the
     * baseline (a framework only fully boots on its first request, and services
     * resolve lazily over the first few) — findings are reported from there on.
     */
    public function check(int $request): void
    {
        $now = $this->snapshot();

        if ($request <= $this->warmup) {
            $this->prev = $now;
            if ($request === $this->warmup) {
                $watched = count(array_filter($this->appClasses));
                $this->emit(["[askr paranoid] baseline set after {$this->warmup} requests — watching $watched app classes for state bleed"]);
            }
            return;
        }

        $findings = [];

        foreach ($now as $key => $fp) {
            $before = $this->prev[$key] ?? null;
            if ($before === $fp) {
                continue;
            }
            $a = self::sizeOf($before);
            $b = self::sizeOf($fp);
            if ($a !== null && $b !== null && $b > $a) {
                $findings[] = sprintf("  ↑ %s  %s → %s  (+%d)", $key, $before, $fp, $b - $a);
            } elseif ($before === null) {
                $findings[] = sprintf("  + %s = %s", $key, $fp);
            } elseif ($a === null) {
                $findings[] = sprintf("  ~ %s  %s → %s", $key, $before, $fp);
            }
        }

        $this->prev = $now;

        if ($findings) {
            $this->findingsTotal += count($findings);
            array_unshift(
                $findings,
                "[askr paranoid] request #$request — state changed after reset (possible bleed):"
            );
            $this->emit($findings);
        }
    }

    /** @return array<string,string> key => fingerprint */
    private function snapshot(): array
    {
        $snap = [];

        // Static properties of app (non-vendor) classes — the classic bleed.
        foreach ($this->appClassList() as $class) {
            try {
                $ref = new ReflectionClass($class);
                foreach ($ref->getStaticProperties() as $name => $value) {
                    $snap["$class::\$$name"] = self::fingerprint($value);
                }
            } catch (\Throwable) {
                // ignore classes that error on reflection
            }
        }

        // Cheap global signals.
        $snap['$GLOBALS.keys'] = 'count:' . count($GLOBALS);
        $snap['declared_classes'] = 'count:' . count(get_declared_classes());
        $snap['declared_functions'] = 'count:' . count(get_defined_functions()['user']);

        // Laravel container (optional).
        if ($this->app !== null) {
            try {
                if (method_exists($this->app, 'getBindings')) {
                    $snap['container.bindings'] = 'count:' . count($this->app->getBindings());
                }
                $ro = new ReflectionObject($this->app);
                if ($ro->hasProperty('instances')) {
                    $p = $ro->getProperty('instances');
                    $p->setAccessible(true);
                    $snap['container.instances'] = 'count:' . count((array) $p->getValue($this->app));
                }
            } catch (\Throwable) {
            }
        }

        return $snap;
    }

    /** @return list<class-string> app classes, cached; rescanned for new autoloads */
    private function appClassList(): array
    {
        foreach (get_declared_classes() as $class) {
            if (array_key_exists($class, $this->appClasses)) {
                continue;
            }
            $this->appClasses[$class] = false;
            try {
                $file = (new ReflectionClass($class))->getFileName();
                $vendor = DIRECTORY_SEPARATOR . 'vendor' . DIRECTORY_SEPARATOR;
                if ($file && str_starts_with($file, $this->appBase) && !str_contains($file, $vendor)) {
                    $this->appClasses[$class] = true;
                }
            } catch (\Throwable) {
            }
        }
        return array_keys(array_filter($this->appClasses));
    }

    private static function fingerprint(mixed $v): string
    {
        return match (true) {
            is_array($v) => 'array:' . count($v),
            is_string($v) => 'string:' . strlen($v),
            is_object($v) => 'object:' . $v::class,
            is_null($v) => 'null',
            is_bool($v) => 'bool:' . ($v ? '1' : '0'),
            is_int($v), is_float($v) => 'num:' . $v,
            default => gettype($v),
        };
    }

    /** Extract the trailing integer from a fingerprint like "array:3" / "count:12". */
    private static function sizeOf(?string $fp): ?int
    {
        if ($fp !== null && preg_match('/:(-?\d+)$/', $fp, $m)) {
            return (int) $m[1];
        }
        return null;
    }

    private function emit(array $lines): void
    {
        // error_log() routes to the SAPI logger -> Askr's stderr.
        error_log(implode("\n", $lines));
    }
}
