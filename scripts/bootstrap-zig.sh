#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
[ "$(uname -s)" = Darwin ] || { echo 'Only macOS is supported.' >&2; exit 1; }
case "$(uname -m)" in
  arm64) arch=aarch64; checksum=3cc2bab367e185cdfb27501c4b30b1b0653c28d9f73df8dc91488e66ece5fa6b ;;
  x86_64) arch=x86_64; checksum=375b6909fc1495d16fc2c7db9538f707456bfc3373b14ee83fdd3e22b3d43f7f ;;
  *) echo 'Unsupported architecture.' >&2; exit 1 ;;
esac
version=0.15.2
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
