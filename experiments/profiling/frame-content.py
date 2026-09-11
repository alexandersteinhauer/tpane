import collections
import hashlib
import importlib.util
import json
from pathlib import Path
import sys

root = Path.cwd()
out = root / 'docs/performance/native-render-diagnosis/frame-content'
out.mkdir(parents=True, exist_ok=True)
spec = importlib.util.spec_from_file_location('benchmark', root / 'scripts/benchmark.py')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
original_stream = bench.HostStream
original_read = bench.os.read
active = None
pending = b''
frames = collections.Counter()
consecutive_duplicates = 0
last = None
sizes = collections.Counter()
examples = {}

class Capture(original_stream):
    def write(self, data):
        global active
        if data == b'b':
            active = self.fd
        return super().write(data)
    def wait(self, pattern, timeout):
        global active
        try:
            return super().wait(pattern, timeout)
        finally:
            if b'TPB_DONE' in pattern.pattern:
                active = None

def read(fd, n):
    global pending, last, consecutive_duplicates
    data = original_read(fd, n)
    if fd == active:
        pending += data
        parts = pending.split(bench.SYNC_END)
        pending = parts.pop()
        for part in parts:
            if b'\x1b[?2026h' not in part:
                continue
            key = hashlib.sha256(part).hexdigest()
            frames[key] += 1
            sizes[len(part)] += 1
            if key == last:
                consecutive_duplicates += 1
            last = key
            if len(examples) < 3 and key not in examples:
                examples[key] = {'size': len(part), 'prefix': repr(part[:200])}
    return data

variant = sys.argv[1]
workload = sys.argv[2] if len(sys.argv) > 2 else 'repeated'
if workload == 'varied':
    bench.__file__ = str(root / '.tools/perf/varied-output/benchmark.py')
binary = {'native': '.tools/perf/tpane-tokio-native', 'sync': 'target/release/tpane'}[variant]
name = variant if workload == 'repeated' else f'{variant}-{workload}'
result = out / f'{name}.profiled.json'
bench.HostStream = Capture
bench.os.read = read
sys.argv = ['benchmark.py', '--binary', str(root / binary), '--panes', '1', '--trials', '1',
            '--keys', '3', '--idle-seconds', '.1', '--output-bytes', str(32 * 1024 * 1024),
            '--output', str(result)]
try:
    bench.main()
finally:
    bench.os.read = original_read
    if result.exists():
        d = json.loads(result.read_text()); d['profile'] = {'tool':'wire frame hashing', 'performance_comparison_valid':False}
        result.write_text(json.dumps(d,indent=2)+'\n')
    report = {'performance_comparison_valid': False, 'scope': 'finite output only; segments containing sync-begin, ended by sync-end; extra standalone end markers excluded',
              'frames': sum(frames.values()), 'unique_wire_frames': len(frames),
              'consecutive_identical_frames': consecutive_duplicates,
              'most_common': frames.most_common(10), 'sizes': sizes, 'examples':examples}
    (out / f'{name}.frames.json').write_text(json.dumps(report,indent=2)+'\n')
