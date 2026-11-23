#!/bin/sh
# Release the version in Cargo.toml: build the archives, create the GitHub release, publish the crate.
#
#   scripts/release.sh             the whole release
#   scripts/release.sh --dry-run   check everything and build the archives; create and publish nothing
#
# Run it on main, on the `chore(release): x.y.z` commit, with a clean tree. It refuses when the
# changelog has no section for the version or the tag already exists. A release is the four
# archives and SHA256SUMS on the GitHub release (install.sh and `cx upgrade` fetch them by name:
# a release without them breaks both), then the crate on crates.io.
#
# The crate is published only when cargo has a crates.io token (`cargo login`, or
# CARGO_REGISTRY_TOKEN). Without one the script says so and leaves that step to be run later:
# `cargo publish --locked`.
#
# The presence of this file also tells the organisation's merge hook that this repository is
# released by hand: it must not tag a version or create a release on its own.
set -eu
cd "$(dirname "$0")/.."
dry=0
[ "${1:-}" = "--dry-run" ] && dry=1

fail() { echo "release: $*" >&2; exit 1; }
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
tag="v$version"

[ -z "$(git status --porcelain)" ] || fail "the working tree has changes"
[ "$(git rev-parse --abbrev-ref HEAD)" = main ] || [ "$dry" = 1 ] || fail "releases are cut from main"
grep -q "^## \[$version\] - [0-9-]*$" CHANGELOG.md || fail "CHANGELOG.md has no '## [$version] - YYYY-MM-DD' section"
if git ls-remote --exit-code --tags origin "refs/tags/$tag" >/dev/null 2>&1; then
  fail "$tag already exists. Raise the version in Cargo.toml and CHANGELOG.md first."
fi

scripts/dist.sh all

# The release notes are the version's section of the changelog.
notes=$(mktemp)
trap 'rm -f "$notes"' EXIT INT TERM
{
  printf '## Install\n\n```sh\ncurl -fsSL https://raw.githubusercontent.com/protoworks-dev/claude-cx/main/install.sh | sh\n```\n\n'
  printf 'An installed cx comes up to date with `cx upgrade`.\n\n'
  awk -v v="$version" '
    /^## \[/ { on = ($2 == "[" v "]"); next }
    /^\[[^]]*\]: / { on = 0 }
    on { print }
  ' CHANGELOG.md
} >"$notes"

if [ "$dry" = 1 ]; then
  echo "release: dry run. Would create $tag with:"
  ls dist/cx-"$tag"-*.tar.gz dist/SHA256SUMS
  echo "release: notes:"
  cat "$notes"
  exit 0
fi

gh release create "$tag" --target "$(git rev-parse HEAD)" --title "cx $version" --notes-file "$notes" \
  dist/cx-"$tag"-*.tar.gz dist/SHA256SUMS
echo "release: $tag is on GitHub"

if [ -n "${CARGO_REGISTRY_TOKEN:-}" ] || grep -qs '^\[registry\]' "${CARGO_HOME:-$HOME/.cargo}/credentials.toml"; then
  cargo publish --locked
  echo "release: claude-cx $version is on crates.io"
else
  echo "release: no crates.io token here, so the crate was not published."
  echo "release: run 'cargo login' once, then 'cargo publish --locked' on this commit."
fi
