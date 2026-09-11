#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
root=$PWD
variant=tokio-native
case "${1:-}" in
    '') ;;
    --readiness) variant=tokio-native-readiness ;;
    --byte-events) variant=tokio-native-byte-events ;;
    --single-message) variant=tokio-native-single ;;
    *) echo 'Usage: scripts/benchmark-tokio-native.sh [--readiness|--byte-events|--single-message]' >&2; exit 1 ;;
esac
experiment="$root/.tools/perf/tokio-native"
mkdir -p "$experiment/src" "$experiment/tests"
cp Cargo.toml Cargo.lock rust-toolchain.toml "$experiment/"
cp src/*.rs "$experiment/src/"
cp tests/*.rs "$experiment/tests/"
cp experiments/tokio-native/*.rs "$experiment/src/"
if [ "$variant" = tokio-native-readiness ]; then
    patch -d "$experiment/src" -p3 < experiments/tokio-native/readiness.patch
elif [ "$variant" != tokio-native ]; then
    patch -d "$experiment/src" -p3 < experiments/tokio-native/byte-events.patch
fi
if [ "$variant" = tokio-native-single ]; then
    patch "$experiment/src/main.rs" < experiments/tokio-native/single-message.patch
fi
python3 - "$experiment" "$variant" <<'PY'
from pathlib import Path
import sys
root = Path(sys.argv[1])
rt = 'rt-multi-thread' if sys.argv[2] == 'tokio-native' else 'rt'
def replace(path, pairs):
    source = path.read_text()
    for old, new in pairs:
        if source.count(old) != 1:
            raise SystemExit(f'Prototype transformation no longer matches in {path}: {old!r}')
        source = source.replace(old, new)
    path.write_text(source)
replace(root / 'Cargo.toml', [
    ('[dependencies]', '[dependencies]\ntokio = { version = "=1.53.1", features = ["' + rt + '", "sync", "time", "net", "macros", "signal", "process", "io-util"] }\ntokio-util = { version = "=0.7.19", features = ["time"] }\nfutures-util = "=0.3.34"'),
    ('features = ["use-dev-tty"]', 'features = ["use-dev-tty", "event-stream"]'),
])
replace(root / 'src/pty.rs', [
    ('use std::io::{Read, Write};', 'use std::io::Write;\n#[cfg(test)]\nuse std::io::Read;'),
    ('    pub fn reader(&self)', '    #[cfg(test)]\n    pub fn reader(&self)'),
    ('    pub fn take_writer(&mut self) -> Result<Box<dyn Write + Send>> {\n        self.writer.take().context("PTY writer already taken")\n    }',
     '    pub fn raw_fd(&self) -> Result<std::os::fd::RawFd> {\n        self.master.as_raw_fd().context("PTY has no file descriptor")\n    }\n    pub fn pid(&self) -> libc::pid_t { self.shell_pid }'),
])
replace(root / 'src/selection.rs', [('pub fn copy(text:', '#[allow(dead_code)]\npub fn copy(text:')])
replace(root / 'tests/terminal.rs', [
    ('CommandBuilder::new(env!("CARGO_BIN_EXE_tpane"))', 'CommandBuilder::new(std::env::var_os("TPANE_TEST_BINARY").unwrap_or_else(|| env!("CARGO_BIN_EXE_tpane").into()))'),
    ('        command.env("HOME", &home);', '        command.env("HOME", &home);\n        command.env("TPANE_TRACE_FILE", home.join("trace.log"));'),
])
tests = root / 'tests/terminal.rs'
tests.write_text(tests.read_text() + Path('experiments/tokio-native/host_pressure_tests.rs.inc').read_text())
PY
./scripts/cargo.sh build --release --manifest-path "$experiment/Cargo.toml" --target-dir "$root/target/perf-tokio-native"
cp "$root/target/perf-tokio-native/release/tpane" "$root/.tools/perf/tpane-$variant"
printf '\nTokio-native prototype: %s/.tools/perf/tpane-%s\n' "$root" "$variant"
