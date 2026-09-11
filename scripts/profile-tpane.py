#!/usr/bin/env python3
"""Sample a busy tpane process. Profiled timings are NOT benchmark comparisons."""
import argparse
import importlib.util
import json
from pathlib import Path
import subprocess
import sys

sys.dont_write_bytecode = True
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--output', type=Path, required=True)
parser.add_argument('--panes', type=int, default=8)
parser.add_argument('--busy', type=int, default=4)
args = parser.parse_args()
args.binary = args.binary.resolve(strict=True)
args.output = args.output.resolve()
if args.output == args.binary:
    parser.error('profile output must not overwrite the binary')
args.output.parent.mkdir(parents=True, exist_ok=True)
result_file = args.output.with_suffix('.profiled.json')
log_file = args.output.with_suffix('.sample.log')
spec = importlib.util.spec_from_file_location('busy_profile', Path(__file__).with_name('benchmark-busy.py'))
busy = importlib.util.module_from_spec(spec)
spec.loader.exec_module(busy)
original_stream = busy.CheckedStream
samplers = []
log = log_file.open('w')

class ProfileStream(original_stream):
    def write(self, data):
        if data == b'x' and not samplers:
            samplers.append(subprocess.Popen(
                ['/usr/bin/sample', str(self.process.pid), '2', '1', '-file', str(args.output)],
                stdout=log, stderr=log))
        return super().write(data)

busy.CheckedStream = ProfileStream
sys.argv = ['benchmark-busy.py', '--binary', str(args.binary), '--panes', str(args.panes),
            '--busy', str(args.busy), '--trials', '1', '--seconds', '5', '--keys', '20',
            '--output', str(result_file)]
try:
    busy.main()
finally:
    codes = []
    for sampler in samplers:
        try:
            codes.append(sampler.wait(timeout=10))
        except subprocess.TimeoutExpired:
            sampler.kill()
            codes.append(sampler.wait())
    log.close()
    if result_file.exists():
        result = json.loads(result_file.read_text())
        result['profile'] = {'tool': 'macOS sample', 'sampling_interval_ms': 1,
                             'duration_seconds': 2, 'output': str(args.output),
                             'tool_exit_codes': codes, 'performance_comparison_valid': False}
        result_file.write_text(json.dumps(result, indent=2) + '\n')
if not samplers or any(code != 0 for code in codes):
    raise SystemExit(f'Sampling failed; inspect {log_file}')
print(f'Profile: {args.output}; timings in {result_file} are perturbed by sampling.')
