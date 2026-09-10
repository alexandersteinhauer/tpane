#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
if [ -x "$PWD/.tools/zig/zig" ]; then
    PATH="$PWD/.tools/zig:$PATH"
fi
if [ -z "${SDKROOT:-}" ]; then
    SDKROOT=$(/usr/bin/xcrun --sdk macosx --show-sdk-path)
    # SDK 26.5's arm64e-only stubs are incompatible with Zig 0.15.2's linker.
    if [ "$(uname -m)" = arm64 ] && ! grep -m 1 '^targets:' "$SDKROOT/usr/lib/libSystem.tbd" | grep -q 'arm64-macos'; then
        older=/Library/Developer/CommandLineTools/SDKs/MacOSX15.4.sdk
        if [ ! -d "$older" ]; then
            echo 'Zig 0.15.2 needs an SDK with arm64 stubs. Set SDKROOT to a compatible SDK (e.g. macOS 15.4).' >&2
            exit 1
        fi
        SDKROOT=$older
    fi
fi
export SDKROOT
PATH="$PWD/scripts/sdk-bin:$PATH"
export PATH
exec cargo "$@"
