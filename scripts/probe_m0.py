#!/usr/bin/env python3
"""Read-only M0 preflight. No recording, clipboard writes, key injection or reload.

Exit 0 means preflight passed, NOT that M0 or V01–V11 passed.
Exit 2 means a prerequisite is blocked. --network queries public Git refs only.
"""

import argparse
import datetime
import json
import os
from pathlib import Path
import platform
import shutil
import socket
import subprocess
import sys


def run(argv, timeout=3, stdout_limit=8192):
    try:
        result = subprocess.run(
            argv, capture_output=True, text=True, timeout=timeout, check=False
        )
        if len(result.stdout.encode("utf-8")) > stdout_limit:
            return {"argv": argv, "returncode": result.returncode, "error": "stdout_limit_exceeded"}
        return {
            "argv": argv,
            "returncode": result.returncode,
            "stdout": result.stdout.strip(),
            "stderr": result.stderr[:4096].strip(),
        }
    except FileNotFoundError:
        return {"argv": argv, "error": "executable_missing"}
    except subprocess.TimeoutExpired:
        return {"argv": argv, "error": "timeout"}
    except OSError as error:
        return {"argv": argv, "error": str(error)}


def unix_socket(path):
    if not path:
        return {"status": "blocked", "reason": "session_variable_missing"}
    try:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as connection:
            connection.settimeout(0.5)
            connection.connect(str(path))
        return {"status": "pass", "path": str(path)}
    except OSError as error:
        return {
            "status": "blocked",
            "path": str(path),
            "errno": error.errno,
            "reason": str(error),
        }


def active_window():
    result = run(["hyprctl", "-j", "activewindow"], stdout_limit=1048576)
    if result.get("returncode") == 0:
        try:
            window = json.loads(result.pop("stdout", ""))
            # Do not persist window titles, class names, paths or command lines.
            result["target_identifiable"] = bool(
                window.get("address") and window.get("pid")
            )
        except (ValueError, AttributeError, TypeError):
            result["error"] = "invalid_window_response"
            result.pop("stdout", None)
    else:
        result.pop("stdout", None)
    return result


def bindings():
    result = run(["hyprctl", "-j", "binds"], stdout_limit=1048576)
    if result.get("returncode") == 0:
        try:
            entries = json.loads(result.pop("stdout", ""))
            if not isinstance(entries, list):
                raise ValueError("not a bindings list")
            result["binding_count"] = len(entries)
            result["super_shift_d_conflicts"] = [
                {key: entry.get(key) for key in ("key", "modmask", "release")}
                for entry in entries
                if str(entry.get("key", "")).lower() == "d"
                and entry.get("modmask") == 65
            ]
        except (ValueError, AttributeError, TypeError):
            result["error"] = "invalid_bindings_response"
            result.pop("stdout", None)
    else:
        result.pop("stdout", None)
    return result


def probe(network=False):
    runtime = os.environ.get("XDG_RUNTIME_DIR")
    signature = os.environ.get("HYPRLAND_INSTANCE_SIGNATURE")
    display = os.environ.get("WAYLAND_DISPLAY")
    runtime_path = Path(runtime) if runtime else None
    hypr = runtime_path / "hypr" / signature if runtime and signature else None
    wayland = None
    if display:
        wayland = Path(display)
        if not wayland.is_absolute():
            wayland = runtime_path / display if runtime else None
    tools = (
        "hyprctl", "wl-copy", "wl-paste", "wtype", "cmake", "clang",
        "node", "npm", "cargo", "rustc", "ninja", "pkg-config", "wpctl",
        "git", "lua", "luac",
    )
    data = {
        "schema_version": 1,
        "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "scope": "M0 preflight only; not desktop or product acceptance",
        "side_effects": "read-only; no recording/clipboard writes/keys/config changes",
        "host": {
            "system": platform.system(), "machine": platform.machine(),
            "kernel": platform.release(),
            "os_release": Path("/etc/os-release").read_text(),
            "logical_cpus": os.cpu_count(),
            "memory": run(["free", "-b"]),
            "cpu": run(["lscpu"]),
            "disk_free_bytes": shutil.disk_usage(Path(__file__).resolve().parent).free,
            "sound_device_directory_present": Path("/dev/snd").exists(),
            "gpu_render_nodes": [str(p) for p in Path("/dev/dri").glob("renderD*")],
        },
        "session": {
            "type": os.environ.get("XDG_SESSION_TYPE"),
            "runtime_directory_present": bool(runtime_path and runtime_path.is_dir()),
            "wayland_display_set": bool(display),
            "hyprland_signature_set": bool(signature),
        },
        "tools": {name: shutil.which(name) for name in tools},
        "versions": {},
        "sockets": {
            "wayland": unix_socket(wayland),
            "hyprland": unix_socket(hypr / ".socket.sock" if hypr else None),
            "hyprland_events": unix_socket(hypr / ".socket2.sock" if hypr else None),
            "pipewire": unix_socket(runtime_path / "pipewire-0" if runtime else None),
            "session_bus": unix_socket(runtime_path / "bus" if runtime else None),
        },
    }
    for tool in ("node", "npm", "cargo", "rustc", "cmake", "clang", "ninja", "git", "lua"):
        data["versions"][tool] = run([tool, "-v" if tool == "lua" else "--version"])
    data["packages"] = run([
        "pacman", "-Q", "hyprland", "omarchy", "wl-clipboard", "wtype", "pipewire",
        "alsa-lib", "webkit2gtk-4.1", "base-devel", "clang", "cmake", "ninja",
    ])
    data["build_libraries"] = run([
        "pkg-config", "--modversion", "webkit2gtk-4.1", "alsa"
    ])
    data["desktop"] = {
        "hyprland_version": run(["hyprctl", "version"]),
        "config_errors": run(["hyprctl", "configerrors"]),
        "active_window": active_window(),
        "bindings": bindings(),
        "audio_devices": run(["wpctl", "status"]),
        "clipboard_mime_types": run(["wl-paste", "--list-types"]),
    }
    clipboard = data["desktop"]["clipboard_mime_types"]
    if clipboard.get("returncode") == 1 and clipboard.get("stderr") == "Nothing is copied":
        clipboard["clipboard_empty"] = True
    data["upstream_access"] = {}
    for engine in ("whisper.cpp", "llama.cpp"):
        data["upstream_access"][engine] = (
            run(["git", "ls-remote", f"https://github.com/ggml-org/{engine}.git", "HEAD"], 15)
            if network else {"status": "not_run", "reason": "use --network"}
        )
    blockers = [f"socket:{name}" for name, result in data["sockets"].items()
                if result["status"] != "pass"]
    blockers.extend(f"tool:{name}" for name, path in data["tools"].items() if path is None)
    if data["build_libraries"].get("returncode") != 0:
        blockers.append("build_libraries")
    blockers.extend(f"desktop:{name}" for name, result in data["desktop"].items()
                    if (result.get("returncode") != 0 and not result.get("clipboard_empty"))
                    or result.get("error"))
    blockers.extend(f"version:{name}" for name, result in data["versions"].items()
                    if result.get("returncode") != 0 or result.get("error"))
    if (data["desktop"]["config_errors"].get("returncode") == 0
            and data["desktop"]["config_errors"].get("stdout")):
        blockers.append("desktop:configuration_errors_present")
    if not data["desktop"]["active_window"].get("target_identifiable"):
        blockers.append("desktop:target_not_identifiable")
    if data["desktop"]["bindings"].get("super_shift_d_conflicts"):
        blockers.append("shortcut:super_shift_d_conflict")
    if network:
        blockers.extend(f"source:{name}" for name, result in data["upstream_access"].items()
                        if result.get("returncode") != 0)
    data["preflight"] = {"status": "blocked" if blockers else "pass", "blockers": blockers}
    data["m0"] = {
        "status": "not_passed",
        "remaining": [
            "real key release and Wayland/XWayland controlled paste round-trip",
            "clipboard owner lifecycle and actual microphone capture",
            "two pinned engine CPU builds and local model loading",
            "verified model hashes, licenses and dependency locks",
        ],
    }
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--network", action="store_true", help="query public upstream Git refs")
    parser.add_argument("--output", type=Path, help="new JSON evidence file; refuses overwrite")
    args = parser.parse_args()
    if args.output and args.output.exists():
        parser.error("output exists; choose a new evidence filename")
    result = probe(args.network)
    encoded = json.dumps(result, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with args.output.open("x", encoding="utf-8") as output:
            output.write(encoded)
    print(json.dumps({"preflight": result["preflight"], "m0": result["m0"]}, ensure_ascii=False, indent=2))
    return 2 if result["preflight"]["status"] == "blocked" else 0


if __name__ == "__main__":
    sys.exit(main())
