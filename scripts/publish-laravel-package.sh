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

# "I could not ask" is not the same answer as "it is not there". Unauthenticated
# api.github.com allows 60 requests an hour, and this script's first version reported a
# freshly published tag as MISSING when it had merely been rate-limited — a false alarm
# in the one tool whose job is to tell you the truth about a release. So: 200 means
# present, 404 means absent, anything else means unknown and says so.
#
# Prefers `gh api` when it's available, since that's authenticated and effectively
# unlimited.
tag_state() {
  local tag=$1 code out
  if command -v gh >/dev/null 2>&1; then
    if out=$(gh api "repos/$REPO/git/ref/tags/$tag" 2>&1); then
      echo present; return
    fi
    # gh exits non-zero for "not there" and for "something went wrong" alike, so read
    # the message: a definite 404 must not be downgraded to "unknown".
    case "$out" in
      *"Not Found"* | *404*) echo absent; return ;;
    esac
  fi
  code=$(curl -s -o /dev/null -w '%{http_code}' \
         "https://api.github.com/repos/$REPO/git/ref/tags/$tag" 2>/dev/null || echo 000)
  case "$code" in
    200) echo present ;;
    404) echo absent ;;
    *) echo "unknown:$code" ;;
  esac
}

published() {
  local tag=$1 ok=0 state vs
  state=$(tag_state "$tag")
  case "$state" in
    present) echo "  tag $tag        present in $REPO" ;;
    absent) echo "  tag $tag        MISSING from $REPO"; ok=1 ;;
    *) echo "  tag $tag        could not verify (HTTP ${state#unknown:} from api.github.com — rate limit?)"; ok=2 ;;
  esac

  if ! vs=$(curl -fsS "https://repo.packagist.org/p2/$REPO.json" 2>/dev/null); then
    echo "  packagist       could not reach repo.packagist.org"
    [ "$ok" = 0 ] && ok=2
    return $ok
  fi
  vs=$(printf '%s' "$vs" | python3 -c 'import json,sys; print(" ".join(p["version"] for p in json.load(sys.stdin)["packages"]["'"$REPO"'"][:5]))' 2>/dev/null || echo "")
  if [[ " $vs " == *" $tag "* ]]; then
    echo "  packagist       serves $tag"
  else
    echo "  packagist       does not serve $tag yet (knows: ${vs:-nothing})"; ok=1
    # Report what is *observable* rather than a mechanism. It is tempting to say a tag on an
    # existing commit fires no webhook — but earlier tags on this very commit did reach
    # Packagist, so that explanation is contradicted by the evidence and would send the next
    # person down the wrong path. What can be stated is that the content is identical, which
    # is what decides whether anyone is affected.
    local sha prev
    sha=$(git ls-remote --tags "https://github.com/$REPO.git" "refs/tags/$tag" 2>/dev/null | cut -f1)
    if [ -n "$sha" ]; then
      prev=$(git ls-remote --tags "https://github.com/$REPO.git" 2>/dev/null \
        | awk -v s="$sha" -v t="refs/tags/$tag" '$1==s && $2!=t {print $2}' \
        | sed 's|refs/tags/||' | tr '\n' ' ')
      if [ -n "${prev// /}" ]; then
        echo "  note            $tag points at the same commit as: ${prev% }"
        echo "                  so the package is byte-identical to those, and anyone on a"
        echo "                  ^1.4 constraint already has this code. Packagist has served"
        echo "                  earlier tags on this same commit, so this is most likely lag"
        echo "                  rather than something structurally broken — but it is not"
        echo "                  installable at this version number until it catches up."
      fi
    fi
  fi
  return $ok
}

if [ "${1:-}" = "--check" ]; then
  TAG=$(git describe --tags --exact-match HEAD 2>/dev/null || git tag --sort=-v:refname | head -1)
  echo "checking $TAG"
  published "$TAG" && { echo "published."; exit 0; }
  rc=$?
  if [ "$rc" = 2 ]; then
    echo "INCONCLUSIVE — could not determine. Not the same as broken; try again." >&2
    exit 2
  fi
  echo "NOT fully published." >&2
  exit 1
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
