#!/bin/sh
# Build tpane-core and run its tests on Linux, in Docker.
#
#     scripts/linux-check.sh [image] [cargo test arguments...]
#
# The image needs a Rust toolchain and apk (Alpine, musl, as hosts like rad
# build static Linux binaries); default rust:1-alpine. Zig comes from
# scripts/bootstrap-zig.sh, the cargo registry and git checkouts are shared
# with the host, and builds go to target/linux-<arch>.
set -eu
cd "$(dirname "$0")/.."
image="${1:-rust:1-alpine}"
[ "$#" -gt 0 ] && shift
root="$PWD"
cargo_home="${CARGO_HOME:-$HOME/.cargo}"
mkdir -p "$cargo_home/registry" "$cargo_home/git"
docker run --rm -i \
  -v "$root":/src \
  -v "$cargo_home/registry":/usr/local/cargo/registry \
  -v "$cargo_home/git":/usr/local/cargo/git \
  -w /src \
  "$image" sh -eu -s -- "$@" <<'SCRIPT'
apk add --no-cache -q bash curl git xz perl vim less linux-headers >/dev/null
arch=$(uname -m)
export CARGO_TARGET_DIR="/src/target/linux-$arch"
# The image's toolchain instead of rust-toolchain.toml's (which would be
# downloaded on every run); rust-version still bounds it from below.
RUSTUP_TOOLCHAIN=$(rustup default | cut -d' ' -f1)
export RUSTUP_TOOLCHAIN
TPANE_ZIG_LINK="zig-linux-$arch" ./scripts/bootstrap-zig.sh >/dev/null
export PATH="/src/.tools/zig-linux-$arch:$PATH"
rustc --version
zig version
cargo test -p tpane-core --locked --all-features "$@"
SCRIPT
