"""Where the current release number is written down, and how to find every copy of it.

One table of patterns, used by two tools: `check-docs.py` fails when any copy disagrees with
`Cargo.toml`, and `bump-version.py` rewrites all of them at release time. Sharing the table
is the point — a checker and a bumper that each keep their own list will drift, and the
drift is exactly what this exists to catch.

Before this, each release bumped 23 pins by hand from a grep for the *previous* version.
That grep cannot see a moving tag like `:1.6` once the release is 1.7.1, which is how
`examples/docker/quickstart.yml` kept teaching `:1.6` through two releases after it.

History is exempt: `CHANGELOG.md` as a whole, `docs/RELEASING.md` (whose numbers are worked
examples), and the "Version-by-version notes" section of `docs/UPGRADING.md`, where old
versions are the subject rather than a pin.
"""
import glob
import os
import re

# (kind, pattern). The captured group is the version; `kind` says which form it must match:
# "full" = MAJOR.MINOR.PATCH, "minor" = MAJOR.MINOR (a moving tag).
PATTERNS = [
    ("full", re.compile(r"VER=v(\d+\.\d+\.\d+)")),
    ("full", re.compile(r"askr:(\d+\.\d+\.\d+)(?=-full\b|\b)")),
    ("minor", re.compile(r"askr:(\d+\.\d+)(?=-full\b|\b)(?![\d.])")),
    ("minor", re.compile(r"`:(\d+\.\d+)(?:-full)?`")),
    ("full", re.compile(r"`(\d+\.\d+\.\d+)-full`")),
    ("full", re.compile(r"<strong>v(\d+\.\d+\.\d+)</strong>")),
    ("full", re.compile(r"Version \*\*(\d+\.\d+\.\d+)\*\*")),
    ("full", re.compile(r'"version":\s*"(\d+\.\d+\.\d+)"')),
    ("full", re.compile(r"ASKR_VERSION=(\d+\.\d+\.\d+)")),
    ("full", re.compile(r"askr-(\d+\.\d+\.\d+)-linux")),
    # A release line written out: "latest 1.7.x", "pair askr-laravel `1.7.x` with an Askr
    # `1.7.x`". The backtick form requires the backtick immediately before the digits, so
    # `PHP_VERSION=8.4.x` in BUILDING.md — a PHP version — is not mistaken for one of ours.
    # The askr-laravel pairing sat at `1.4.x` through three minor releases before this.
    ("minor", re.compile(r"latest (\d+\.\d+)\.x\b")),
    ("minor", re.compile(r"`(\d+\.\d+)\.x`")),
]

# The heading that opens the history section in UPGRADING.md; it runs to the next `## `.
HISTORY_HEADING = "## Version-by-version notes"


def files() -> list[str]:
    found = ["README.md", "Dockerfile"]
    found += glob.glob("docs/*.md")
    found += [p for p in glob.glob("examples/**/*", recursive=True) if os.path.isfile(p)]
    # CHANGELOG.md is history throughout. RELEASING.md is *about* version numbers — its
    # figures are worked examples ("bump to 1.8.0") and past incidents, never a pin a
    # reader copies — so scanning it only reports its own examples back as stale.
    exempt = ("CHANGELOG.md", "RELEASING.md")
    return sorted(f for f in set(found) if os.path.isfile(f) and not f.endswith(exempt))


def cargo_version() -> str:
    """The workspace version — the one number every pin must agree with."""
    for line in open("Cargo.toml", encoding="utf-8"):
        m = re.match(r'^version\s*=\s*"(\d+\.\d+\.\d+)"', line)
        if m:
            return m.group(1)
    raise SystemExit("no version in Cargo.toml")


def _lines_outside_history(path: str, lines: list[str]):
    """Yield (index, line) for every line that is a pin rather than history."""
    in_history = False
    for i, line in enumerate(lines):
        if line.startswith("## "):
            in_history = path.endswith("UPGRADING.md") and line.startswith(HISTORY_HEADING)
        if not in_history:
            yield i, line


def expected(kind: str, version: str) -> str:
    return version if kind == "full" else ".".join(version.split(".")[:2])


def scan(version: str):
    """Every pin that does not match `version`, as (file, line_no, found, wanted, text)."""
    wrong = []
    for f in files():
        try:
            lines = open(f, encoding="utf-8").read().split("\n")
        except UnicodeDecodeError:
            continue
        for i, line in _lines_outside_history(f, lines):
            for kind, pat in PATTERNS:
                for m in pat.finditer(line):
                    want = expected(kind, version)
                    if m.group(1) != want:
                        wrong.append((f, i + 1, m.group(1), want, line.strip()))
    return wrong


def rewrite(version: str) -> dict[str, int]:
    """Set every pin to `version` (or its MAJOR.MINOR). Returns replacements per file."""
    changed = {}
    for f in files():
        try:
            text = open(f, encoding="utf-8").read()
        except UnicodeDecodeError:
            continue
        lines = text.split("\n")
        count = 0
        for i, line in _lines_outside_history(f, lines):
            new = line
            for kind, pat in PATTERNS:
                want = expected(kind, version)

                def sub(m, want=want):
                    nonlocal count
                    if m.group(1) == want:
                        return m.group(0)
                    count += 1
                    s, e = m.span(1)
                    return m.group(0)[: s - m.start()] + want + m.group(0)[e - m.start() :]

                new = pat.sub(sub, new)
            lines[i] = new
        if count:
            open(f, "w", encoding="utf-8").write("\n".join(lines))
            changed[f] = count
    return changed
