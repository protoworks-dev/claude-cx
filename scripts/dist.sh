#!/bin/sh
# Build the release archives: cx for macOS (both chips) and Linux (both chips), and their checksums.
#
#   scripts/dist.sh            everything, into dist/
#   scripts/dist.sh macos      only the macOS pair (needs a Mac)
#   scripts/dist.sh linux      only the Linux pair (needs Docker, on any machine)
#
# macOS is built here with cargo. Linux is built in a Debian container, so the binaries need
# nothing newer than that Debian's glibc, and the tests are run there too: it is the only place
# this project's Linux side is compiled outside CI. The container keeps its own target directory
# in a Docker volume (cx-dist-target), so nothing of it lands in ./target.
#
# Each archive is cx-v<version>-<target>.tar.gz holding cx-v<version>-<target>/{cx,README.md,
# LICENSE,NOTICE}. That name is what Cargo.toml tells cargo-binstall and what install.sh fetches.
set -eu
cd "$(dirname "$0")/.."
what=${1:-all}
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[ -n "$version" ] || { echo "dist: no version in Cargo.toml" >&2; exit 1; }
# Debian 11: glibc 2.31, so the binaries run on Ubuntu 20.04 and anything newer.
image=${CX_DIST_IMAGE:-rust:1-bullseye}
mkdir -p dist

pack() { # pack <target> <path to the built cx>
  name="cx-v$version-$1"
  stage="dist/$name"
  mkdir -p "$stage"
  cp "$2" "$stage/cx"
  cp README.md LICENSE NOTICE "$stage/"
  tar -C dist -czf "dist/$name.tar.gz" "$name"
  echo "dist: dist/$name.tar.gz"
}

if [ "$what" = all ] || [ "$what" = macos ]; then
  [ "$(uname -s)" = Darwin ] || { echo "dist: the macOS archives need a Mac" >&2; exit 1; }
  for target in aarch64-apple-darwin x86_64-apple-darwin; do
    rustup target add "$target" >/dev/null
    cargo build --release --locked --target "$target"
    pack "$target" "target/$target/release/cx"
  done
fi

if [ "$what" = all ] || [ "$what" = linux ]; then
  command -v docker >/dev/null 2>&1 || { echo "dist: the Linux archives need Docker" >&2; exit 1; }
  # The container builds for its own chip natively and for the other one with Debian's cross
  # compiler (ring and the linker need a C toolchain for the target). The source is mounted
  # read-only; only dist/linux/ comes back.
  mkdir -p dist/linux
  docker run --rm \
    -v "$PWD":/src:ro -v "$PWD/dist/linux":/out \
    -v cx-dist-target:/target -v cx-dist-cargo:/usr/local/cargo/registry \
    -e CARGO_TARGET_DIR=/target -e CARGO_TERM_COLOR=never \
    "$image" sh -euc '
      cd /src
      native=$(rustc -vV | sed -n "s/^host: //p")
      case "$native" in
        aarch64-*) other=x86_64-unknown-linux-gnu; gnu=x86_64-linux-gnu; deb=amd64 ;;
        x86_64-*)  other=aarch64-unknown-linux-gnu; gnu=aarch64-linux-gnu; deb=arm64 ;;
        *) echo "dist: unknown container chip $native" >&2; exit 1 ;;
      esac
      echo "dist: $(rustc --version), glibc $(ldd --version | sed -n 1p | sed "s/.* //")"
      echo "dist: testing on $native"
      # Warnings are errors here as in CI: code only Linux compiles is checked nowhere else.
      RUSTFLAGS="-D warnings" cargo test --locked >/tmp/test.log 2>&1 || { tail -40 /tmp/test.log; echo "dist: the tests fail on Linux" >&2; exit 1; }
      grep -E "^test result" /tmp/test.log
      cargo build --release --locked --target "$native"
      cp "/target/$native/release/cx" "/out/cx-$native"
      apt-get update -qq >/dev/null && apt-get install -y -qq "gcc-$(echo $gnu | tr _ -)" "libc6-dev-$deb-cross" >/dev/null
      rustup target add "$other" >/dev/null
      upper=$(echo "$other" | tr "a-z-" "A-Z_")
      under=$(echo "$other" | tr - _)
      export "CARGO_TARGET_${upper}_LINKER=$gnu-gcc" "CC_$under=$gnu-gcc"
      cargo build --release --locked --target "$other"
      cp "/target/$other/release/cx" "/out/cx-$other"
      "/out/cx-$native" --version
    '
  for target in aarch64-unknown-linux-gnu x86_64-unknown-linux-gnu; do
    pack "$target" "dist/linux/cx-$target"
  done
fi

( cd dist && shasum -a 256 cx-v"$version"-*.tar.gz > SHA256SUMS && cat SHA256SUMS )
