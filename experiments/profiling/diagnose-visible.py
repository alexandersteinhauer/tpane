import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

root = Path.cwd()
p = argparse.ArgumentParser()
p.add_argument('variant', choices=['uncapped', 'capped', 'read-budget', 'queue-first', 'input-task', 'combined'])
p.add_argument('--raw', action='store_true')
a = p.parse_args()
name = a.variant + ('-raw' if a.raw else '-profile')
base = root / '.tools/perf/visible-diagnosis'
base.mkdir(exist_ok=True)
out = root / 'docs/performance/native-render-diagnosis'
out.mkdir(parents=True, exist_ok=True)
source = base / (name + '.source')
shutil.copytree(root / '.tools/perf/tokio-native', source)
if a.variant == 'capped':
    for f in ['main', 'bus', 'output']:
        shutil.copy2(root / f'.tools/perf/frame-interval/native-{f}-capped.rs', source / f'src/{f}.rs')
if a.variant == 'read-budget':
    path = source / 'src/transport.rs'
    text = path.read_text()
    for old,new in [
        ('    let credits = Arc::new(Semaphore::new(OUTPUT_CREDITS));\n    let mut buffer = [0; CHUNK];', '    let credits = Arc::new(Semaphore::new(OUTPUT_CREDITS));\n    let mut buffer = [0; CHUNK];\n    let mut reads = 0;'),
        ('        // AsyncFd can stay ready forever. Bound work even on a hot descriptor.\n        tokio::task::yield_now().await;',
         '        reads += 1;\n        if reads == 64 {\n            reads = 0;\n            tokio::task::yield_now().await;\n        }')]:
        assert text.count(old) == 1,old
        text = text.replace(old,new)
    path.write_text(text)

if a.variant in ['queue-first', 'combined']:
    path = source / 'src/main.rs'
    text = path.read_text()
    for old,new in [
        ('        let mut processed = 0;', '        let mut processed = 0;\n        let mut since_frame = 0;'),
        ('let render = app.needs_render() && !output_busy;', 'let render = app.needs_render() && !output_busy && (data.is_empty() || since_frame >= 64);'),
        ('                        output.send_frame(frame)?;', '                        since_frame = 0;\n                        output.send_frame(frame)?;'),
        ('                        processed += batch.len();', '                        since_frame += batch.len();\n                        processed += batch.len();')]:
        assert text.count(old) == 1,old
        text = text.replace(old,new)
    path.write_text(text)
if a.variant in ['input-task', 'combined']:
    path = source / 'src/main.rs'
    text = path.read_text()
    for old,new in [
        ('    let mut input = EventStream::new();', '''    let (input_tx, mut input) = mpsc::channel(128);
    let input_task = tokio::spawn(async move {
        let mut stream = EventStream::new();
        while let Some(event) = stream.next().await {
            let failed = event.is_err();
            if input_tx.send(event).await.is_err() || failed { break; }
        }
    });'''),
        ('    drop(input);', '    drop(input);\n    input_task.abort();\n    let _ = input_task.await;'),
        ('    input: &mut EventStream,', '    input: &mut mpsc::Receiver<std::io::Result<Event>>,'),
        ('event = input.next() =>', 'event = input.recv() =>')]:
        assert text.count(old) == 1,old
        text = text.replace(old,new)
    path.write_text(text)

spec = importlib.util.spec_from_file_location('instrument', root / 'scripts/profile-instrumented.py')
helper = importlib.util.module_from_spec(spec)
spec.loader.exec_module(helper)
project = base / (name + '.project')
metadata = {'variant': a.variant, 'instrumented': not a.raw, 'performance_comparison_valid': a.raw}
if a.raw:
    shutil.copytree(source, project)
else:
    metadata['snapshot'] = helper.prepare_project(source, project, 'native')
    main = project / 'src/main.rs'
    text = main.read_text().replace('mod profile;', 'mod profile;\nmod diag;')
    text = text.replace('fn main() -> ExitCode {', 'fn main() -> ExitCode {\n    let _diag = diag::Report;')
    text = text.replace('                    let mut frame = Vec::new();', '                    diag::render(data.len());\n                    let mut frame = Vec::new();')
    text = text.replace('                        processed += batch.len();', '                        diag::batch(batch.len());\n                        processed += batch.len();')
    main.write_text(text)
    (project / 'src/diag.rs').write_text('''use std::sync::{Mutex, atomic::{AtomicUsize, Ordering}};
static BATCHES: Mutex<[u64; 65]> = Mutex::new([0; 65]);
static RENDERS: AtomicUsize = AtomicUsize::new(0);
static QUEUED: AtomicUsize = AtomicUsize::new(0);
pub fn batch(n: usize) { BATCHES.lock().unwrap()[n] += 1; }
pub fn render(queued: usize) {
    RENDERS.fetch_add(1, Ordering::Relaxed);
    if queued != 0 { QUEUED.fetch_add(1, Ordering::Relaxed); }
}
pub struct Report;
impl Drop for Report {
    fn drop(&mut self) {
        if let Some(path) = std::env::var_os("TPANE_DIAG_FILE") {
            let counts = BATCHES.lock().unwrap();
            let json = format!("{{\\\"performance_comparison_valid\\\":false,\\\"batch_histogram\\\":{:?},\\\"renders\\\":{},\\\"renders_with_queued_data\\\":{}}}\\n",
                &*counts, RENDERS.load(Ordering::Relaxed), QUEUED.load(Ordering::Relaxed));
            std::fs::write(path, json).unwrap();
        }
    }
}
''')
metadata['project_hashes'] = helper.hashes(project)
(out / (name + '.source.json')).write_text(json.dumps(metadata, indent=2) + '\n')
with (base / (name + '.build.log')).open('w') as log:
    subprocess.run([str(root / 'scripts/cargo.sh'), 'build', '--release', '--locked',
                    '--manifest-path', str(project / 'Cargo.toml'),
                    '--target-dir', str(root / 'target/perf-profile')], check=True, stdout=log, stderr=subprocess.STDOUT)
binary = base / name
shutil.copy2(root / 'target/perf-profile/release/tpane', binary)
if a.raw:
    print(binary)
    sys.exit(0)
spec = importlib.util.spec_from_file_location('benchmark', root / 'scripts/benchmark.py')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
original = subprocess.Popen

def launch(*args, **kwargs):
    if args and isinstance(args[0], (list, tuple)) and str(args[0][0]) == str(binary):
        kwargs['env'] = dict(kwargs['env'], TPANE_PROFILE_FILE=str(out / (name + '.profile.json')),
                             TPANE_DIAG_FILE=str(out / (name + '.batches.json')))
    return original(*args, **kwargs)

subprocess.Popen = launch
result = out / (name + '.profiled.json')
sys.argv = ['benchmark.py', '--binary', str(binary), '--panes', '1', '--trials', '1',
            '--keys', '3', '--idle-seconds', '.1', '--output-bytes', str(32 * 1024 * 1024),
            '--output', str(result)]
try:
    bench.main()
finally:
    subprocess.Popen = original
    if result.exists():
        d=json.loads(result.read_text());d['profile']={'performance_comparison_valid':False, 'tool':'instrumented synchronous spans and batch counters'}
        result.write_text(json.dumps(d,indent=2)+'\n')
helper.validate_profile(out / (name + '.profile.json'))
json.loads((out / (name + '.batches.json')).read_text())
