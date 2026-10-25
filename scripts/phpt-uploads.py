#!/usr/bin/env python3
"""Run PHP's own multipart-upload tests against Askr, with PHP as the oracle.

    scripts/phpt-uploads.py --askr target/debug/askr                  # both modes
    scripts/phpt-uploads.py --askr target/debug/askr --mode worker -v

PHP ships tests for the code that turns a multipart body into `$_POST` and `$_FILES`
(`tests/basic/rfc1867_*.phpt`): a raw request, a script, and the exact output PHP gives.
Askr is in that code path twice:

- per-request mode hands the body to PHP, so PHP's own parser runs — these tests check
  that Askr delivers the request to it intact;
- worker mode parses the body itself (`upload.rs`, to stream files to disk) and hands the
  worker `$r['post']` and `$r['files']` — these tests check that what it hands over says
  what PHP would have said.

Each test runs the way php-src's run-tests.php runs it: the first `Content-Type:` line of
`--POST_RAW--` is the header, the remaining lines are the body joined with "\\n", and
`--INI--` is the php.ini. In per-request mode the output must match `--EXPECT--`, or
`--EXPECTF--` with its placeholders. In worker mode the test script runs inside a worker
that builds `$_POST` and `$_FILES` from `$r` with PHP's own variable parser, and its
output must match per-request mode's for the same request (see `run_one`).

Known divergences are listed by name in `scripts/phpt-uploads.known` — a test there is
expected to fail, with the reason beside it. A test that fails and is not listed is a
regression; a listed test that now passes is a stale entry, and fails the run too, so the
list never claims a divergence that is gone. Exit: 0 as expected, 1 not, 2 could not run.
"""

import argparse, glob, http.client, os, re, socket, subprocess, sys, tempfile, time

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))

# Worker mode: build the superglobals a worker script would, from what Askr hands over,
# then run the test's own code. Files are rebuilt in PHP's `$_FILES` shape by PHP's own
# bracket rules (parse_str), so a difference here is a difference in what Askr parsed.
WORKER = r"""<?php
$test = getenv('ASKR_PHPT_SCRIPT');
// One query string per attribute, through PHP's own variable parser — `[]`, nesting and
// name mangling then work exactly as when PHP registers `$_POST` and `$_FILES` itself.
function askr_phpt_parse(array $pairs): array {
    $q = [];
    foreach ($pairs as [$k, $v]) {
        $q[] = rawurlencode($k) . '=' . rawurlencode((string) $v);
    }
    parse_str(implode('&', $q), $out);
    return $out;
}
while (askr_handle_request(function (array $r) use ($test): int {
    $_POST = askr_phpt_parse(array_map(null, array_keys($r['post'] ?? []), $r['post'] ?? []));
    $_FILES = [];
    foreach (['name', 'full_path', 'type', 'tmp_name', 'error', 'size'] as $k) {
        $pairs = [];
        foreach ($r['files'] ?? [] as $f) {
            $at = strcspn($f['field'], '[');
            $pairs[] = [substr($f['field'], 0, $at) . "[$k]" . substr($f['field'], $at),
                        $f[$k]];
        }
        $one = askr_phpt_parse($pairs);
        if (in_array($k, ['error', 'size'], true)) {
            array_walk_recursive($one, function (&$x) { $x = (int) $x; });
        }
        $_FILES = array_replace_recursive($_FILES, $one);
    }
    include $test;
    return 200;
})) {}
"""


def sections(text):
    out, cur = {}, None
    for line in text.split("\n"):
        m = re.fullmatch(r"--([A-Z_]+)--", line)
        if m:
            cur = m.group(1)
            out[cur] = []
            continue
        if cur:
            out[cur].append(line)
    return {k: "\n".join(v) for k, v in out.items()}


def expectf(pattern):
    rep = {"%s": r"[^\r\n]+", "%S": r"[^\r\n]*", "%a": r".+", "%A": r".*", "%d": r"\d+",
           "%i": r"[+-]?\d+", "%f": r"[+-]?\.?\d+\.?\d*(?:[Ee][+-]?\d+)?", "%c": ".",
           "%e": re.escape(os.sep), "%w": r"\s*", "%x": r"[0-9a-fA-F]+", "%0": "\x00"}
    out, i = "", 0
    while i < len(pattern):
        two = pattern[i:i + 2]
        if two in rep:
            out += rep[two]
            i += 2
        else:
            out += re.escape(pattern[i])
            i += 1
    return re.compile(out, re.S)


def free_port():
    s = socket.socket()
    s.bind(("127.0.0.1", 0))
    port = s.getsockname()[1]
    s.close()
    return port


def request(sec, crlf):
    """The request run-tests.php builds from `--POST_RAW--`: the first `Content-Type:`
    line is the header, the rest is the body, joined with "\\n" — or with "\\r\\n", the
    line ending a real client sends, when `crlf`."""
    ctype, lines = "", []
    for line in sec["POST_RAW"].strip().split("\n"):
        if not ctype and re.match(r"(?i)^Content-Type:", line):
            ctype = line.split(":", 1)[1].replace("\r", "").strip()
            continue
        lines.append(line)
    return ctype, ("\r\n" if crlf else "\n").join(lines).encode()


def serve(askr, test, sec, mode, work, ctype, body, tag):
    """Run the test's script under Askr in `mode`, send it the request, return the output."""
    d = os.path.join(work, tag, test)
    os.makedirs(d, exist_ok=True)
    script = os.path.join(d, "test.php")
    with open(script, "w") as f:
        f.write(sec["FILE"])
    if mode == "worker":
        with open(os.path.join(d, "worker.php"), "w") as f:
            f.write(WORKER)
        with open(os.path.join(d, "index.php"), "w") as f:
            f.write("<?php\n")
    else:
        with open(os.path.join(d, "index.php"), "w") as f:
            f.write(sec["FILE"])
    ini = sec.get("INI", "").strip() + "\nhtml_errors=0\ndisplay_errors=1\nerror_reporting=E_ALL"
    env = dict(os.environ, ASKR_PHP_INI=ini, ASKR_PHPT_SCRIPT=script, RUST_LOG="askr=warn")

    # A port taken between choosing and binding is retried, never reported.
    for _ in range(5):
        port = free_port()
        cfg = os.path.join(d, "askr.toml")
        with open(cfg, "w") as f:
            f.write(f'[server]\nlisten = "127.0.0.1:{port}"\nroot = "{d}"\nworkers = "1"\n'
                    f'max_body_size = "64M"\n')
            if mode == "worker":
                f.write(f'[worker]\nscript = "{os.path.join(d, "worker.php")}"\n')
        log = open(os.path.join(d, "askr.log"), "w")
        p = subprocess.Popen([askr, "serve", "--config", cfg], env=env, stdout=log,
                             stderr=subprocess.STDOUT)
        got = None
        try:
            for _ in range(100):
                if p.poll() is not None:
                    break
                try:
                    c = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
                    c.request("POST", "/", body=body,
                              headers={"Content-Type": ctype} if ctype else {})
                    got = c.getresponse().read().decode("utf-8", "replace")
                    # Closed before the stop: an idle keep-alive holds a drain for 10 s.
                    c.close()
                    break
                except OSError:
                    time.sleep(0.1)
        finally:
            p.terminate()
            p.wait()
            log.close()
        if got is not None or "address already in use" not in open(log.name).read().lower():
            break
    if got is None:
        return None, "the server never answered — see " + log.name
    got = got.replace("\r\n", "\n").strip()
    with open(os.path.join(d, "got.txt"), "w") as f:
        f.write(got)
    return got, os.path.join(d, "got.txt")


# An upload's temporary path is the one thing two correct runs may print differently:
# PHP names it `php` + random characters, Askr's worker mode `askr-uploads/…`.
TMP = re.compile(r'string\(\d+\) "[^"]*(?:/php[A-Za-z0-9]+|/askr-uploads/[^"]*)"')


def run_one(askr, test, sec, mode, work):
    """Per-request mode: the request as php-src writes it, against `--EXPECT(F)--`.

    Worker mode: the same request with CRLF line endings, against what PHP itself makes of
    that request — per-request mode, whose agreement with PHP the first half establishes.
    CRLF because Askr's parser follows RFC 2046 and refuses bare-LF multipart, which no
    client sends and PHP tolerates; and PHP's answer to it rather than `--EXPECT--`,
    because the line endings change the sizes the expectation spells out."""
    if mode == "per-request":
        ctype, body = request(sec, crlf=False)
        got, where = serve(askr, test, sec, mode, work, ctype, body, mode)
        if got is None:
            return "error", where
        if "EXPECT" in sec:
            ok = got == sec["EXPECT"].strip()
        else:
            ok = bool(expectf(sec["EXPECTF"].strip()).fullmatch(got))
        return ("pass" if ok else "fail"), where
    ctype, body = request(sec, crlf=True)
    want, where = serve(askr, test, sec, "per-request", work, ctype, body, "worker-oracle")
    if want is None:
        return "error", where
    got, where = serve(askr, test, sec, "worker", work, ctype, body, "worker")
    if got is None:
        return "error", where
    norm = lambda s: TMP.sub('string(%d) "<tmp>"', s)
    return ("pass" if norm(got) == norm(want) else "fail"), where


def main():
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--askr", required=True)
    ap.add_argument("--php-src", help="php-src tree (default: the one build-libphp.sh unpacks)")
    ap.add_argument("--mode", choices=["per-request", "worker", "both"], default="both")
    ap.add_argument("--known", default=os.path.join(ROOT, "scripts", "phpt-uploads.known"))
    ap.add_argument("-v", "--verbose", action="store_true")
    a = ap.parse_args()

    src = a.php_src
    if not src:
        # The version build-libphp.sh builds, so the tests are those of the PHP under test.
        m = re.search(r'PHP_VERSION="\$\{PHP_VERSION:-([^}]+)\}"',
                      open(os.path.join(ROOT, "scripts", "build-libphp.sh")).read())
        version = os.environ.get("PHP_VERSION") or (m and m.group(1))
        src = os.path.join(ROOT, "vendor", "php-build", f"php-{version}")
    tests = sorted(glob.glob(os.path.join(src, "tests", "basic", "rfc1867_*.phpt")))
    if not tests:
        print(f"no rfc1867 tests under {src}: run scripts/build-libphp.sh, or pass --php-src",
              file=sys.stderr)
        return 2

    known = {}
    if os.path.exists(a.known):
        for line in open(a.known):
            line = line.split("#", 1)[0].strip()
            if line:
                mode, name, *why = line.split(None, 2)
                known[(mode, name)] = why[0] if why else ""

    modes = ["per-request", "worker"] if a.mode == "both" else [a.mode]
    work = tempfile.mkdtemp(prefix="askr-phpt-")
    unexpected, ran = [], 0
    for mode in modes:
        counts = {"pass": 0, "fail": 0, "skip": 0}
        for path in tests:
            name = os.path.basename(path)[: -len(".phpt")]
            sec = sections(open(path, encoding="utf-8", errors="replace").read())
            if "POST_RAW" not in sec or "FILE" not in sec or "EXTENSIONS" in sec or \
                    not ({"EXPECT", "EXPECTF"} & sec.keys()):
                counts["skip"] += 1
                continue
            result, where = run_one(a.askr, name, sec, mode, work)
            if result == "error":
                print(f"ERROR {mode:11} {name}: {where}")
                return 2
            ran += 1
            counts[result] += 1
            listed = (mode, name) in known
            if result == "fail" and not listed:
                unexpected.append(f"{mode} {name}: fails, and is not a known divergence ({where})")
            elif result == "pass" and listed:
                unexpected.append(f"{mode} {name}: passes now — remove it from {os.path.basename(a.known)}")
            if a.verbose or (result == "fail") != listed:
                tag = "known" if listed and result == "fail" else result
                print(f"{tag:5} {mode:11} {name}" + (f"  — {known[(mode, name)]}" if listed else ""))
        print(f"{mode}: {counts['pass']} pass, {counts['fail']} fail "
              f"({sum(1 for k in known if k[0] == mode)} known), {counts['skip']} skipped")
    if unexpected:
        print("\nNOT AS EXPECTED:\n  " + "\n  ".join(unexpected))
        return 1
    print(f"all {ran} as expected")
    return 0


if __name__ == "__main__":
    sys.exit(main())
