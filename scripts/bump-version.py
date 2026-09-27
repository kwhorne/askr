#!/usr/bin/env python3
"""Set the release number everywhere it is written down, then prove nothing was missed.

    python3 scripts/bump-version.py 1.8.0

Updates the workspace version in Cargo.toml, refreshes Cargo.lock, and rewrites every
version pin in the docs, the Dockerfile and the examples — exact pins to the new version,
moving tags (`:1.8`, `askr:1.8-full`) to its MAJOR.MINOR. History is left alone:
CHANGELOG.md, and the "Version-by-version notes" in docs/UPGRADING.md.

It replaced a step that was done by hand every release — a sed for Cargo.toml and the
Dockerfile, then a grep for the previous version and an edit per line. That grep cannot
see a moving tag once the patch number has moved on, which is how
examples/docker/quickstart.yml kept saying `:1.6` through two releases after it.

The patterns are shared with check-docs.py (version_pins.py), and this finishes by running
the same scan the checker runs, so "bumped" and "checked" mean the same thing.
"""
import re
import subprocess
import sys

import version_pins


def main() -> int:
    if len(sys.argv) != 2 or not re.fullmatch(r"\d+\.\d+\.\d+", sys.argv[1].lstrip("v")):
        print(__doc__.strip().split("\n")[2].strip(), file=sys.stderr)
        return 2
    new = sys.argv[1].lstrip("v")
    old = version_pins.cargo_version()

    text = open("Cargo.toml", encoding="utf-8").read()
    text = re.sub(r'(?m)^(version\s*=\s*)"\d+\.\d+\.\d+"', rf'\g<1>"{new}"', text, count=1)
    open("Cargo.toml", "w", encoding="utf-8").write(text)
    print(f"Cargo.toml: {old} -> {new}")

    # Workspace members only, and offline: this must not pull in dependency updates.
    r = subprocess.run(["cargo", "update", "--workspace", "--offline"], capture_output=True, text=True)
    print("Cargo.lock: refreshed" if r.returncode == 0 else f"Cargo.lock: FAILED\n{r.stderr}")

    for f, n in sorted(version_pins.rewrite(new).items()):
        print(f"{f}: {n} pin(s)")

    stale = version_pins.scan(new)
    for f, n, found, want, line in stale:
        print(f"still stale: {f}:{n}: {found} (want {want}) — {line}")
    print(f"{len(stale)} stale pin(s) remain")
    return 0 if not stale and r.returncode == 0 else 1


if __name__ == "__main__":
    sys.exit(main())
