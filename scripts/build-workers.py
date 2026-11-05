#!/usr/bin/env python3
"""Build the two Linux CPU workers against the locked M0 static archives."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import datetime

root = Path(__file__).resolve().parents[1]
parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument("--jobs", type=int, choices=(1, 2), default=1)
parser.add_argument("--profile", choices=("portable", "avx2"), default="portable")
parser.add_argument("--grammar-reference", action="store_true", help="also build full-clear/full-mask reference and cache probe for development A/B; neither is packaged")
args = parser.parse_args()
lock = json.loads((root / "engines.lock.json").read_text())
profile = lock["profiles"][args.profile]
suffix = profile["build_suffix"]
if profile["required_cpu_flags"]:
    flag_sets = []
    for line in Path("/proc/cpuinfo").read_text().splitlines():
        if line.startswith("flags"):
            flag_sets.append(set(line.split(":", 1)[1].split()))
    flags = set.intersection(*flag_sets) if flag_sets else set()
    if not set(profile["required_cpu_flags"]).issubset(flags):
        raise SystemExit("CPU lacks required AVX2/FMA/F16C/BMI2 features")
if any(not (root / (entry["build"] + suffix) / "src" / ("libwhisper.a" if name == "whisper.cpp" else "libllama.a")).is_file()
       or not (root / entry["source"] / ".git").exists() for name, entry in lock["engines"].items()):
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%S%fZ")
    subprocess.run(["python3", str(root / "scripts/build-engines.py"), "--profile", args.profile, "--jobs", str(args.jobs), "--evidence", str(root / f"docs/evidence/engines-auto-{args.profile}-{stamp}.json")], check=True)
for name, engine in lock["engines"].items():
    source = root / engine["source"]
    commit = subprocess.check_output(["git", "-C", str(source), "rev-parse", "HEAD"], text=True).strip()
    if commit != engine["commit"]:
        raise SystemExit(f"{name}: source commit differs from engines.lock.json")
    dirty = subprocess.check_output(["git", "-C", str(source), "status", "--porcelain", "--untracked-files=no"], text=True).strip()
    if dirty:
        raise SystemExit(f"{name}: tracked source changes must be resolved before packaging")
    archive = root / (engine["build"] + suffix) / "src" / ("libwhisper.a" if name == "whisper.cpp" else "libllama.a")
    if not archive.is_file():
        raise SystemExit("CPU archives missing; run scripts/build-engines.py first")
triple = subprocess.check_output(["rustc", "-vV"], text=True)
triple = next(line.split(": ", 1)[1] for line in triple.splitlines() if line.startswith("host: "))
if triple != "x86_64-unknown-linux-gnu":
    raise SystemExit("Only the measured x86_64-unknown-linux-gnu worker packaging is currently supported")
build = root / ("build/workers" + suffix)
subprocess.run(["cmake", "-S", str(root / "workers"), "-B", str(build), "-G", "Ninja", "-DCMAKE_BUILD_TYPE=Release", f"-DVOICE_CPU_PROFILE={args.profile}", f"-DVOICE_BUILD_GRAMMAR_REFERENCE={'ON' if args.grammar_reference else 'OFF'}"], check=True)
subprocess.run(["cmake", "--build", str(build), "--parallel", str(args.jobs)], check=True)
destination = root / "src-tauri/binaries"
destination.mkdir(parents=True, exist_ok=True)
for name in ("voice-asr-worker", "voice-llm-worker"):
    name += suffix
    with tempfile.NamedTemporaryFile(dir=destination, prefix=f".{name}-", delete=False) as temporary:
        temporary_path = Path(temporary.name)
    try:
        shutil.copy2(build / name, temporary_path)
        temporary_path.replace(destination / f"{name}-{triple}")
    finally:
        temporary_path.unlink(missing_ok=True)
print(f"Built CPU workers and copied package sidecars for {triple}")
