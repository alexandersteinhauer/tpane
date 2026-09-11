#!/usr/bin/env python3
"""OPTIONAL source-instrumented profiling; never use these runs as benchmarks.

Example: scripts/profile-instrumented.py --variant sync --panes 8 --busy 4 \
    --outputprefix /tmp/tpane-sync-profile

Builds a retained, isolated source snapshot and runs benchmark-busy.py once
(5 seconds, 20 keys). Native uses the ALREADY generated .tools/perf/tokio-native
project; this script neither regenerates nor edits it. No sample, sudo, or
production-source changes. Artifacts append .project, .tpane, .build.log,
.profile.json and .profiled.json to the prefix; existing artifacts are refused.
The profile covers the entire process lifetime, not just the busy window.
Nested synchronous-handler wall spans are NOT CPU attribution or additive.
"""

import argparse
import fcntl
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys

sys.dont_write_bytecode = True
ROOT = Path(__file__).resolve().parents[1]
PROFILE_SOURCE = ROOT / "experiments/profiling/profile.rs"


def hashes(root):
    return {str(path.relative_to(root)): hashlib.sha256(path.read_bytes()).hexdigest()
            for path in sorted(root.rglob("*")) if path.is_file()}


def replace_once(project, filename, old, new, transforms):
    path = project / "src" / filename
    source = path.read_text()
    count = source.count(old)
    if count != 1:
        raise RuntimeError(f"Instrumentation mismatch in {path}: expected exactly one "
                           f"match, found {count}: {old!r}")
    path.write_text(source.replace(old, new, 1))
    transforms.append({"file": f"src/{filename}", "matched": old, "replacement": new})


def prepare_project(source, project, variant):
    """Snapshot then validate every insertion; also usable as a no-build dry run."""
    if variant not in ("sync", "native"):
        raise ValueError(f"Unknown variant: {variant}")
    for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "src/main.rs"):
        if not (source / name).is_file():
            raise RuntimeError(f"Missing {source / name}; native requires an already "
                               "generated project (scripts/benchmark-tokio-native.sh)")
    project.mkdir()  # Never reuse a partially instrumented or stale snapshot.
    for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml"):
        shutil.copy2(source / name, project / name)
    for name in ("src", "tests", ".cargo"):
        if (source / name).is_dir():
            shutil.copytree(source / name, project / name)
    if (source / "build.rs").is_file():
        shutil.copy2(source / "build.rs", project / "build.rs")
    original = hashes(project)
    for path in (project / "src").rglob("*.rs"):
        text = path.read_text()
        if path.name == "profile.rs" or "mod profile;" in text or "crate::profile::" in text:
            raise RuntimeError(f"Snapshot already contains profiling instrumentation: {path}")

    transforms = []

    def insert(filename, signature, statements):
        replace_once(project, filename, signature,
                     signature + "\n" + statements, transforms)

    def span(filename, signature, kind, extra=""):
        insert(filename, signature,
               f"        let _profile_span = crate::profile::Span::new(crate::profile::Kind::{kind});"
               + extra)

    replace_once(project, "main.rs", "mod app;\n", "mod app;\nmod profile;\n", transforms)
    insert("main.rs", "fn main() -> ExitCode {",
           "    let _profile_report = crate::profile::Report::new();")
    span("engine.rs", "    pub fn feed(&mut self, bytes: &[u8]) -> Result<()> {", "Feed",
         "\n        crate::profile::feed_bytes(bytes.len());")
    span("render.rs", """    pub fn draw_at(
        &mut self,
        terminal: &Terminal<'static, '_>,
        output: &mut impl Write,
        top: u16,
        force: bool,
    ) -> Result<()> {""", "BuildFrame")
    span("app.rs", "    pub fn draw(&mut self, output: &mut impl Write) -> Result<bool> {",
         "Render")
    if variant == "sync":
        span("app.rs", "    pub fn tick(&mut self) -> Result<(Option<u8>, bool)> {", "Dispatch")
        insert("pane.rs", "    pub fn poll(&mut self, read_output: bool) -> Result<Progress> {",
               "        crate::profile::pane_visit();")
    else:
        span("app.rs", "    pub fn data(&mut self, event: PtyEvent) -> Result<()> {", "Dispatch")
        insert("pane.rs", "    pub fn output(&mut self, bytes: &[u8]) -> Result<()> {",
               "        crate::profile::pane_visit();")
    insert("pty.rs", "    pub fn try_wait(&mut self) -> Result<Option<ExitStatus>> {",
           "        crate::profile::child_check();")
    shutil.copy2(PROFILE_SOURCE, project / "src/profile.rs")
    return {"source_project": str(source), "original_sha256": original,
            "instrumented_sha256": hashes(project), "transforms": transforms}


def run_busy(binary, profile_file, result_file, panes, busy_count):
    """Inject only into the exact app launch, not ps/sysctl or the driver env."""
    harness = ROOT / "scripts/benchmark-busy.py"
    previous_path = sys.path[:]
    previous_argv = sys.argv[:]
    original_popen = subprocess.Popen
    launches = []
    try:
        sys.path.insert(0, str(harness.parent))
        spec = importlib.util.spec_from_file_location("tpane_instrumented_busy", harness)
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)

        def profiled_popen(*positional, **kwargs):
            command = positional[0] if positional else kwargs.get("args")
            if (isinstance(command, (list, tuple)) and command
                    and os.fsdecode(command[0]) == str(binary)):
                if launches:
                    raise RuntimeError("Expected exactly one profiled app launch")
                # benchmark-busy passes env by keyword. Copy it: never mutate its
                # allowlist, os.environ, or env dictionaries used by other tools.
                env = kwargs.get("env")
                kwargs["env"] = dict(os.environ if env is None else env)
                kwargs["env"]["TPANE_PROFILE_FILE"] = str(profile_file)
                process = original_popen(*positional, **kwargs)
                launches.append(process)
                return process
            return original_popen(*positional, **kwargs)

        # busy.subprocess is the global subprocess module, also used by benchmark.
        # Save the real Popen above so the wrapper cannot recurse on itself.
        module.subprocess.Popen = profiled_popen
        sys.argv = [str(harness), "--binary", str(binary), "--panes", str(panes),
                    "--busy", str(busy_count), "--trials", "1", "--seconds", "5",
                    "--keys", "20", "--output", str(result_file)]
        module.main()
    finally:
        subprocess.Popen = original_popen
        sys.argv = previous_argv
        sys.path[:] = previous_path
    if len(launches) != 1:
        raise RuntimeError(f"Expected one profiled app launch, got {len(launches)}")
    return launches[0].returncode


def validate_profile(path):
    if not path.is_file():
        raise RuntimeError("No RAII profile report: app may have been killed/aborted, "
                           "failed to return from main, or failed to write the file")
    report = json.loads(path.read_text())
    if report.get("performance_comparison_valid") is not False:
        raise RuntimeError("Profile report missing non-comparison marker")
    for name in ("feedBytes", "paneVisits", "childChecks"):
        value = report["counts"][name]
        if type(value) is not int or value < 0:
            raise RuntimeError(f"Invalid counter: {name}")
    for name in ("Feed", "BuildFrame", "Render", "Dispatch"):
        for field in ("calls", "inclusive_wall_ns"):
            value = report["spans"][name][field]
            if type(value) is not int or value < 0 or (field == "calls" and value == 0):
                raise RuntimeError(f"Missing/invalid busy-episode span: {name}.{field}")
    if report["counts"]["feedBytes"] <= 0 or report["counts"]["paneVisits"] <= 0:
        raise RuntimeError("No feed bytes or pane visits recorded")
    # Native event-driven operation may legitimately make zero child checks.
    return report


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--variant", choices=("sync", "native"), required=True)
    parser.add_argument("--panes", type=int, default=8)
    parser.add_argument("--busy", type=int, default=4)
    parser.add_argument("--outputprefix", "--output-prefix", type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("requires macOS (tpane and the busy harness libproc counters)")
    if not 1 <= args.busy < args.panes <= 97:
        parser.error("require 1 <= busy < panes <= 97")
    prefix = args.outputprefix.expanduser().resolve()
    artifacts = {name: Path(str(prefix) + suffix) for name, suffix in (
        ("project", ".project"), ("binary", ".tpane"), ("build_log", ".build.log"),
        ("profile", ".profile.json"), ("result", ".profiled.json"))}
    for path in artifacts.values():
        if path.exists() or path.is_symlink():
            parser.error(f"refusing to overwrite existing artifact: {path}")
    prefix.parent.mkdir(parents=True, exist_ok=True)
    source = ROOT if args.variant == "sync" else ROOT / ".tools/perf/tokio-native"
    metadata = {"tool": "optional instrumented source", "variant": args.variant,
                "performance_comparison_valid": False, "status": "preparing",
                "helper_sha256": hashlib.sha256(Path(__file__).read_bytes()).hexdigest(),
                "artifacts": {k: str(v) for k, v in artifacts.items()},
                "scope": "entire app lifetime: startup, busy episode, drain, shutdown",
                "measurement": "nested inclusive synchronous-handler wall spans, NOT CPU time; do not sum",
                "limitations": "instrumentation perturbs scheduling and timings; no statistical CPU attribution; async waits and uninstrumented workers not attributed",
                "episode": {"panes": args.panes, "busy": args.busy,
                            "trials": 1, "seconds": 5, "keys": 20}}
    try:
        metadata["snapshot"] = prepare_project(source, artifacts["project"], args.variant)
        target = ROOT / "target/perf-profile"
        target.mkdir(parents=True, exist_ok=True)
        command = [str(ROOT / "scripts/cargo.sh"), "build", "--release", "--locked",
                   "--message-format=json-render-diagnostics",
                   "--bin", "tpane", "--manifest-path", str(artifacts["project"] / "Cargo.toml"),
                   "--target-dir", str(target)]
        metadata["build_command"] = command
        metadata["status"] = "building"
        print(f"Building isolated {args.variant} profile; log: {artifacts['build_log']}", file=sys.stderr)
        # Serialize this helper's use of the shared cache through the binary copy.
        # --target-dir overrides CARGO_TARGET_DIR; target/release is never touched.
        with (target / ".instrumented-build.lock").open("a") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX)
            with artifacts["build_log"].open("x") as log:
                subprocess.run(command, cwd=ROOT, check=True, stdout=log, stderr=subprocess.STDOUT)
            # Use Cargo's actual artifact, not a potentially stale default path
            # when CARGO_BUILD_TARGET / Cargo configuration selects a target triple.
            executables = []
            for line in artifacts["build_log"].read_text().splitlines():
                try:
                    message = json.loads(line)
                except ValueError:
                    continue  # Cargo progress / rendered diagnostics on stderr.
                if (isinstance(message, dict) and message.get("reason") == "compiler-artifact"
                        and message.get("target", {}).get("name") == "tpane"
                        and message.get("executable")):
                    executables.append(Path(message["executable"]).resolve())
            if len(executables) != 1 or target.resolve() not in executables[0].parents:
                raise RuntimeError(f"Expected one tpane build artifact inside {target}: {executables}")
            shutil.copy2(executables[0], artifacts["binary"])
        metadata["binary_sha256"] = hashlib.sha256(artifacts["binary"].read_bytes()).hexdigest()
        metadata["status"] = "running"
        metadata["app_returncode"] = run_busy(artifacts["binary"], artifacts["profile"],
                                              artifacts["result"], args.panes, args.busy)
        validate_profile(artifacts["profile"])
        metadata["status"] = "complete"
    except BaseException as exc:
        metadata.update(status="failed", error=f"{type(exc).__name__}: {exc}")
        raise
    finally:
        # Even harness failures/build failures produce an explicitly non-benchmark
        # result. Retain the busy harness diagnostics, but never endorse its numbers.
        result = json.loads(artifacts["result"].read_text()) if artifacts["result"].exists() else {}
        result.update(profiled=True, performance_comparison_valid=False, profile=metadata)
        if metadata["status"] == "failed":
            result["status"] = "failed"
        artifacts["result"].write_text(json.dumps(result, indent=2, allow_nan=False) + "\n")
    print(f"Wall-span profile: {artifacts['profile']}\n"
          f"Profiled diagnostics (NOT benchmark comparisons): {artifacts['result']}")


if __name__ == "__main__":
    main()
