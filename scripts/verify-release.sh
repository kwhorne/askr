#!/usr/bin/env bash
# Verify a published release from the outside, the way a user would meet it.
#
#   scripts/verify-release.sh v1.7.1
#   scripts/verify-release.sh v1.7.1 --laravel ~/code/app   # also serve a real Laravel app
#
# RELEASING.md §8–10, as one command. Every release so far was checked by hand with these
# same steps, and the hand-run version got two of them wrong in ways that looked like
# findings: the provenance digest was passed to a Python check as an argument where it read
# an environment variable, so every tarball reported "provenance MISSING"; and `:latest`
# was read from a stale local image and reported the previous release. So: provenance is
# matched on the file's own digest among the attested subjects, and image tags are compared
# by registry manifest, never by whatever `docker run` happens to have cached.
#
# Checks, in order: the release has its 12 assets; every tarball's minisign signature
# verifies against keys/release.pub, its .sha256 matches, and its SLSA provenance names this
# tag and the tag's commit; all six image tags exist for amd64 and arm64, and the moving
# tags point at this release; Packagist serves the Laravel package at this version; and the
# published image serves a request a framework accepts, over HTTP/1.0, HTTP/1.1 and HTTP/2,
# in both per-request and worker mode.
#
# Exits 0 when everything verifies, 1 when something is wrong, 2 when something could not be
# checked (network, rate limit, a missing tool) — "could not tell" is not "broken".

set -uo pipefail

TAG=${1:-}
[[ "$TAG" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] || { sed -n '4,5p' "$0" | sed 's/^# //' >&2; exit 2; }
shift
LARAVEL=""
while [ $# -gt 0 ]; do
  case "$1" in
    --laravel) LARAVEL=$2; shift 2 ;;
    *) echo "unknown argument: $1" >&2; exit 2 ;;
  esac
done

V=${TAG#v}
MIN=${V%.*}
REPO=kwhorne/askr
IMAGE=ghcr.io/kwhorne/askr
ROOT=$(cd "$(dirname "$0")/.." && pwd)
WORK=$(mktemp -d "${TMPDIR:-/tmp}/askr-verify.XXXXXX")
CONTAINERS=()
cleanup() {
  for c in "${CONTAINERS[@]+"${CONTAINERS[@]}"}"; do docker rm -f "$c" >/dev/null 2>&1; done
  rm -rf "$WORK"
}
trap cleanup EXIT

fails=0 unknown=0
ok() { printf 'ok    %s\n' "$1"; }
bad() { printf 'FAIL  %s\n' "$1"; fails=$((fails + 1)); }
unk() { printf '??    %s\n' "$1"; unknown=$((unknown + 1)); }

for tool in gh docker curl python3 shasum; do
  command -v "$tool" >/dev/null || { echo "missing tool: $tool" >&2; exit 2; }
done
VERIFY=""
if command -v rsign >/dev/null; then VERIFY=rsign; elif command -v minisign >/dev/null; then VERIFY=minisign; fi

# --- the release and its assets -------------------------------------------------------------
echo "== GitHub release $TAG"
assets=$(gh release view "$TAG" --repo "$REPO" --json assets --jq '.assets[].name' 2>/dev/null) \
  || { unk "could not read release $TAG (does it exist? rate-limited?)"; assets=""; }
n=$(printf '%s\n' "$assets" | grep -c . || true)
if [ -n "$assets" ]; then
  [ "$n" = 12 ] && ok "12 assets" || bad "$n assets (expected 12)"
fi
commit=$(git -C "$ROOT" rev-list -n1 "$TAG" 2>/dev/null || true)
[ -n "$commit" ] || unk "tag $TAG is not in the local clone — provenance commit cannot be compared"

for a in $(printf '%s\n' "$assets" | grep '\.tar\.gz$'); do
  if ! gh release download "$TAG" --repo "$REPO" -p "$a" -p "$a.minisig" -p "$a.sha256" -D "$WORK" --clobber 2>/dev/null; then
    unk "$a: could not download"; continue
  fi
  f="$WORK/$a"
  case "$VERIFY" in
    rsign) rsign verify -p "$ROOT/keys/release.pub" -x "$f.minisig" "$f" >/dev/null 2>&1 \
             && ok "$a: signature" || bad "$a: signature does not verify against keys/release.pub" ;;
    minisign) minisign -V -p "$ROOT/keys/release.pub" -x "$f.minisig" -m "$f" >/dev/null 2>&1 \
             && ok "$a: signature" || bad "$a: signature does not verify against keys/release.pub" ;;
    *) unk "$a: signature not checked (install rsign2 or minisign)" ;;
  esac
  want=$(awk '{print $1}' "$f.sha256"); got=$(shasum -a 256 "$f" | awk '{print $1}')
  [ "$want" = "$got" ] && ok "$a: sha256" || bad "$a: sha256 mismatch"
  # The digest goes in through the environment, and the file must be *among* the attested
  # subjects — one attestation covers both tarballs of a build job, and `gh attestation
  # verify` exiting 0 with no output is not evidence of anything on its own.
  prov=$(gh attestation verify "$f" --repo "$REPO" --format json 2>/dev/null \
    | DIGEST="$got" TAG="$TAG" COMMIT="$commit" python3 -c '
import json, os, sys
try:
    data = json.load(sys.stdin)
except Exception:
    print("unknown: no attestation returned"); sys.exit()
for at in data:
    subjects = at["verificationResult"]["statement"]["subject"]
    if not any(s["digest"].get("sha256") == os.environ["DIGEST"] for s in subjects):
        continue
    cert = at["verificationResult"]["signature"]["certificate"]
    ref, sha = cert.get("sourceRepositoryRef"), cert.get("sourceRepositoryDigest", "")
    # No backslashes inside f-string expressions: that needs Python 3.12, and macOS ships 3.9.
    tag, commit = os.environ["TAG"], os.environ["COMMIT"]
    if ref != "refs/tags/" + tag:
        print(f"bad: built from {ref}"); sys.exit()
    if commit and sha != commit:
        print(f"bad: built from commit {sha[:12]}, the tag is {commit[:12]}"); sys.exit()
    print(f"ok: {ref} @ {sha[:12]}"); sys.exit()
print("bad: this file is not among the attested subjects")
')
  case "$prov" in
    ok:*) ok "$a: provenance ${prov#ok: }" ;;
    bad:*) bad "$a: provenance — ${prov#bad: }" ;;
    *) unk "$a: provenance — ${prov#unknown: }" ;;
  esac
done

# --- images -----------------------------------------------------------------------------------
echo "== container images"
manifest_hash() {
  docker manifest inspect "$IMAGE:$1" 2>/dev/null | python3 -c '
import hashlib, json, sys
print(hashlib.sha256(json.dumps(json.load(sys.stdin), sort_keys=True).encode()).hexdigest())' 2>/dev/null
}
for t in "$V" "$MIN" latest "$V-full" "$MIN-full" full; do
  arches=$(docker manifest inspect "$IMAGE:$t" 2>/dev/null | python3 -c '
import json, sys
m = json.load(sys.stdin)
print(" ".join(sorted({x["platform"]["architecture"] for x in m.get("manifests", []) if x["platform"]["architecture"] != "unknown"})))' 2>/dev/null)
  case "$arches" in
    "amd64 arm64") ok "$IMAGE:$t (amd64, arm64)" ;;
    "") bad "$IMAGE:$t is missing" ;;
    *) bad "$IMAGE:$t has only: $arches" ;;
  esac
done
exact=$(manifest_hash "$V")
# A moving tag follows the newest release in its line, so it only has to point here when
# this *is* that release — verifying 1.5.1 after 1.5.2 shipped must not call :1.5 wrong.
line_newest=$(gh release list --repo "$REPO" --limit 100 --json tagName --jq '.[].tagName' 2>/dev/null \
  | grep -E "^v${MIN//./\\.}\.[0-9]+$" | sort -t. -k3 -n | tail -1)
if [ "$line_newest" = "$TAG" ]; then
  [ "$(manifest_hash "$MIN")" = "$exact" ] && ok ":$MIN points at $V" || bad ":$MIN does not point at $V"
elif [ -n "$line_newest" ]; then
  ok ":$MIN not expected to point here (${line_newest#v} is the newest $MIN.x)"
else
  unk "could not list releases to tell whether :$MIN should point here"
fi
# :latest follows the newest release, which a patch to an older line is not.
newest=$(gh release list --repo "$REPO" --limit 1 --json tagName --jq '.[0].tagName' 2>/dev/null)
if [ "$newest" = "$TAG" ]; then
  [ "$(manifest_hash latest)" = "$exact" ] && ok ":latest points at $V" || bad ":latest does not point at $V"
else
  ok ":latest not expected to move ($newest is the newest release)"
fi

# --- the Laravel package ----------------------------------------------------------------------
echo "== Packagist"
served=$(curl -fsS "https://repo.packagist.org/p2/kwhorne/askr-laravel.json" 2>/dev/null | python3 -c '
import json, sys
print(" ".join(p["version"] for p in json.load(sys.stdin)["packages"]["kwhorne/askr-laravel"]))' 2>/dev/null)
if [ -z "$served" ]; then unk "could not reach Packagist"
elif [[ " $served " == *" $TAG "* ]]; then ok "kwhorne/askr-laravel $TAG is installable"
else bad "Packagist does not serve kwhorne/askr-laravel $TAG — run scripts/publish-laravel-package.sh $TAG"
fi

# --- like a user: the published image serves requests a framework accepts ----------------------
echo "== the published image, serving"
docker pull -q "$IMAGE:$V" >/dev/null 2>&1 || unk "could not pull $IMAGE:$V"
reported=$(docker run --rm --entrypoint /opt/askr/askr "$IMAGE:$V" --version 2>/dev/null)
[ "$reported" = "askr $V" ] && ok "the image reports askr $V" || bad "the image reports '${reported:-nothing}'"

FIX="$ROOT/crates/askr/tests/fixtures"
mkdir -p "$WORK/public"
echo "<?php require '/fixtures/framework_check.php';" > "$WORK/public/index.php"
for mode in per-request worker; do
  cfg="$WORK/$mode.toml"
  printf '[server]\nlisten = "0.0.0.0:8080"\nroot = "/app/public"\n' > "$cfg"
  [ "$mode" = worker ] && printf '\n[worker]\nscript = "/fixtures/framework_worker.php"\n' >> "$cfg"
  port=$(python3 -c 'import socket; s=socket.socket(); s.bind(("127.0.0.1",0)); print(s.getsockname()[1])')
  name="askr-verify-$mode-$$"
  CONTAINERS+=("$name")
  docker run -d --name "$name" -p "127.0.0.1:$port:8080" \
    -v "$WORK/public:/app/public:ro" -v "$FIX:/fixtures:ro" -v "$cfg:/etc/askr/askr.toml:ro" \
    "$IMAGE:$V" serve --config /etc/askr/askr.toml >/dev/null 2>&1 || { unk "$mode: could not start the image"; continue; }
  deadline=$((SECONDS + 60)); up=""
  while [ "$SECONDS" -lt "$deadline" ]; do
    up=$(curl -s -o /dev/null -w '%{http_code}' --max-time 3 "http://127.0.0.1:$port/" 2>/dev/null)
    case "$up" in 200 | 4??) break ;; esac
    sleep 1
  done
  for proto in --http1.0 --http1.1 --http2-prior-knowledge; do
    body=$(curl -s --max-time 10 "$proto" -H "Cookie: a=1" "http://127.0.0.1:$port/probe?x=1" -w '\n%{http_code}')
    code=${body##*$'\n'}; body=${body%$'\n'*}
    if [ "$code" = 200 ]; then ok "$mode over ${proto#--}: a framework accepts the request"
    elif [ "$code" = 000 ]; then unk "$mode over ${proto#--}: no answer"
    else bad "$mode over ${proto#--}: $code — $body"
    fi
  done
done

if [ -n "$LARAVEL" ]; then
  echo "== a real Laravel app on the published image"
  "$ROOT/scripts/laravel-smoke.sh" --image "$IMAGE:$V" --app "$LARAVEL" | sed 's/^/  /'
  case "${PIPESTATUS[0]}" in 0) ok "Laravel smoke" ;; 1) bad "Laravel smoke" ;; *) unk "Laravel smoke could not run" ;; esac
fi

echo
if [ "$fails" -gt 0 ]; then echo "NOT VERIFIED: $fails failure(s), $unknown unchecked"; exit 1; fi
if [ "$unknown" -gt 0 ]; then echo "INCONCLUSIVE: $unknown check(s) could not run — not the same as broken"; exit 2; fi
echo "$TAG verified."
