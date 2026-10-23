#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -s)" = Darwin ] || { echo 'Only macOS is supported.' >&2; exit 1; }
case "$(uname -m)" in
  arm64) arch=aarch64; checksum=b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489 ;;
  x86_64) arch=x86_64; checksum=0387557ed1877bc6a2e1802c8391953baddba76081876301c522f52977b52ba7 ;;
  *) echo 'Unsupported architecture.' >&2; exit 1 ;;
esac
version=0.16.0
directory="zig-$arch-macos-$version"
mkdir -p .tools
if [ ! -x ".tools/$directory/zig" ]; then
  archive=$(mktemp -t tpane-zig)
  trap 'rm -f "$archive"' EXIT HUP INT TERM
  curl --fail --location --retry 3 "https://ziglang.org/download/$version/$directory.tar.xz" -o "$archive"
  printf '%s  %s\n' "$checksum" "$archive" | shasum -a 256 -c -
  tar -xJf "$archive" -C .tools
fi
ln -sfn "$directory" .tools/zig
.tools/zig/zig version
printf '\nBuild with: PATH="$PWD/.tools/zig:$PATH" cargo build --locked\n'
