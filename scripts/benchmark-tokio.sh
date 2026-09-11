#!/bin/sh
set -eu
cd "$(dirname "$0")/.."
root=$PWD
experiment="$root/.tools/perf/tokio"
mkdir -p "$experiment/src" "$experiment/tests"
cp Cargo.toml Cargo.lock rust-toolchain.toml "$experiment/"
cp src/*.rs "$experiment/src/"
cp tests/*.rs "$experiment/tests/"
python3 - "$experiment" <<'PY'
from pathlib import Path
import sys
root = Path(sys.argv[1])
manifest = root / 'Cargo.toml'
manifest.write_text(manifest.read_text().replace('[dependencies]', '[dependencies]\ntokio = { version = "=1.53.1", features = ["rt", "sync", "time"] }'))
main = root / 'src/main.rs'
source = main.read_text()
for old, new in [
    ('    match run() {', '    let runtime = tokio::runtime::Builder::new_current_thread().enable_time().build().expect("Tokio runtime");\n    match runtime.block_on(run()) {'),
    ('fn run() -> Result<u8> {', 'async fn run() -> Result<u8> {'),
    ('wake.wait(timeout);', 'wake.wait(timeout).await;'),
]:
    if source.count(old) != 1:
        raise SystemExit(f'Prototype transformation no longer matches: {old!r}')
    source = source.replace(old, new)
main.write_text(source)
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
# An isolated target directory keeps the experimental binary out of the normal release path.
./scripts/cargo.sh build --release --manifest-path "$experiment/Cargo.toml" --target-dir "$root/target/perf-tokio"
cp "$root/target/perf-tokio/release/tpane" "$root/.tools/perf/tpane-tokio-wake"
printf '\nTokio prototype: %s/.tools/perf/tpane-tokio-wake\n' "$root"
