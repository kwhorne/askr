# Releasing Askr

For maintainers. A release touches **four pipelines and two package registries**, and
they don't all fail loudly — the Laravel package silently stopped reaching Packagist for
three weeks while every workflow showed a green check. Hence a checklist that ends in
verification rather than in `git push --tags`.

Everything below assumes a clean tree on `main` with CI green.

---

## 0. One-time: the release signing key

Do this once, before the first signed release. `askr upgrade` refuses a release whose
signature does not verify against the key embedded in the binary, so the key has to
exist before a build carries it.

```bash
cargo install rsign2                     # or: brew install minisign
rsign generate -W -c "askr release signing key" \
  -p keys/release.pub -s ~/.askr/askr-release.key
```

Both flags matter. `-W` makes the secret key passwordless, because CI has no terminal to
answer a prompt. **`-p` is what saves the public key** — without it `rsign generate`
prints it once and there is no file, and there is no subcommand to get it back
afterwards. If that happens, regenerate with `-f`; nothing is lost until a release has
been signed.

minisign and rsign2 implement the same format and are interoperable, so a key made by
either works with either. The release workflow uses rsign2 because that is what these
instructions generate with, and because `rsign sign -W` is non-interactive without
depending on how a passwordless key gets prompted for.

- **Commit `keys/release.pub`.** It is compiled into the binary (`include_str!` in
  `crates/askr/src/upgrade.rs`), which is the point: changing what an install trusts
  means changing the source and getting it released.
- **Put the contents of `askr-release.key` in the repository secret
  `MINISIGN_SECRET_KEY`**, keep an offline backup, and delete the local copy. It must
  never enter this repository.
- **Losing it locks out every future release.** There is no revocation here: a new key
  means installs built against the old one refuse the new tarballs and have to be
  reinstalled by hand. Back it up somewhere you would still have after losing this
  machine.

Until `keys/release.pub` holds a real key, `askr upgrade` verifies the download against
its own `.sha256` and nothing else — which proves it arrived intact and says nothing
about who produced it — and prints exactly that on every upgrade.

## 1. Decide the version

Askr follows [semver under the 1.x freeze](STABILITY.md): new capability arrives as
default-off config keys or new subcommands, so releases are `1.MINOR.0` for features and
`1.MINOR.PATCH` for fixes. If something in [`STABILITY.md`](STABILITY.md) has to change
meaning, stop and reconsider the design instead.

## 2. Bump the version

Five places, and they must agree:

```bash
V=1.4.0

sed -i '' "s/^version = \".*\"/version = \"$V\"/" Cargo.toml
cargo build --bin askr                  # refreshes Cargo.lock
sed -i '' "s/ASKR_VERSION=[0-9.]*/ASKR_VERSION=$V/g" Dockerfile
```

Then by hand: `README.md` (badge line, `VER=` in the install snippet, the
"What works today" heading, and a **new roadmap row**), `docs/README.md` (same),
and any doc that pins a version — `docs/ADMIN.md`, `docs/DOCKER.md`, `docs/UBUNTU.md`,
`docs/BENCHMARKS.md`, `docs/UPGRADING.md`.

```bash
grep -rn "1\.3\.0" README.md docs/*.md Cargo.toml Dockerfile | grep -v CHANGELOG
```

...should come back empty for the *previous* version, except in `CHANGELOG.md` and the
version-by-version notes in `docs/UPGRADING.md`, where history belongs.

## 3. Write the changelog

Move `## Unreleased` to `## <version> — <date>` and add a short lede saying what the
release is *for*. Entries explain the problem before the solution, and say what was
actually verified — "verified against a real Laravel 12 app" is worth more than a
feature list.

## 4. Update the upgrade guide

[`docs/UPGRADING.md`](UPGRADING.md) gets a `### To <version>` section: what's worth
adopting, in what order, and honestly **what can bite you**. Behaviour changes go here
even when they're bug fixes — someone may see a page stop being cached, and they deserve
the real reason.

## 5. Local gate

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
for f in sql-backend observ otel http3 "sql-backend observ otel http3"; do
  cargo clippy --workspace --all-targets --features "$f" -- -D warnings || break
done
cargo test --workspace           # unit + the e2e suite
```

And check no doc link rotted:

```bash
for f in README.md docs/*.md packages/laravel/README.md; do
  d=$(dirname "$f")
  grep -oE '\]\((\.\./)?[A-Za-z0-9_/.-]+\.md(#[a-z0-9-]+)?\)' "$f" \
    | sed -E 's/\]\(([^)#]+)(#[^)]*)?\)/\1/' \
    | while read -r l; do [ -f "$d/$l" ] || echo "BROKEN: $f -> $l"; done
done
```

## 6. Tag

Attribution must be `Knut W. Horne <kh@gets.no>` with no bot trailers:

```bash
git -c user.name="Knut W. Horne" -c user.email="kh@gets.no" \
    commit --author="Knut W. Horne <kh@gets.no>" -m "release: $V — <theme>"
git tag -a "v$V" -m "Askr $V — <theme>"
git push origin main && git push origin "v$V"
```

Keep the release commit separate from feature commits. (Easy to get wrong with
`git add -A` when the bump is already sitting in the tree.)

## 7. Watch all four pipelines

```bash
for w in CI Release Docker "Split askr-laravel"; do
  printf '%-22s ' "$w"; gh run list --workflow="$w" --limit 1 | awk '{print $1, $2}'
done
```

## 8. Verify the artefacts exist

Green is a claim; these are facts.

```bash
gh release view "v$V" --json assets --jq '.assets | length'      # expect 12

# Every tarball must carry a signature that verifies against the committed key. The
# release workflow already checks this before publishing; check it again from outside.
for a in $(gh release view "v$V" --json assets --jq '.assets[].name' | grep '\.tar\.gz$'); do
  gh release download "v$V" -p "$a" -p "$a.minisig" -D /tmp/relsig --clobber
  rsign verify -p keys/release.pub -x "/tmp/relsig/$a.minisig" "/tmp/relsig/$a" \
    && echo "$a signed ok"
done
rm -rf /tmp/relsig

# And the provenance attestation binds it to this workflow and commit.
gh attestation verify "/tmp/relsig/$a" --repo kwhorne/askr 2>/dev/null || true

MIN=${V%.*}
for t in "$V" "$MIN" latest "$V-full" "$MIN-full" full; do
  docker manifest inspect "ghcr.io/kwhorne/askr:$t" >/dev/null 2>&1 \
    && echo "askr:$t ok" || echo "askr:$t MISSING"
done
```

## 9. Verify the Laravel package actually published

**This is the step that has failed silently.** The `Split askr-laravel` workflow copies
`packages/laravel/` to the standalone repo Packagist watches, and it needs an
`ASKR_LARAVEL_SPLIT_TOKEN` secret to push to another repo. It now errors on a version
tag if the secret is missing, and asserts the tag landed — but verify anyway:

```bash
./scripts/publish-laravel-package.sh --check
```

It exits `0` published, `1` genuinely not published, `2` couldn't tell (usually an
api.github.com rate limit — 60 requests an hour unauthenticated). The three are
deliberately distinct: an early version of this script reported a freshly published tag
as **MISSING** when it had merely been rate-limited, which is a false alarm in the one
tool whose job is telling you the truth about a release.

### Publishing by hand

This is a supported route, not a workaround — if the `ASKR_LARAVEL_SPLIT_TOKEN` secret
isn't set, this is the process. Anyone with push rights to `kwhorne/askr-laravel` can run
it; no secret needed, since your own credentials are already good enough:

```bash
./scripts/publish-laravel-package.sh "v$V"     # or no argument to use the tag at HEAD
./scripts/publish-laravel-package.sh --check   # verify only
```

It splits **from the tag** rather than from whatever is checked out, refuses to publish a
tree with no `composer.json`, is safe to re-run, and ends by asserting that both the tag
and Packagist have it.

The `Split askr-laravel` workflow's final step checks the same thing without needing any
credentials, so it doesn't care whether the push was automatic or manual — **red means
the release genuinely isn't installable.** Publish, then re-run the job and it goes
green.

## 10. Verify like a user, not like an author

The point of the whole exercise. Install from the registries, not from disk:

```bash
docker run --rm "ghcr.io/kwhorne/askr:$V" askr --version

rsync -a --exclude vendor --exclude node_modules ~/code/laravel12/ /tmp/relcheck/
cd /tmp/relcheck && composer require "kwhorne/askr-laravel:^${V%.*}" --no-interaction
php -r 'require "vendor/autoload.php"; var_dump(class_exists("Askr\\Laravel\\AskrServiceProvider"));'
cd - && rm -rf /tmp/relcheck
```

Then exercise whatever the release actually claims, against the **published image** —
not the local build. Several genuine bugs this project has shipped were only visible
that way, and one "bug" turned out to be a missing `--admin` in a healthcheck.

---

## What has gone wrong before

Worth reading once; each line is a real incident.

- **A publish step reported success while doing nothing.** A missing-credential guard
  used `exit 0`. Three weeks of releases looked green. Guards on a release path must
  fail, and success must mean *published*.
- **The version bump got swallowed by a feature commit** because of `git add -A`.
  Harmless, but it makes `git log` lie about what a release contained.
- **A doc claimed a security property the code hadn't had for two versions.**
  Version-bumping is a good moment to grep docs for claims, not just numbers.
- **A test harness lied twice in one afternoon**: `composer --no-scripts` skipped package
  discovery, and a probe file 404'd because of our own (correct) security fix. Read the
  failure before believing the feature is broken.
