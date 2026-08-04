#!/usr/bin/env bash
# Publish packages/laravel/ to kwhorne/askr-laravel, which Packagist watches.
#
# The GitHub workflow does this automatically when the ASKR_LARAVEL_SPLIT_TOKEN secret
# is set. Without it, this script is the supported route: anyone with push rights to the
# split repo can run it, no secret required.
#
#   ./scripts/publish-laravel-package.sh            # publish the tag at HEAD
#   ./scripts/publish-laravel-package.sh v1.4.0     # publish a specific tag
#   ./scripts/publish-laravel-package.sh --check     # only verify what's published
#
# Safe to re-run: an existing tag or release is left alone.

set -euo pipefail

REPO=kwhorne/askr-laravel
PREFIX=packages/laravel
WORKTREE=$(mktemp -d /tmp/askr-split.XXXXXX)
cleanup() { rm -rf "$WORKTREE"; }
trap cleanup EXIT

cd "$(dirname "$0")/.."

published() {
  local tag=$1 ok=0
  if curl -fsS "https://api.github.com/repos/$REPO/git/ref/tags/$tag" >/dev/null 2>&1; then
    echo "  tag $tag        present in $REPO"
  else
    echo "  tag $tag        MISSING from $REPO"; ok=1
  fi
  local vs
  vs=$(curl -fsS "https://repo.packagist.org/p2/$REPO.json" 2>/dev/null \
       | python3 -c 'import json,sys; print(" ".join(p["version"] for p in json.load(sys.stdin)["packages"]["'"$REPO"'"][:5]))' \
       2>/dev/null || echo "")
  if [[ " $vs " == *" $tag "* ]]; then
    echo "  packagist       serves $tag"
  else
    echo "  packagist       does not serve $tag yet (knows: ${vs:-nothing})"; ok=1
  fi
  return $ok
}

if [ "${1:-}" = "--check" ]; then
  TAG=$(git describe --tags --exact-match HEAD 2>/dev/null || git tag --sort=-v:refname | head -1)
  echo "checking $TAG"
  published "$TAG" && echo "published." || { echo "NOT fully published."; exit 1; }
  exit 0
fi

TAG="${1:-$(git describe --tags --exact-match HEAD 2>/dev/null || true)}"
[ -n "$TAG" ] || { echo "no tag at HEAD — pass one explicitly, e.g. $0 v1.4.0" >&2; exit 1; }
git rev-parse -q --verify "refs/tags/$TAG" >/dev/null \
  || { echo "tag $TAG does not exist locally" >&2; exit 1; }

# Split from the tag, not from HEAD: publishing whatever happens to be checked out is
# how you ship a package that doesn't match the release it claims to be.
echo "splitting $PREFIX at $TAG"
git clone -q --no-hardlinks . "$WORKTREE"
cd "$WORKTREE"
git checkout -q "$TAG"
SPLIT_SHA=$(git subtree split --prefix="$PREFIX" 2>/dev/null | tail -1)
[ -n "$SPLIT_SHA" ] || { echo "subtree split produced nothing" >&2; exit 1; }

# A package with no composer.json is not a package. Cheap check, real failure mode.
git ls-tree -r --name-only "$SPLIT_SHA" | grep -qx composer.json \
  || { echo "split has no composer.json — refusing to publish" >&2; exit 1; }
echo "  split $(git rev-parse --short "$SPLIT_SHA"), $(git ls-tree -r --name-only "$SPLIT_SHA" | wc -l | tr -d ' ') files"

echo "pushing to $REPO main"
git push --force "https://github.com/$REPO.git" "$SPLIT_SHA:refs/heads/main"

echo "tagging $TAG on $REPO"
V=${TAG#v}
gh release create "$TAG" --repo "$REPO" --target "$SPLIT_SHA" \
  --title "askr-laravel $V" \
  --notes "Laravel drivers for Askr $V. See the main CHANGELOG: https://github.com/kwhorne/askr/blob/main/CHANGELOG.md" \
  2>/dev/null || echo "  release $TAG already exists — left as is"

# Packagist's webhook is usually instant, but assert rather than assume.
echo "verifying"
for i in 1 2 3 4 5 6; do
  sleep 5
  if published "$TAG" >/dev/null 2>&1; then break; fi
done
published "$TAG" && echo "published." || {
  echo "NOT fully published. If the tag is present but Packagist isn't serving it," >&2
  echo "click Update on https://packagist.org/packages/$REPO and re-run with --check." >&2
  exit 1
}
