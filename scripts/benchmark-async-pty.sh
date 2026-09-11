#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
root=$PWD
variant=async-pty
case "${1:-}" in
    '') ;;
    --batched) variant=async-pty-batched ;;
    *) echo 'Usage: scripts/benchmark-async-pty.sh [--batched]' >&2; exit 1 ;;
esac
experiment="$root/.tools/perf/async-pty"
mkdir -p "$experiment/src" "$experiment/tests"
cp Cargo.toml Cargo.lock rust-toolchain.toml "$experiment/"
cp src/*.rs "$experiment/src/"
cp tests/*.rs "$experiment/tests/"
cp experiments/async-pty/io_worker.rs "$experiment/src/io_worker.rs"
if [ "$variant" = async-pty-batched ]; then
    patch "$experiment/src/io_worker.rs" < experiments/async-pty/batched.patch
fi
python3 - "$experiment" <<'PY'
from pathlib import Path
import sys
root = Path(sys.argv[1])

def replace(path, pairs):
    source = path.read_text()
    for old, new in pairs:
        if source.count(old) != 1:
            raise SystemExit(f'Prototype transformation no longer matches in {path}: {old!r}')
        source = source.replace(old, new)
    path.write_text(source)

replace(root / 'Cargo.toml', [('[dependencies]', '[dependencies]\ntokio = { version = "=1.53.1", features = ["rt", "sync", "time", "net", "macros"] }')])
replace(root / 'src/main.rs', [
    ('    match run() {', '    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("Tokio runtime");\n    match runtime.block_on(run()) {'),
    ('fn run() -> Result<u8> {', 'async fn run() -> Result<u8> {'),
    ('wake.wait(timeout);', 'wake.wait(timeout).await;'),
    ('if more_output || input_budget_exhausted {\n            continue;', 'if more_output || input_budget_exhausted {\n            tokio::task::yield_now().await;\n            continue;'),
])
replace(root / 'src/pane.rs', [
    ('    sync::mpsc::{Receiver, TryRecvError},\n', ''),
    ('use anyhow::{Context, Result};', 'use anyhow::{Context, Result};\nuse tokio::sync::mpsc::{Receiver, error::TryRecvError};'),
    ('    writer: Writer,', '    writer: Writer,\n    _tasks: io_worker::Tasks,'),
    ('        let mut session = Session::spawn_in(shell, rows, cols, cwd)?;\n        let output = io_worker::read_output(session.reader()?, wake.clone())?;\n        let writer = Writer::new(session.take_writer()?, wake)?;',
     '        let session = Session::spawn_in(shell, rows, cols, cwd)?;\n        let connection = io_worker::connect(session.raw_fd()?, wake)?;'),
    ('            output,\n            writer,', '            output: connection.output,\n            writer: connection.writer,\n            _tasks: connection.tasks,'),
])
replace(root / 'src/pty.rs', [
    ('use std::io::{Read, Write};', 'use std::io::Write;\n#[cfg(test)]\nuse std::io::Read;'),
    ('    pub fn reader(&self)', '    #[cfg(test)]\n    pub fn reader(&self)'),
    ('    pub fn take_writer(&mut self) -> Result<Box<dyn Write + Send>> {\n        self.writer.take().context("PTY writer already taken")\n    }',
     '    pub fn raw_fd(&self) -> Result<std::os::fd::RawFd> {\n        self.master.as_raw_fd().context("PTY has no file descriptor")\n    }'),
])
replace(root / 'src/app.rs', [
    ('    #[test]\n    fn a_clean_render_does_not_count_as_a_presented_frame()', '    #[tokio::test(flavor = "current_thread")]\n    async fn a_clean_render_does_not_count_as_a_presented_frame()'),
])
(root / 'src/wake.rs').write_text('''use std::{sync::Arc, time::Duration};

#[derive(Clone)]
pub struct Wake(Arc<tokio::sync::Notify>);

impl Wake {
    pub fn new() -> Self { Self(Arc::new(tokio::sync::Notify::new())) }
    pub fn notify(&self) { self.0.notify_one(); }
    pub async fn wait(&self, timeout: Duration) {
        let _ = tokio::time::timeout(timeout, self.0.notified()).await;
    }
}
''')
PY
./scripts/cargo.sh build --release --manifest-path "$experiment/Cargo.toml" --target-dir "$root/target/perf-async-pty"
cp "$root/target/perf-async-pty/release/tpane" "$root/.tools/perf/tpane-$variant"
printf '\nAsync PTY prototype: %s/.tools/perf/tpane-%s\n' "$root" "$variant"
