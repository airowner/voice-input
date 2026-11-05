#!/usr/bin/env python3
"""Package verified local Linux artifacts. Never installs or downloads anything.

--development requires development-scoped evidence and produces a dev version.
Without it, all eleven V1 acceptance records must be explicitly passed.
--prepare-only creates the audited payload and PKGBUILD without running makepkg.
--metadata-only collects cached Cargo license metadata without building binaries.
"""
import argparse
import datetime
import hashlib
import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tarfile
import tomllib


ROOT = Path(__file__).resolve().parents[1]
TRIPLE = "x86_64-unknown-linux-gnu"
ARTIFACTS = {
    "target/release/voice-input": "voice-input",
    "target/release/voice-inputctl": "voice-inputctl",
    **{f"src-tauri/binaries/{name}-{TRIPLE}": name for name in (
        "voice-asr-worker", "voice-llm-worker", "voice-asr-worker-avx2", "voice-llm-worker-avx2"
    )},
}
BASE_DEPENDS = {"glibc", "gcc-libs", "alsa-lib", "gtk3", "webkit2gtk-4.1", "libappindicator-gtk3", "wl-clipboard", "wtype", "libnotify"}
HEX = re.compile(r"[0-9a-f]{64}\Z")


def sha(path):
    digest = hashlib.sha256()
    with path.open("rb") as source:
        for block in iter(lambda: source.read(1024 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def command(argv, cwd=None):
    result = subprocess.run(argv, cwd=cwd, env={**os.environ, "LC_ALL": "C"}, capture_output=True, text=True, check=False)
    if result.returncode:
        raise ValueError(f"command failed: {argv[0]} ({result.returncode}): {result.stderr.strip()[:200]}")
    return result.stdout


def local_file(root, relative):
    if not isinstance(relative, str) or not relative or Path(relative).is_absolute() or ".." in Path(relative).parts:
        raise ValueError("input paths must be repository-relative")
    path = root / relative
    if path.is_symlink() or not path.is_file() or not path.resolve().is_relative_to(root.resolve()):
        raise ValueError(f"required regular file missing or outside repository: {relative}")
    return path


def evidence(root, references):
    if not isinstance(references, list) or not references:
        raise ValueError("each verified item requires nonempty evidence references")
    for reference in references:
        if not isinstance(reference, dict):
            raise ValueError("each evidence reference must be an object")
        path = local_file(root, reference.get("path"))
        expected = reference.get("sha256", "")
        if not HEX.fullmatch(expected) or path.stat().st_size == 0 or sha(path) != expected:
            raise ValueError(f"missing, empty or changed verification evidence: {reference.get('path')}")


def selected_artifacts(with_avx2):
    return {path: name for path, name in ARTIFACTS.items() if with_avx2 or not name.endswith("-avx2")}


def validate(root, record, development, with_avx2=False):
    if not isinstance(record, dict):
        raise ValueError("verification JSON must be an object")
    if record.get("schema_version") != 1:
        raise ValueError("verification schema_version must be 1")
    allowed = {"schema_version", "status", "scope", "limitations", "artifacts", "checks", "v1_acceptance", "captured_at_utc", "source_revision", "source_date_epoch"}
    if set(record) - allowed:
        raise ValueError("unknown verification fields; keep private logs/environment outside the packaged verification document")
    expected = ("development", "linux_development_binary_verification") if development else ("passed", "v1_release_acceptance")
    if (record.get("status"), record.get("scope")) != expected:
        raise ValueError("verification status/scope does not match selected development/stable mode")
    if development and (not isinstance(record.get("limitations"), list) or not record["limitations"]):
        raise ValueError("development package must disclose remaining limitations")
    if "limitations" in record and (not isinstance(record["limitations"], list)
            or any(not isinstance(value, str) or not value.strip() for value in record["limitations"])):
        raise ValueError("limitations must be nonempty explanatory strings")
    artifacts = record.get("artifacts", {})
    if not isinstance(artifacts, dict) or set(artifacts) != set(selected_artifacts(with_avx2)):
        raise ValueError("verification must cover exactly the selected release programs/workers (--with-avx2 selects six files; default four)")
    for relative, item in artifacts.items():
        if not isinstance(item, dict):
            raise ValueError("each artifact verification must be an object")
        path = local_file(root, relative)
        if item.get("verified") is not True or not HEX.fullmatch(item.get("sha256", "")) or sha(path) != item["sha256"]:
            raise ValueError(f"artifact unverified or changed after verification: {relative}")
        with path.open("rb") as binary:
            header = binary.read(20)
        if (len(header) < 20 or header[:6] != b"\x7fELF\x02\x01" or int.from_bytes(header[18:20], "little") != 62
                or int.from_bytes(header[16:18], "little") not in (2, 3) or not os.access(path, os.X_OK)):
            raise ValueError(f"not an executable x86-64 Linux ELF: {relative}")
        evidence(root, item.get("evidence"))
    checks = record.get("checks")
    if not isinstance(checks, list) or not checks:
        raise ValueError("verification requires actual checks")
    for item in checks:
        if not isinstance(item, dict):
            raise ValueError("each check must be an object")
        if item.get("status") != "passed" or not item.get("name"):
            raise ValueError("verification contains an unchecked or failed gate")
        evidence(root, item.get("evidence"))
    if not development:
        acceptance = record.get("v1_acceptance", {})
        if not isinstance(acceptance, dict) or set(acceptance) != {f"V{i:02}" for i in range(1, 12)}:
            raise ValueError("stable package requires all V01–V11 acceptance entries")
        for name, item in acceptance.items():
            if not isinstance(item, dict) or item.get("status") != "passed":
                raise ValueError(f"stable acceptance not passed: {name}")
            evidence(root, item.get("evidence"))


def rust_metadata(root, cargo_home=None):
    """Read locked manifests from verified Cargo cache; never invoke Cargo/network."""
    locked = tomllib.loads(local_file(root, "Cargo.lock").read_text())["package"]
    workspace = tomllib.loads(local_file(root, "Cargo.toml").read_text())["workspace"]
    defaults = workspace.get("package", {})
    local = {}
    for member in workspace["members"]:
        for directory in root.glob(member):
            manifest = directory / "Cargo.toml"
            if manifest.is_file():
                package = tomllib.loads(manifest.read_text()).get("package", {})
                local[package.get("name")] = (manifest, package)
    cache = Path(cargo_home or os.environ.get("CARGO_HOME", Path.home() / ".cargo")) / "registry/src"
    packages = []
    for item in locked:
        row = {key: item[key] for key in ("name", "version", "source", "checksum") if key in item}
        row.update({"license": None, "repository": None, "metadata_status": "missing_cached_source"})
        found = None
        archived_license = None
        if not item.get("source") and item["name"] in local:
            manifest, package = local[item["name"]]
            found = (manifest, package, "workspace")
        elif item.get("source", "").startswith("registry+"):
            for manifest in sorted(cache.glob(f"*/{item['name']}-{item['version']}/Cargo.toml")):
                archived_license = None
                checksum = manifest.parent / ".cargo-checksum.json"
                archive = cache.parent / "cache" / manifest.parent.parent.name / (manifest.parent.name + ".crate")
                if archive.is_file():
                    if sha(archive) != item.get("checksum"):
                        continue
                    with tarfile.open(archive, "r:gz") as bundle:
                        member = bundle.extractfile(manifest.parent.name + "/Cargo.toml")
                        if member is None:
                            continue
                        original = member.read(1024 * 1024 + 1)
                        if len(original) > 1024 * 1024 or original != manifest.read_bytes():
                            continue
                        package = tomllib.loads(original.decode()).get("package", {})
                        license_name = package.get("license-file")
                        if license_name and not Path(license_name).is_absolute() and ".." not in Path(license_name).parts:
                            try:
                                license_stream = bundle.extractfile(manifest.parent.name + "/" + license_name)
                                if license_stream:
                                    license_bytes = license_stream.read(2 * 1024 * 1024 + 1)
                                    if len(license_bytes) <= 2 * 1024 * 1024:
                                        archived_license = {"path": license_name, "sha256": hashlib.sha256(license_bytes).hexdigest(), "origin": "verified_crate_archive"}
                            except KeyError:
                                pass
                elif checksum.is_file():
                    recorded = json.loads(checksum.read_text())
                    if recorded.get("package") != item.get("checksum") or recorded.get("files", {}).get("Cargo.toml") != sha(manifest):
                        continue
                else:
                    continue
                package = tomllib.loads(manifest.read_text()).get("package", {})
                if package.get("name") == item["name"] and package.get("version") == item["version"]:
                    found = (manifest, package, "cargo_registry_cache")
                    break
        if found:
            manifest, package, origin = found
            license_expression = package.get("license")
            if isinstance(license_expression, dict) and license_expression.get("workspace"):
                license_expression = defaults.get("license")
            row.update({"license": license_expression, "repository": package.get("repository"), "manifest_sha256": sha(manifest), "metadata_origin": origin, "metadata_status": "available"})
            license_file = package.get("license-file")
            if license_file:
                path = manifest.parent / license_file
                if archived_license:
                    row["license_file"] = archived_license
                elif path.is_file() and path.resolve().is_relative_to(manifest.parent.resolve()):
                    row["license_file"] = {"path": license_file, "sha256": sha(path)}
                else:
                    row["metadata_status"] = "license_file_missing"
            if not license_expression and "license_file" not in row:
                row["metadata_status"] = "license_unspecified"
        packages.append(row)
    missing = sum(row["metadata_status"] != "available" for row in packages)
    return {"scope": "All Cargo.lock packages, including target-specific/unlinked packages; not an exact linked SBOM or legal review", "cargo_lock_sha256": sha(root / "Cargo.lock"), "package_count": len(packages), "incomplete_count": missing, "metadata_complete": missing == 0, "legal_review_status": "not_performed", "packages": packages}


def libraries(root, artifacts):
    dependencies = set(BASE_DEPENDS)
    observed = {}
    for relative in artifacts:
        path = root / relative
        dynamic = command(["readelf", "-d", str(path)])
        required = re.findall(r"Shared library: \[([^]]+)\]", dynamic)
        linked = command(["ldd", str(path)])
        if "not found" in linked:
            raise ValueError(f"unresolved runtime library: {relative}")
        mapping = dict(re.findall(r"^\s*(\S+)\s+=>\s+(/\S+)\s+\(", linked, re.M))
        # ldd prints the loader either as an absolute alias => absolute path,
        # or as a standalone absolute path. Resolve only paths actually observed.
        mapping.update({Path(name).name: resolved for name, resolved in list(mapping.items()) if name.startswith("/")})
        mapping.update({Path(resolved).name: resolved for resolved in re.findall(r"^\s*(/\S+)\s+\(", linked, re.M)})
        owners = {}
        for name in required:
            if name not in mapping:
                raise ValueError(f"cannot resolve direct library {name} for {relative}")
            package = command(["pacman", "-Qqo", mapping[name]]).strip()
            if not re.fullmatch(r"[a-z0-9@._+-]+", package):
                raise ValueError("unexpected package provider")
            owners[name] = package
            dependencies.add(package)
        observed[relative] = {"direct_libraries": owners, "ldd": linked.strip()}
    command(["pacman", "-T", *sorted(dependencies)])
    versions = command(["pkg-config", "--modversion", "webkit2gtk-4.1", "alsa", "gtk+-3.0", "appindicator3-0.1"]).splitlines()
    return sorted(dependencies), {"artifacts": observed, "pkg_config_versions": dict(zip(("webkit2gtk-4.1", "alsa", "gtk+-3.0", "appindicator3-0.1"), versions))}


def write_json(path, data):
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(json.dumps(data, ensure_ascii=False, indent=2) + "\n")
    path.chmod(0o644)


def copy(source, destination, mode=0o644):
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(source, destination)
    destination.chmod(mode)


def source_snapshot(root):
    paths = ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "package.json", "package-lock.json", "engines.lock.json", "models.lock.json", "LICENSE", "src-tauri/Cargo.toml", "src-tauri/tauri.conf.json", "src-tauri/build.rs", "workers/CMakeLists.txt"]
    files = {name: sha(local_file(root, name)) for name in paths}
    for directory in ("crates", "src-tauri/src", "src-tauri/capabilities", "workers", "ui", "scripts", "packaging/arch"):
        for path in sorted((root / directory).rglob("*")):
            if path.is_file() and not path.is_symlink() and path.suffix in {".rs", ".cpp", ".hpp", ".toml", ".json", ".js", ".html", ".css", ".py", ".in", ".desktop", ".example"}:
                files[str(path.relative_to(root))] = sha(path)
    revision = command(["git", "-C", str(root), "rev-parse", "HEAD"]).strip()
    return {"scope": "Source snapshot observed during packaging, not a claim of bit-reproducibility", "git_revision": revision, "files_sha256": files}


def prepare(root, record, validation_path, output, development, with_avx2=False):
    validate(root, record, development, with_avx2)
    validation_bytes = validation_path.read_bytes()
    if json.loads(validation_bytes) != record:
        raise ValueError("verification document changed while being checked")
    artifacts = selected_artifacts(with_avx2)
    workspace = tomllib.loads(local_file(root, "Cargo.toml").read_text())["workspace"]["package"]
    version = workspace["version"]
    tauri = json.loads(local_file(root, "src-tauri/tauri.conf.json").read_text())
    if not re.fullmatch(r"[0-9]+(?:\.[0-9]+){2}", version) or tauri["version"] != version:
        raise ValueError("Cargo/Tauri versions must match and be numeric X.Y.Z")
    now = datetime.datetime.now(datetime.timezone.utc)
    pkgver = version + ("dev" + now.strftime("%Y%m%d") if development else "")
    dependencies, library_report = libraries(root, artifacts)
    rust = rust_metadata(root)
    if not development and not rust["metadata_complete"]:
        raise ValueError("stable package requires complete locked dependency license metadata")
    snapshot = source_snapshot(root)
    engines = json.loads(local_file(root, "engines.lock.json").read_text())
    for name, entry in engines["engines"].items():
        source = root / entry["source"]
        if command(["git", "-C", str(source), "rev-parse", "HEAD"]).strip() != entry["commit"]:
            raise ValueError(f"engine source differs from locked commit: {name}")
        if command(["git", "-C", str(source), "status", "--porcelain", "--untracked-files=no"]).strip():
            raise ValueError(f"engine has tracked changes: {name}")
    required = ["LICENSE", "specs/v1.md", "engines.lock.json", "models.lock.json", "Cargo.lock", "docs/INSTALL.md", "docs/RELEASE.md", "src-tauri/icons/128x128.png", "vendor/whisper.cpp/LICENSE", "vendor/llama.cpp/LICENSE", "vendor/llama.cpp/licenses/LICENSE-jsonhpp"]
    for name in required:
        local_file(root, name)
    models = json.loads(local_file(root, "models.lock.json").read_text())["models"]
    for entry in models:
        local_file(root, entry["license_evidence"])
    output.mkdir(parents=True, exist_ok=False)
    payload = output / "payload"
    docs = payload / "usr/share/doc/voice-input"
    licenses = payload / "usr/share/licenses/voice-input"
    for relative, installed in artifacts.items():
        destination = payload / "usr/bin" / installed
        copy(root / relative, destination, 0o755)
        if sha(destination) != record["artifacts"][relative]["sha256"]:
            raise ValueError(f"artifact changed while copying: {relative}")
    for relative, destination in {
        "LICENSE": licenses / "LICENSE",
        "vendor/whisper.cpp/LICENSE": licenses / "whisper.cpp-LICENSE",
        "vendor/llama.cpp/LICENSE": licenses / "llama.cpp-LICENSE",
        "vendor/llama.cpp/licenses/LICENSE-jsonhpp": licenses / "nlohmann-json-LICENSE",
        "src-tauri/icons/128x128.png": payload / "usr/share/icons/hicolor/128x128/apps/voice-input.png",
        "packaging/arch/voice-input-bindings.lua.example": docs / "voice-input-bindings.lua.example",
        "specs/v1.md": docs / "V1-SPECIFICATION.md",
        "engines.lock.json": docs / "engines.lock.json",
        "models.lock.json": docs / "models.lock.json",
        "Cargo.lock": docs / "Cargo.lock",
        "docs/INSTALL.md": docs / "INSTALL.md",
        "docs/RELEASE.md": docs / "RELEASE.md",
    }.items():
        copy(local_file(root, relative), destination)
    desktop = local_file(root, "packaging/arch/voice-input.desktop").read_text()
    if development:
        desktop = desktop.replace("Name=Voice Input", "Name=Voice Input (Development)").replace("Name[zh_CN]=声笺", "Name[zh_CN]=声笺（开发验证版）")
    target = payload / "usr/share/applications/voice-input.desktop"
    target.parent.mkdir(parents=True, exist_ok=True)
    target.write_text(desktop)
    target.chmod(0o644)
    license_map = {}
    for entry in models:
        if not re.fullmatch(r"[a-zA-Z0-9._-]+", entry["id"]):
            raise ValueError("invalid model catalog ID")
        filename = entry["id"] + ".LICENSE"
        copy(root / entry["license_evidence"], licenses / "models" / filename)
        license_map[entry["id"]] = {"license": entry["license"], "installed_license": "/usr/share/licenses/voice-input/models/" + filename, "source": entry["source"], "sha256": entry["sha256"]}
    write_json(docs / "model-license-sources.json", license_map)
    write_json(docs / "rust-dependency-licenses.json", rust)
    write_json(docs / "source-snapshot.json", snapshot)
    write_json(docs / "validation.json", record)
    manifest = {"schema_version": 1, "captured_at_utc": now.isoformat(), "package": "voice-input", "package_version": pkgver, "application_version": version, "development": development, "verification_scope": record["scope"], "verification_input_sha256": hashlib.sha256(validation_bytes).hexdigest(), "v1_release_accepted": not development, "limitations": record.get("limitations", []), "models_bundled": False, "avx2_workers_bundled": with_avx2, "binary_sources": artifacts, "third_party_legal_review": "not_performed; metadata and upstream license copies only", "rust_metadata_complete": rust["metadata_complete"], "depends": dependencies, "runtime_library_observations": library_report, "files": {}}
    for path in payload.rglob("*"):
        if path.is_dir():
            path.chmod(0o755)
    for path in sorted(payload.rglob("*")):
        if path.is_file():
            manifest["files"][str(path.relative_to(payload))] = {"sha256": sha(path), "bytes": path.stat().st_size, "mode": oct(path.stat().st_mode & 0o777)}
    manifest["self_hash_note"] = "This manifest excludes itself and makepkg-generated ALPM metadata; archive SHA-256 is recorded externally after building"
    write_json(docs / "release-manifest.json", manifest)
    write_json(output / "release-manifest.json", manifest)
    archive = output / "payload.tar"
    with tarfile.open(archive, "w") as bundle:
        for path in sorted(payload.rglob("*")):
            info = bundle.gettarinfo(str(path), str(path.relative_to(payload)))
            info.uid = info.gid = 0
            info.uname = info.gname = "root"
            info.mtime = int(now.timestamp())
            if path.is_file():
                with path.open("rb") as source:
                    bundle.addfile(info, source)
            else:
                bundle.addfile(info)
    template = local_file(root, "packaging/arch/PKGBUILD.in").read_text()
    substitutions = {"@PKGVER@": shlex.quote(pkgver), "@PKGDESC@": shlex.quote("Local voice input (development verification; V1 acceptance pending)" if development else "Local voice input and text cleanup"), "@DEPENDS@": " ".join(shlex.quote(name) for name in dependencies), "@PAYLOAD_SHA256@": sha(archive)}
    for key, value in substitutions.items():
        template = template.replace(key, value)
    (output / "PKGBUILD").write_text(template)
    return manifest


def build(output, manifest):
    log = output / "makepkg.log"
    with log.open("x") as stream:
        result = subprocess.run(["makepkg", "--cleanbuild", "--noconfirm"], cwd=output, stdout=stream, stderr=subprocess.STDOUT, check=False)
    if result.returncode:
        raise ValueError(f"makepkg failed ({result.returncode}); inspect {log}")
    packages = [Path(line.strip()) for line in command(["makepkg", "--packagelist"], cwd=output).splitlines() if line.strip()]
    if len(packages) != 1:
        raise ValueError("makepkg did not produce exactly one package archive")
    package = packages[0]
    if not package.is_absolute():
        package = output / package
    if not package.is_file():
        raise ValueError("makepkg-reported archive is missing")
    if package.parent.resolve() != output.resolve():
        target = output / package.name
        if target.exists():
            raise ValueError("refusing to overwrite an existing archive copy")
        shutil.copy2(package, target)
        package = target
    entries = command(["bsdtar", "-tf", str(package)]).splitlines()
    listing = {entry.removeprefix("./"): entry for entry in entries}
    for installed in manifest["binary_sources"].values():
        relative = "usr/bin/" + installed
        if relative not in listing:
            raise ValueError(f"package entry missing: {relative}")
        digest = hashlib.sha256()
        process = subprocess.Popen(["bsdtar", "-xOf", str(package), listing[relative]], stdout=subprocess.PIPE)
        for block in iter(lambda: process.stdout.read(1024 * 1024), b""):
            digest.update(block)
        process.stdout.close()
        if process.wait() or digest.hexdigest() != manifest["files"][relative]["sha256"]:
            raise ValueError(f"packaged artifact bytes changed: {relative}")
    if any(name.endswith((".gguf", ".bin", ".sqlite3")) for name in listing):
        raise ValueError("unexpected model weights or user database in package")
    record = {"scope": "Actual archive build and contents verification; not installation/uninstallation or V1 acceptance", "archive": package.name, "sha256": sha(package), "bytes": package.stat().st_size, "development": manifest["development"], "package_version": manifest["package_version"], "binary_payload_hashes_match": True, "installed": False, "uninstall_tested": False}
    write_json(output / "archive-manifest.json", record)
    return record


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--validation", type=Path)
    parser.add_argument("--development", action="store_true")
    parser.add_argument("--with-avx2", action="store_true", help="include additionally verified optimized workers; default portable only")
    parser.add_argument("--prepare-only", action="store_true")
    parser.add_argument("--metadata-only", action="store_true")
    parser.add_argument("--output-dir", type=Path)
    args = parser.parse_args()
    stamp = datetime.datetime.now(datetime.timezone.utc).strftime("%Y%m%dT%H%M%SZ")
    output = (args.output_dir or ROOT / "dist/arch" / stamp).resolve()
    try:
        if output.exists():
            raise ValueError("output directory exists; choose a new path to preserve evidence")
        if args.metadata_only:
            output.mkdir(parents=True, exist_ok=False)
            metadata = rust_metadata(ROOT)
            write_json(output / "rust-dependency-licenses.json", metadata)
            print(json.dumps({"scope": metadata["scope"], "packages": metadata["package_count"], "incomplete": metadata["incomplete_count"], "output": str(output)}))
            return 0
        if not args.validation:
            raise ValueError("--validation is required for package preparation/building")
        validation_path = args.validation.resolve()
        if not validation_path.is_file():
            raise ValueError("verification JSON is missing")
        record = json.loads(validation_path.read_text())
        manifest = prepare(ROOT, record, validation_path, output, args.development, args.with_avx2)
        if args.prepare_only:
            print(json.dumps({"scope": "Payload preparation only; no package built or installed", "output": str(output), "development": args.development}))
        else:
            result = build(output, manifest)
            print(json.dumps({**result, "output": str(output)}))
        return 0
    except (ValueError, OSError, KeyError, TypeError, subprocess.SubprocessError) as error:
        parser.exit(2, f"Packaging stopped: {error}\n")


if __name__ == "__main__":
    raise SystemExit(main())
