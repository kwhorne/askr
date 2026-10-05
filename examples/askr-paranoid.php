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
 * bindings/instances.
 *
 * Two modes:
 *
 * - `--paranoid` (dev): every request is checked, and every growth is reported. A
 *   one-time bump when a singleton first resolves is normal and self-limiting;
 *   something that grows on *every* request is a leak. Reflecting over app classes
 *   on every request is expensive, which is why this is dev only.
 * - `[worker] paranoid_sample = N` (production): one request in N is checked, and a
 *   key is reported only once it has grown in three checks running — lazy services
 *   and autoloading level off as the routes are visited; a leak does not. Class and
 *   function counts are not watched here, since autoloading is exactly what grows them.
 *
 * Findings go to the log and, through askr_state_bleed(), to /api/status.
 */
final class AskrParanoid
{
    /** @var array<string,string> previous snapshot: key => fingerprint */
    private array $prev = [];
    /** @var array<string,int> key => checks in a row it has grown */
    private array $streak = [];
    private int $checks = 0;
    /** @var array<string,bool> class name => is it an app (non-vendor) class */
    private array $appClasses = [];
    private int $findingsTotal = 0;

    /**
     * @param int $sample  check one request in this many (1 = every request, dev)
     * @param int $sustain report a key once it has grown in this many checks running
     *                     (default: 1 when checking every request, 3 when sampling)
     */
    public function __construct(
        private string $appBase,
        private ?object $app = null,
        private int $warmup = 2,
        private int $sample = 1,
        private ?int $sustain = null,
    ) {
        $this->sample = max(1, $this->sample);
        $this->sustain ??= $this->sample > 1 ? 3 : 1;
        // Class files come back as realpaths (e.g. /tmp -> /private/tmp on
        // macOS), so canonicalise the base to compare correctly.
        $this->appBase = realpath($appBase) ?: $appBase;
    }

    /** Announce (call once, before serving). */
    public function baseline(): void
    {
        $this->emit([$this->sample > 1
            ? "[askr paranoid] armed — checking one request in {$this->sample}, reporting what grows {$this->sustain} checks running"
            : "[askr paranoid] armed — warming up {$this->warmup} requests before watching (dev mode)"]);
    }

    /**
     * Check after a request's reset. The first `warmup` requests establish the
     * baseline (a framework only fully boots on its first request, and services
     * resolve lazily over the first few) — findings are reported from there on.
     */
    public function check(int $request): void
    {
        if ($request % $this->sample !== 0) {
            return; // not this request's turn — the cost of sampling is this line
        }
        $check = ++$this->checks;
        $now = $this->snapshot();

        if ($check <= $this->warmup) {
            $this->prev = $now;
            if ($check === $this->warmup) {
                $watched = count(array_filter($this->appClasses));
                $this->emit(["[askr paranoid] baseline set after {$this->warmup} checks — watching $watched app classes for state bleed"]);
            }
            return;
        }

        $lines = [];
        $report = [];

        foreach ($now as $key => $fp) {
            $before = $this->prev[$key] ?? null;
            $a = self::sizeOf($before);
            $b = self::sizeOf($fp);
            $grew = $before !== $fp && ($before === null || $a === null || ($b !== null && $b > $a));
            $this->streak[$key] = $grew ? ($this->streak[$key] ?? 0) + 1 : 0;
            // Report on the check that completes a streak, and again each time it is
            // completed anew — not on every check of a long one.
            if (!$grew || $this->streak[$key] % $this->sustain !== 0) {
                continue;
            }
            if ($a !== null && $b !== null && $b > $a) {
                $lines[] = sprintf("  ↑ %s  %s → %s  (+%d)", $key, $before, $fp, $b - $a);
            } elseif ($before === null) {
                $lines[] = sprintf("  + %s = %s", $key, $fp);
            } else {
                $lines[] = sprintf("  ~ %s  %s → %s", $key, $before, $fp);
            }
            $report[] = ['key' => $key, 'from' => (string) $before, 'to' => $fp];
        }

        $this->prev = $now;

        if ($lines) {
            $this->findingsTotal += count($lines);
            array_unshift(
                $lines,
                $this->sample > 1
                    ? "[askr paranoid] request #$request — still growing after {$this->sustain} checks one in {$this->sample} apart (likely bleed):"
                    : "[askr paranoid] request #$request — state changed after reset (possible bleed):"
            );
            $this->emit($lines);
            if (function_exists('askr_state_bleed')) {
                askr_state_bleed(json_encode($report));
            }
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

        // Cheap global signals. Class and function counts grow with autoloading as new
        // routes are visited, which is all a sampled check sees of them — so only the
        // every-request mode watches those.
        $snap['$GLOBALS.keys'] = 'count:' . count($GLOBALS);
        if ($this->sample === 1) {
            $snap['declared_classes'] = 'count:' . count(get_declared_classes());
            $snap['declared_functions'] = 'count:' . count(get_defined_functions()['user']);
        }

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
