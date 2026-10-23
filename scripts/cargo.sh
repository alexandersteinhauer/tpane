#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
if [ -x "$PWD/.tools/zig/zig" ]; then
    PATH="$PWD/.tools/zig:$PATH"
    export PATH
fi
exec cargo "$@"
