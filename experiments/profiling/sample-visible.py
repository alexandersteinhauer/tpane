import importlib.util
import json
from pathlib import Path
import subprocess
import sys

root = Path.cwd()
out = root / 'docs/performance/native-render-diagnosis'
out.mkdir(parents=True, exist_ok=True)
spec = importlib.util.spec_from_file_location('benchmark', root / 'scripts/benchmark.py')
bench = importlib.util.module_from_spec(spec)
spec.loader.exec_module(bench)
original = bench.HostStream
samplers = []
log = (out / 'sample.log').open('w')

class SampleStream(original):
    def write(self, data):
        if data == b'b':
            samplers.append(subprocess.Popen([
                '/usr/bin/sample', str(self.process.pid), '2', '1', '-file', str(out / 'native.sample.txt')],
                stdout=log, stderr=subprocess.STDOUT))
        return super().write(data)

bench.HostStream = SampleStream
result = out / 'native.sampled.json'
sys.argv = ['benchmark.py', '--binary', str(root / '.tools/perf/tpane-tokio-native'),
            '--panes', '1', '--trials', '1', '--keys', '3', '--idle-seconds', '.1',
            '--output-bytes', str(256 * 1024 * 1024), '--output', str(result)]
try:
    bench.main()
finally:
    codes = [p.wait(timeout=10) for p in samplers]
    log.close()
    if result.exists():
        data = json.loads(result.read_text())
        data['profile'] = {'tool': 'macOS sample', 'exit_codes': codes, 'performance_comparison_valid': False}
        result.write_text(json.dumps(data, indent=2) + '\n')
    if not codes or any(codes):
        raise SystemExit('sample failed; inspect log')
