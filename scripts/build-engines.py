#!/usr/bin/env python3
"""Build pinned M0 CPU libraries and CLI tools. Does not build app sidecars.

Missing source checkouts are fetched from the locked official repositories.
Existing checkouts must match the locked commit and have no tracked changes.
All builds run sequentially, at most two compiler jobs per build.
"""

import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]


def run(argv, log, timeout=1800):
    log.write("\n$ " + " ".join(str(value) for value in argv) + "\n")
    log.flush()
    started = time.monotonic()
    process = subprocess.Popen(argv, cwd=ROOT, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    try:
        returncode = process.wait(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            os.killpg(process.pid, signal.SIGKILL)
            process.wait()
        raise
    if returncode:
        raise RuntimeError(f"command failed ({returncode}): {argv[0]}")
    return round(time.monotonic() - started, 3)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--engine", choices=("whisper.cpp", "llama.cpp"))
    parser.add_argument("--profile", choices=("portable", "avx2"), default="portable")
    parser.add_argument("--jobs", type=int, choices=(1, 2), default=2)
    parser.add_argument("--evidence", type=Path, required=True, help="new JSON evidence path")
    args = parser.parse_args()
    evidence = args.evidence.resolve()
    log_path = evidence.with_suffix(".log")
    if evidence.exists() or log_path.exists():
        parser.error("evidence/log exists; choose a new filename")
    evidence.parent.mkdir(parents=True, exist_ok=True)
    lock = json.loads((ROOT / "engines.lock.json").read_text())
    profile = lock["profiles"][args.profile]
    if profile["required_cpu_flags"]:
        flags = set()
        for line in Path("/proc/cpuinfo").read_text().splitlines():
            if line.startswith("flags"):
                flags.update(line.split(":", 1)[1].split())
        if not set(profile["required_cpu_flags"]).issubset(flags):
            parser.error("CPU lacks required AVX2/FMA/F16C/BMI2 features")
    report = {
        "started_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "scope": lock["scope"],
        "profile": args.profile,
        "required_cpu_flags": profile["required_cpu_flags"],
        "engines": {},
        "jobs": args.jobs,
        "path": os.environ.get("PATH"),
        "compiler": {},
    }
    for tool in ("cc", "c++", "cmake", "ninja"):
        report["compiler"][tool] = subprocess.check_output([tool, "--version"], text=True).splitlines()[0]
    with log_path.open("x") as log:
        try:
            for name, engine in lock["engines"].items():
                if args.engine and args.engine != name:
                    continue
                source = ROOT / engine["source"]
                build = ROOT / (engine["build"] + profile["build_suffix"])
                if not source.exists():
                    source.parent.mkdir(parents=True, exist_ok=True)
                    run(["git", "init", str(source)], log, 10)
                    run(["git", "-C", str(source), "remote", "add", "origin", engine["repository"]], log, 10)
                    run(["git", "-C", str(source), "fetch", "--depth", "1", "origin", engine["commit"]], log, 180)
                    run(["git", "-C", str(source), "checkout", "--detach", "FETCH_HEAD"], log, 10)
                actual = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
                dirty = subprocess.check_output(["git", "-C", str(source), "status", "--porcelain", "--untracked-files=no"], text=True).strip()
                if actual != engine["commit"] or dirty:
                    raise RuntimeError(f"{name}: source commit mismatch or tracked local changes; refusing to overwrite")
                license_bytes = (source / "LICENSE").read_bytes()
                license_copy = evidence.parent / f"engines-{name}-LICENSE.txt"
                if license_copy.exists() and license_copy.read_bytes() != license_bytes:
                    raise RuntimeError(f"{name}: existing license evidence differs")
                license_copy.write_bytes(license_bytes)
                command = ["cmake", "-S", str(source), "-B", str(build), *lock["cmake_common"], *engine["cmake"], *profile["cmake_overrides"], *profile.get("engine_overrides", {}).get(name, [])]
                entry = report["engines"][name] = {
                    "commit": actual,
                    "license": engine["license"],
                    "license_sha256": hashlib.sha256(license_bytes).hexdigest(),
                    "configure_command": command,
                    "status": "building",
                }
                print(f"Configuring {name} at {actual}", flush=True)
                entry["configure_seconds"] = run(command, log)
                targets = engine["targets"] if profile["build_cli"] else [engine["targets"][0]]
                command = ["cmake", "--build", str(build), "--parallel", str(args.jobs), "--target", *targets]
                entry["build_command"] = command
                print(f"Building {name} (jobs={args.jobs})", flush=True)
                entry["build_seconds"] = run(command, log)
                if profile["build_cli"]:
                    cli = build / engine["cli"]
                    entry["smoke_command"] = [str(cli), *engine["smoke_args"]]
                    entry["smoke_seconds"] = run(entry["smoke_command"], log, 15)
                    entry["cli_sha256"] = hashlib.sha256(cli.read_bytes()).hexdigest()
                else:
                    entry["smoke_scope"] = "Core library build only; actual optimized worker loading is tested separately"
                entry["artifacts"] = [str(path.relative_to(ROOT)) for path in build.rglob("*.a")]
                entry["artifact_sha256"] = {str(path.relative_to(ROOT)): hashlib.sha256(path.read_bytes()).hexdigest() for path in build.rglob("*.a")}
                entry["status"] = "passed"
                print(f"PASS: {name} CPU library ({args.profile})" + (" and CLI smoke" if profile["build_cli"] else ""), flush=True)
            report["status"] = "passed"
        except (OSError, RuntimeError, subprocess.SubprocessError) as error:
            report["status"] = "failed"
            report["error"] = str(error)
        finally:
            report["finished_at_utc"] = datetime.datetime.now(datetime.timezone.utc).isoformat()
            with evidence.open("x") as output:
                output.write(json.dumps(report, ensure_ascii=False, indent=2) + "\n")
    print(f"Evidence: {evidence}", flush=True)
    return 0 if report["status"] == "passed" else 1


if __name__ == "__main__":
    sys.exit(main())
