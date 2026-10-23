#!/bin/sh
# Install the checksum-verified Zig that builds libghostty-vt into .tools/zig.
set -eu
cd "$(dirname "$0")/.."
version=0.16.0
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-macos; checksum=b23d70deaa879b5c2d486ed3316f7eaa53e84acf6fc9cc747de152450d401489 ;;
  Darwin-x86_64) target=x86_64-macos; checksum=0387557ed1877bc6a2e1802c8391953baddba76081876301c522f52977b52ba7 ;;
  Linux-x86_64) target=x86_64-linux; checksum=70e49664a74374b48b51e6f3fdfbf437f6395d42509050588bd49abe52ba3d00 ;;
  Linux-aarch64 | Linux-arm64) target=aarch64-linux; checksum=ea4b09bfb22ec6f6c6ceac57ab63efb6b46e17ab08d21f69f3a48b38e1534f17 ;;
  *) echo "Unsupported platform: $(uname -s) $(uname -m)" >&2; exit 1 ;;
esac
directory="zig-$target-$version"
mkdir -p .tools
if [ ! -x ".tools/$directory/zig" ]; then
  archive=$(mktemp "${TMPDIR:-/tmp}/tpane-zig.XXXXXX")
  trap 'rm -f "$archive"' EXIT HUP INT TERM
  curl --fail --location --retry 3 "https://ziglang.org/download/$version/$directory.tar.xz" -o "$archive"
  if command -v sha256sum >/dev/null 2>&1; then
    printf '%s  %s\n' "$checksum" "$archive" | sha256sum -c -
  else
    printf '%s  %s\n' "$checksum" "$archive" | shasum -a 256 -c -
  fi
  tar -xJf "$archive" -C .tools
fi
# Another link name keeps a container's Linux Zig apart from the host's.
link="${TPANE_ZIG_LINK:-zig}"
ln -sfn "$directory" ".tools/$link"
".tools/$link/zig" version
printf '\nBuild with: ./scripts/cargo.sh build --locked\n'
