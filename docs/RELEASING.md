# Releasing Askr

For maintainers. A release touches **four pipelines and two package registries**, and
they don't all fail loudly — the Laravel package silently stopped reaching Packagist for
three weeks while every workflow showed a green check. Hence a checklist that ends in
verification rather than in `git push --tags`.

Everything below assumes a clean tree on `main` with CI green.

---

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
gh release view "v$V" --json assets --jq '.assets | length'      # expect 8

MIN=${V%.*}
for t in "$V" "$MIN" latest "$V-full" "$MIN-full" latest-full; do
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
gh api "repos/kwhorne/askr-laravel/git/ref/tags/v$V" --jq .ref
curl -s "https://repo.packagist.org/p2/kwhorne/askr-laravel.json" \
  | python3 -c "import json,sys; print(json.load(sys.stdin)['packages']['kwhorne/askr-laravel'][0]['version'])"
```

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
