#!/bin/sh
# Install cx (claude-cx) from a prebuilt binary. No Rust toolchain needed.
#
#   curl -fsSL https://raw.githubusercontent.com/protoworks-dev/claude-cx/main/install.sh | sh
#
#   CX_VERSION=0.6.0   a version other than the latest release
#   CX_INSTALL_DIR=…   where cx goes (default: ~/.local/bin)
#
# It downloads the archive for this machine from the GitHub release, checks it against the
# release's SHA256SUMS, and copies one file, cx, into the install directory. Nothing else is
# changed. macOS and Linux, on Apple silicon, ARM64 and x86-64.
set -eu
repo="protoworks-dev/claude-cx"
dir=${CX_INSTALL_DIR:-"$HOME/.local/bin"}

fail() { echo "install: $*" >&2; exit 1; }
need() { command -v "$1" >/dev/null 2>&1 || fail "$1 is needed"; }
need curl; need tar; need uname

case "$(uname -s)" in
  Darwin) os=apple-darwin ;;
  Linux) os=unknown-linux-gnu ;;
  *) fail "no prebuilt cx for $(uname -s). Build it: cargo install --locked claude-cx" ;;
esac
case "$(uname -m)" in
  arm64 | aarch64) chip=aarch64 ;;
  x86_64 | amd64) chip=x86_64 ;;
  *) fail "no prebuilt cx for $(uname -m). Build it: cargo install --locked claude-cx" ;;
esac
target="$chip-$os"

version=${CX_VERSION:-}
if [ -z "$version" ]; then
  # The latest release's tag, from the address GitHub redirects /releases/latest to.
  latest=$(curl -fsSLI -o /dev/null -w '%{url_effective}' "https://github.com/$repo/releases/latest") ||
    fail "could not reach github.com"
  version=${latest##*/v}
  case "$version" in "" | *[!0-9.]*) fail "could not read the latest version from $latest" ;; esac
fi
version=${version#v}
name="cx-v$version-$target"
base="https://github.com/$repo/releases/download/v$version"

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT INT TERM
echo "install: cx $version for $target"
curl -fsSL -o "$tmp/$name.tar.gz" "$base/$name.tar.gz" || fail "no archive at $base/$name.tar.gz"
curl -fsSL -o "$tmp/SHA256SUMS" "$base/SHA256SUMS" || fail "no checksums at $base/SHA256SUMS"

want=$(awk -v f="$name.tar.gz" '$2 == f { print $1 }' "$tmp/SHA256SUMS")
[ -n "$want" ] || fail "SHA256SUMS does not list $name.tar.gz"
if command -v sha256sum >/dev/null 2>&1; then
  got=$(sha256sum "$tmp/$name.tar.gz" | awk '{ print $1 }')
else
  need shasum
  got=$(shasum -a 256 "$tmp/$name.tar.gz" | awk '{ print $1 }')
fi
[ "$got" = "$want" ] || fail "the download does not match its checksum (got $got, want $want)"

tar -C "$tmp" -xzf "$tmp/$name.tar.gz"
mkdir -p "$dir"
# Written beside the old one and renamed, so a running cx is not cut off mid-file.
cp "$tmp/$name/cx" "$dir/.cx.new"
chmod 755 "$dir/.cx.new"
mv -f "$dir/.cx.new" "$dir/cx"
echo "install: $("$dir/cx" --version) is at $dir/cx"
case ":$PATH:" in
  *":$dir:"*) ;;
  *) echo "install: $dir is not on your PATH. Add it, or run $dir/cx" ;;
esac
