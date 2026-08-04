#!/usr/bin/env python3
"""Check every Markdown link in the repo: the file exists, and the #anchor exists in it.

Broken anchors are invisible on GitHub — the link renders fine and quietly lands at the
top of the page. One had been sitting in docs/DOCKER.md pointing at a section that was
right there, because `ubuntu:24.04` slugs to `ubuntu2404` (punctuation is dropped, not
turned into a separator).

Mirrors GitHub's slugger: strip formatting, lowercase, drop punctuation except hyphens
and underscores, then turn each space into one hyphen — note "A & B" becomes "a--b",
which is why collapsing whitespace here reports false breaks.
"""
import glob
import os
import re
import sys


def anchors(path: str) -> set[str]:
    found = set()
    for line in open(path, encoding="utf-8"):
        m = re.match(r"^#{1,6}\s+(.*)", line)
        if not m:
            continue
        text = m.group(1).strip().replace("`", "").lower()
        text = re.sub(r"[^\w\s-]", "", text)
        found.add(text.strip().replace(" ", "-"))
    return found


def main() -> int:
    files = ["README.md", "CONTRIBUTING.md", "SECURITY.md", "CHANGELOG.md"]
    files += glob.glob("docs/*.md") + glob.glob("packages/*/README.md")
    files += glob.glob("examples/**/*.md", recursive=True)

    cache: dict[str, set[str]] = {}
    broken = []
    for f in files:
        if not os.path.isfile(f):
            continue
        base = os.path.dirname(f) or "."
        body = open(f, encoding="utf-8").read()
        # Any relative link, not just .md — docs point at quickstart.yml, askr.toml,
        # example PHP scripts. A stale reference to one of those rots just as quietly,
        # and this checker missed them until a compose file was added.
        for m in re.finditer(r"\]\(([^)\s]*?)(#([A-Za-z0-9_-]+))?\)", body):
            target, anchor = m.group(1), m.group(3)
            if re.match(r"^[a-z][a-z0-9+.-]*:", target) or target.startswith("//"):
                continue  # external / mailto
            path = os.path.normpath(os.path.join(base, target)) if target else f
            if target and not os.path.exists(path):
                broken.append(f"{f} -> {target} (no such file)")
                continue
            if anchor and path.endswith(".md"):
                if path not in cache:
                    cache[path] = anchors(path)
                if anchor.lower() not in cache[path]:
                    broken.append(f"{f} -> {target or ''}#{anchor} (no such heading)")

    for b in broken:
        print(f"broken: {b}")
    print(f"{len(files)} files checked, {len(broken)} broken link(s)")
    return 1 if broken else 0


if __name__ == "__main__":
    sys.exit(main())
