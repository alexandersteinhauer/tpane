import cProfile
import importlib.util
import json
from pathlib import Path
import pstats
import subprocess
import sys

root = Path.cwd()
if len(sys.argv) != 2:
    raise SystemExit('Usage: python3 reproduce.py NEW_OUTPUT_DIRECTORY')
out = Path(sys.argv[1]).resolve()
out.mkdir(parents=True, exist_ok=False)
spec = importlib.util.spec_from_file_location('benchmark', root / 'scripts/benchmark.py')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
original = bench.HostStream
profile = cProfile.Profile()
debuggers = []
log = (out / 'lldb.log').open('w')

class DiagnosticStream(original):
    def write(self, data):
        if data == b'b':
            profile.enable()
            debuggers.append(subprocess.Popen([
                '/usr/bin/lldb', '--batch', '-p', str(self.process.pid),
                '-o', 'thread backtrace all', '-o', 'process detach'],
                stdout=log, stderr=subprocess.STDOUT))
        return super().write(data)

    def wait(self, pattern, timeout):
        try:
            return super().wait(pattern, timeout)
        finally:
            if b'TPB_DONE' in pattern.pattern:
                profile.disable()

bench.HostStream = DiagnosticStream
result = out / 'sync-uncapped.profiled.json'
sys.argv = ['benchmark.py', '--binary', str(root / '.tools/perf/frame-interval/sync-uncapped'),
            '--panes', '1', '--trials', '1', '--keys', '3', '--idle-seconds', '.1',
            '--output-bytes', str(256 * 1024 * 1024), '--output', str(result)]
try:
    bench.main()
finally:
    for debugger in debuggers:
        debugger.wait(timeout=20)
    log.close()
    profile.disable()
    with (out / 'driver-profile.txt').open('w') as stream:
        pstats.Stats(profile, stream=stream).sort_stats('tottime').print_stats(30)
    if result.exists():
        data = json.loads(result.read_text())
        data['profile'] = {'tools': ['LLDB', 'Python cProfile'], 'performance_comparison_valid': False}
        result.write_text(json.dumps(data, indent=2) + '\n')
