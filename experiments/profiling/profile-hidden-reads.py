import hashlib, importlib.util, json, shutil, subprocess, sys
from pathlib import Path
root=Path.cwd();variant=sys.argv[1]
assert variant in ['control','yield64']
source=root/('.tools/perf/tokio-native' if variant=='control' else '.tools/perf/hidden-io/yield64')
project=root/f'.tools/perf/hidden-io/{variant}-profile'
out=root/'docs/performance/hidden-io'
spec=importlib.util.spec_from_file_location('instrument',root/'scripts/profile-instrumented.py')
helper=importlib.util.module_from_spec(spec);spec.loader.exec_module(helper)
metadata=helper.prepare_project(source,project,'native')
transforms=metadata['transforms']
for filename,old,new in [
 ('main.rs','mod profile;','mod profile;\nmod read_probe;'),
 ('main.rs','fn main() -> ExitCode {','fn main() -> ExitCode {\n    let _read_probe = read_probe::Report;'),
 ('main.rs','                        processed += batch.len();','                        read_probe::batch(batch.len());\n                        processed += batch.len();'),
 ('transport.rs','    let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };','    let count = unsafe { libc::read(fd, bytes.as_mut_ptr().cast(), bytes.len()) };\n    crate::read_probe::read(count);')]:
 helper.replace_once(project,filename,old,new,transforms)
shutil.copy2(root/'experiments/profiling/read_probe.rs',project/'src/read_probe.rs')
metadata['instrumented_sha256']=helper.hashes(project)
(out/f'{variant}-read-profile.source.json').write_text(json.dumps(metadata,indent=2)+'\n')
# Refresh every snapshot source immediately before sharing a target cache.
for path in (project/'src').glob('*.rs'):path.touch()
with (project/'build.log').open('w') as log:
 subprocess.run([str(root/'scripts/cargo.sh'),'build','--release','--locked','--manifest-path',str(project/'Cargo.toml'),'--target-dir',str(root/'target/perf-hidden-io')],check=True,stdout=log,stderr=subprocess.STDOUT)
binary=root/f'.tools/perf/hidden-io/{variant}-profiled'
shutil.copy2(root/'target/perf-hidden-io/release/tpane',binary)
result=out/f'{variant}-read-profile.profiled.json';profile=out/f'{variant}-read-profile.json'
try:
 helper.run_busy(binary,profile,result,24,8)
finally:
 if result.exists():
  d=json.loads(result.read_text());d['profile']={'tool':'synchronous spans plus read/batch counters','performance_comparison_valid':False};result.write_text(json.dumps(d,indent=2)+'\n')
helper.validate_profile(profile)
