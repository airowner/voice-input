#!/usr/bin/env python3
"""Isolated ALPM bookkeeping/file test; never host dependency/GUI acceptance.

Runs in a new user and network namespace with an owned temporary root, database,
config, hooks, cache, GPG and log. No sudo, sync repositories or scriptlets.
--fixture-selftest creates a clearly labelled small fixture, not a product build.
"""
import argparse
import datetime
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile


BINS = ("voice-input", "voice-inputctl", "voice-asr-worker", "voice-llm-worker", "voice-asr-worker-avx2", "voice-llm-worker-avx2")


def sha(path):
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(block)
    return h.hexdigest()


def run(argv, cwd=None):
    result = subprocess.run(argv, cwd=cwd, env={**os.environ, "LC_ALL": "C"}, capture_output=True, text=True, timeout=120)
    require(len(result.stdout.encode()) <= 8 * 1024 * 1024, "command stdout exceeded explicit 8 MiB limit")
    require(len(result.stderr.encode()) <= 1024 * 1024, "command stderr exceeded explicit 1 MiB limit")
    return {"argv": [str(x) for x in argv], "returncode": result.returncode, "stdout": result.stdout, "stderr": result.stderr}


def require(ok, reason):
    if not ok:
        raise AssertionError(reason)


def host_paths():
    result = {}
    for name in BINS:
        path = Path("/usr/bin") / name
        result[str(path)] = {"exists": path.exists(), "sha256": sha(path) if path.is_file() else None}
    return result


def fixture(work):
    release = {"schema_version": 1, "package": "voice-input-alpm-fixture", "package_version": "0.0.0", "development": True, "fixture_only": True, "files": {}}
    data = {}
    for name in BINS:
        relative = "usr/bin/" + name
        content = ("OWNED STRUCTURAL FIXTURE ONLY: " + name + "\n").encode()
        data[relative] = content
        release["files"][relative] = {"sha256": hashlib.sha256(content).hexdigest(), "bytes": len(content), "mode": "0o755"}
    data["usr/share/doc/voice-input/release-manifest.json"] = (json.dumps(release, indent=2) + "\n").encode()
    info = "pkgname = voice-input-alpm-fixture\npkgbase = voice-input-alpm-fixture\npkgver = 0.0.0-1\npkgdesc = OWNED STRUCTURAL FIXTURE ONLY\nbuilddate = 1\npackager = Local fixture, not product\nsize = 4096\narch = x86_64\nlicense = MIT\ndepend = deliberately-missing-fixture-dependency\n"
    data[".PKGINFO"] = info.encode()
    package = work / "owned-fixture-0.0.0-1-x86_64.pkg.tar.gz"
    with tarfile.open(package, "w:gz") as archive:
        for name, content in data.items():
            entry = tarfile.TarInfo(name)
            entry.size = len(content)
            entry.mode = 0o755 if name.startswith("usr/bin/") else 0o644
            entry.uid = entry.gid = 0
            archive.addfile(entry, io.BytesIO(content))
    release_path = work / "fixture-release.json"
    release_path.write_text(json.dumps(release))
    archive_path = work / "fixture-archive.json"
    archive_path.write_text(json.dumps({"archive": package.name, "sha256": sha(package), "bytes": package.stat().st_size, "fixture_only": True}))
    return package, archive_path, release_path


def inspect(package, archive_manifest, release_manifest, fixture_only):
    external = json.loads(archive_manifest.read_text())
    release = json.loads(release_manifest.read_text())
    require(package.is_file() and not package.is_symlink(), "package must be a regular local file")
    require(external["archive"] == package.name and external["sha256"] == sha(package), "archive SHA/name mismatch")
    require(external["bytes"] == package.stat().st_size, "archive size mismatch")
    require(bool(release.get("fixture_only", False)) == fixture_only, "fixture/product mode mismatch")
    require(release.get("development") is True, "this verifier is scoped to development packages")
    listing = run(["bsdtar", "-tf", str(package)])
    require(listing["returncode"] == 0, "cannot inspect package")
    names = [line.removeprefix("./") for line in listing["stdout"].splitlines()]
    require(all(not Path(name).is_absolute() and ".." not in Path(name).parts for name in names), "unsafe archive path")
    require(not any(name.endswith(".hook") or "libalpm/hooks" in name or name == ".INSTALL" for name in names), "package hook/scriptlet unexpectedly present")
    embedded = run(["bsdtar", "-xOf", str(package), "usr/share/doc/voice-input/release-manifest.json"])
    require(embedded["returncode"] == 0 and json.loads(embedded["stdout"]) == release, "embedded release manifest differs")
    info = run(["bsdtar", "-xOf", str(package), ".PKGINFO"])
    require(info["returncode"] == 0, "PKGINFO missing")
    pkginfo = dict(line.split(" = ", 1) for line in info["stdout"].splitlines() if line.startswith(("pkgname = ", "pkgver = ", "arch = ")))
    require(pkginfo["pkgname"] == release["package"] and pkginfo["arch"] == "x86_64", "package identity differs from manifest")
    require(pkginfo["pkgver"] == release["package_version"] + "-1", "package version differs from release manifest")
    expected = {}
    for name in BINS:
        relative = "usr/bin/" + name
        require(relative in names and relative in release["files"], "six-file payload missing")
        item = release["files"][relative]
        # Hash each archive member without loading worker binaries into memory.
        with tempfile.TemporaryFile() as extracted:
            extraction = subprocess.run(["bsdtar", "-xOf", str(package), relative], stdout=extracted, stderr=subprocess.PIPE, timeout=120)
            require(extraction.returncode == 0, "archive binary extraction failed")
            require(extracted.tell() == item["bytes"], "archive binary size differs from release manifest")
            extracted.seek(0)
            digest = hashlib.file_digest(extracted, "sha256").hexdigest()
            require(digest == item["sha256"], "archive binary SHA differs from release manifest")
        expected[relative] = item
    return release, pkginfo, expected


def inside(args):
    work = args.work.resolve()
    require(os.geteuid() == 0, "isolated mapped root required")
    require(os.readlink("/proc/self/ns/user") != args.parent_userns, "user namespace was not isolated")
    require(os.readlink("/proc/self/ns/net") != args.parent_netns, "network namespace was not isolated")
    uid_map = Path("/proc/self/uid_map").read_text().strip()
    mapping = [int(value) for value in uid_map.split()]
    require(mapping == [0, args.outer_uid, 1], "unexpected UID mapping")
    require(work.is_dir() and work != Path("/") and work.stat().st_uid == 0, "workspace must be owned in mapped namespace")
    root = work / "root"
    for name in ("root", "db", "cache", "hooks", "gpg", "log"):
        (work / name).mkdir(mode=0o700)
    (root / "usr/share/libalpm/hooks").mkdir(parents=True)
    config = work / "pacman.conf"
    config.write_text("[options]\nArchitecture = x86_64\nSigLevel = Never\nLocalFileSigLevel = Never\n")
    base = [shutil.which("pacman"), "--config", str(config), "--root", str(root), "--dbpath", str(work / "db"), "--cachedir", str(work / "cache"), "--hookdir", str(work / "hooks"), "--gpgdir", str(work / "gpg"), "--logfile", str(work / "log/pacman.log"), "--noconfirm"]
    release, info, expected = inspect(args.package, args.archive_manifest, args.release_manifest, args.fixture_selftest)
    results = []
    def pacman(extra, expected_return=0):
        trace = work / "log" / f"exec-{len(results)}.strace"
        result = run([shutil.which("strace"), "-qq", "-f", "-e", "trace=execve", "-o", str(trace), *base, *extra])
        executed = re.findall(r'execve\("([^"\n]+)"', trace.read_text())
        require(executed and set(executed) == {base[0]}, "unexpected child/scriptlet/hook execution")
        result["executed_programs"] = executed
        results.append(result)
        require(result["returncode"] == expected_return, "isolated pacman command failed: " + json.dumps(result))
        return result
    empty = pacman(["-Qq"], expected_return=1)
    require(not empty["stdout"].strip(), "temporary package database was not empty")
    # Pacman documents one --nodeps as skipping only versions; twice skips ALL
    # dependency checks, deliberately excluding dependency acceptance here.
    pacman(["-U", "--nodeps", "--nodeps", "--noscriptlet", str(args.package)])
    query = pacman(["-Q", info["pkgname"]])
    require(query["stdout"].strip() == info["pkgname"] + " " + info["pkgver"], "package database query differs")
    hashes = {}
    for relative, item in expected.items():
        path = root / relative
        require(path.is_file() and not path.is_symlink(), "installed file is missing or a symlink")
        require(sha(path) == item["sha256"] and path.stat().st_size == item["bytes"], "installed payload SHA/size mismatch")
        require((path.stat().st_mode & 0o777) == int(item["mode"], 8), "installed payload mode mismatch")
        hashes[relative] = sha(path)
    installed_manifest = root / "usr/share/doc/voice-input/release-manifest.json"
    require(json.loads(installed_manifest.read_text()) == release, "installed release manifest mismatch")
    owned = pacman(["-Qlq", info["pkgname"]])["stdout"].splitlines()
    pacman(["-R", "--nodeps", "--nodeps", "--noscriptlet", "--nosave", info["pkgname"]])
    pacman(["-Qq", info["pkgname"]], expected_return=1)
    require(not pacman(["-Qq"], expected_return=1)["stdout"].strip(), "database still has package entries")
    require(not any((root / name).exists() for name in expected), "installed program remains after removal")
    require(not installed_manifest.exists(), "release manifest remains after removal")
    require(not any(path.is_file() or path.is_symlink() for path in root.rglob("*")), "package-owned files remain in temporary root")
    require(not list((work / "db/local").glob(info["pkgname"] + "-*")), "ALPM database record remains")
    require(not list((work / "hooks").iterdir()) and not list((root / "usr/share/libalpm/hooks").glob("*")), "hook directories changed")
    report = {"status": "passed", "scope": "Owned fixture ALPM test only; NOT product installation" if args.fixture_selftest else "Actual development archive isolated ALPM bookkeeping/files only; NOT host dependency/GUI/V1 acceptance", "fixture_only": args.fixture_selftest, "uid_map": uid_map, "user_namespace": os.readlink("/proc/self/ns/user"), "network_namespace": os.readlink("/proc/self/ns/net"), "package": info, "archive_sha256": sha(args.package), "dependency_checks": "DISABLED explicitly by --nodeps twice", "scriptlets": "disabled by --noscriptlet", "hooks": "empty isolated hook directories; strace confirms pacman is the only executed program", "commands": results, "six_archive_file_sha256": {key: value["sha256"] for key, value in expected.items()}, "six_installed_file_sha256": hashes, "queried_owned_entries": len(owned), "removed_programs_and_manifest": True, "temporary_database_empty_after_removal": True, "host_dependency_validation": False, "host_gui_validation": False}
    args.inner_result.write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--package", type=Path)
    parser.add_argument("--archive-manifest", type=Path)
    parser.add_argument("--release-manifest", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--fixture-selftest", action="store_true")
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--work", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--inner-result", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--outer-uid", type=int, help=argparse.SUPPRESS)
    parser.add_argument("--parent-userns", help=argparse.SUPPRESS)
    parser.add_argument("--parent-netns", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.inside:
        inside(args)
        return
    if not args.output or args.output.exists():
        parser.error("a new --output JSON path is required; refusing overwrite")
    for tool in ("unshare", "pacman", "bsdtar", "strace"):
        require(shutil.which(tool), "required tool missing: " + tool)
    before = host_paths()
    report = {"captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "fixture_only": args.fixture_selftest, "status": "running", "verifier_sha256": sha(Path(__file__).resolve()), "tools": {name: {"path": shutil.which(name), "sha256": sha(Path(shutil.which(name)))} for name in ("unshare", "pacman", "bsdtar", "strace")}}
    try:
        with tempfile.TemporaryDirectory(prefix="voice-alpm-isolated-") as temp:
            work = Path(temp)
            if args.fixture_selftest:
                package, archive, release = fixture(work)
            else:
                require(args.package and args.archive_manifest and args.release_manifest, "package and both manifests required")
                package, archive, release = (path.resolve() for path in (args.package, args.archive_manifest, args.release_manifest))
            inner = work / "result.json"
            argv = ["unshare", "--user", "--map-root-user", "--net", sys.executable, str(Path(__file__).resolve()), "--inside", "--work", str(work), "--inner-result", str(inner), "--outer-uid", str(os.geteuid()), "--parent-userns", os.readlink("/proc/self/ns/user"), "--parent-netns", os.readlink("/proc/self/ns/net"), "--package", str(package), "--archive-manifest", str(archive), "--release-manifest", str(release)]
            if args.fixture_selftest:
                argv.append("--fixture-selftest")
            result = run(argv)
            report["namespace_runner"] = result
            require(result["returncode"] == 0 and inner.is_file(), "namespace ALPM verification failed")
            report.update(json.loads(inner.read_text()))
            report["temporary_tree_removed_after_run"] = True
        require(before == host_paths(), "host named program paths changed")
        report["host_six_named_program_paths_unchanged"] = True
    except Exception as error:
        report.update(status="failed", error_type=type(error).__name__, error=str(error)[:300])
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with args.output.open("x") as output:
            json.dump(report, output, ensure_ascii=False, indent=2)
            output.write("\n")
        print(json.dumps({"status": report["status"], "fixture_only": args.fixture_selftest, "output": str(args.output)}))


if __name__ == "__main__":
    main()
