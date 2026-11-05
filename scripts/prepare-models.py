#!/usr/bin/env python3
"""Explicitly download the two M0 model baselines and verify pinned SHA-256.

Development preparation only. No model activation, recording, inference or keys.
"""

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import urllib.parse
import urllib.request


ROOT = Path(__file__).resolve().parents[1]
MODELS = json.loads((ROOT / "models.lock.json").read_text())["models"]


class HttpsRedirects(urllib.request.HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        parsed = urllib.parse.urlsplit(newurl)
        host = parsed.hostname or ""
        if parsed.scheme != "https" or not (
            host == "huggingface.co" or host.endswith(".huggingface.co")
            or host == "hf.co" or host.endswith(".hf.co")
        ):
            raise ValueError("download redirect outside approved HTTPS model hosts")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def checksum(path):
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def verify(path, model):
    if path.stat().st_size != model["bytes"]:
        raise ValueError(f'{model["id"]}: file length mismatch')
    if checksum(path) != model["sha256"]:
        raise ValueError(f'{model["id"]}: SHA-256 mismatch')
    with path.open("rb") as stream:
        magic = stream.read(4)
    expected = b"GGUF" if model["engine"] == "llama.cpp" else b"lmgg"
    if magic != expected:
        raise ValueError(f'{model["id"]}: model magic mismatch')


def prepare(model, verify_only=False):
    destination = ROOT / model["path"]
    if destination.exists():
        verify(destination, model)
        return "verified_existing"
    if verify_only:
        raise FileNotFoundError(model["path"])
    destination.parent.mkdir(parents=True, exist_ok=True)
    if shutil.disk_usage(destination.parent).free < model["bytes"] + 100 * 1024 * 1024:
        raise OSError("insufficient free space for model and safety margin")
    partial = destination.with_suffix(destination.suffix + ".part")
    # Only this preparation tool's unfinished file is removed; never an active model.
    partial.unlink(missing_ok=True)
    url = f'https://huggingface.co/{model["repository"]}/resolve/{model["revision"]}/{model["filename"]}'
    opener = urllib.request.build_opener(urllib.request.ProxyHandler({}), HttpsRedirects())
    try:
        with opener.open(url, timeout=60) as response, partial.open("xb") as output:
            count = 0
            while chunk := response.read(1024 * 1024):
                count += len(chunk)
                if count > model["bytes"]:
                    raise ValueError("model exceeds pinned byte limit")
                output.write(chunk)
            output.flush()
            os.fsync(output.fileno())
        verify(partial, model)
        os.replace(partial, destination)
        return "downloaded_verified"
    finally:
        partial.unlink(missing_ok=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-only", action="store_true")
    parser.add_argument("--only", choices=[m["id"] for m in MODELS])
    args = parser.parse_args()
    for model in MODELS:
        if args.only and args.only != model["id"]:
            continue
        print(json.dumps({"model": model["id"], "status": prepare(model, args.verify_only)}, ensure_ascii=False), flush=True)


if __name__ == "__main__":
    main()
