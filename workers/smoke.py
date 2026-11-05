#!/usr/bin/env python3
"""Actual worker protocol/model smoke. Not UI, recording, V07 or V11 acceptance."""
import argparse
import array
import datetime
import hashlib
import json
import os
import platform
from pathlib import Path
import select
import struct
import subprocess
import sys
import threading
import time
import wave

ROOT = Path(__file__).resolve().parents[1]
checks = []
PROFILE_SUFFIX = ""


def check(name, passed, **details):
    checks.append({"check": name, "passed": bool(passed), **details})
    if not passed:
        raise AssertionError(name)


class Worker:
    def __init__(self, kind, executable=None):
        self.kind = kind
        self.stderr_bytes = 0
        self.process = subprocess.Popen([str(executable or ROOT / f"build/workers{PROFILE_SUFFIX}/voice-{kind}-worker{PROFILE_SUFFIX}")], stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        def drain():
            while True:
                chunk = self.process.stderr.read(4096)
                if not chunk:
                    break
                self.stderr_bytes += len(chunk)
        self.drainer = threading.Thread(target=drain, daemon=True)
        self.drainer.start()
        hello = self.read(5)
        check(f"{kind}: protocol hello", hello == {"event": "hello", "protocol": 1, "worker": "asr" if kind == "asr" else "llm"})

    def exact(self, size, deadline):
        result = bytearray()
        while len(result) < size:
            timeout = deadline - time.monotonic()
            if timeout <= 0 or not select.select([self.process.stdout], [], [], timeout)[0]:
                raise TimeoutError("worker response timeout")
            chunk = os.read(self.process.stdout.fileno(), size - len(result))
            if not chunk:
                raise EOFError("worker exited before response")
            result.extend(chunk)
        return result

    def read(self, timeout=120):
        deadline = time.monotonic() + timeout
        length = struct.unpack("<I", self.exact(4, deadline))[0]
        if not 1 <= length <= 4 * 1024 * 1024:
            raise AssertionError("stdout is not valid framing")
        payload = self.exact(length, deadline)
        if payload[0] != 1:
            raise AssertionError("worker emitted a non-JSON frame")
        return json.loads(payload[1:])

    def send(self, data, binary=False):
        payload = bytes([2 if binary else 1]) + (data if binary else json.dumps(data, ensure_ascii=False).encode())
        self.process.stdin.write(struct.pack("<I", len(payload)) + payload)
        self.process.stdin.flush()

    def stop(self, abrupt=False):
        if self.process.poll() is None and not abrupt:
            self.send({"op": "shutdown"})
            check(f"{self.kind}: shutdown bye", self.read(5).get("event") == "bye")
        try:
            self.process.stdin.close()
        except BrokenPipeError:
            pass
        started = time.monotonic()
        self.process.wait(timeout=5)
        self.drainer.join(timeout=1)
        elapsed = time.monotonic() - started
        check(f"{self.kind}: process exits within five seconds", elapsed < 5, seconds=round(elapsed, 4), returncode=self.process.returncode)
        check(f"{self.kind}: stderr has no engine/user/model text", self.stderr_bytes == 0, stderr_bytes=self.stderr_bytes)
        self.process.stdout.close()
        self.process.stderr.close()


def load(worker, path, **extra):
    worker.send({"op": "load", "model_path": str(ROOT / path), "threads": 2, "gpu_layers": 0, **extra})
    response = worker.read(60)
    check(f"{worker.kind}: actual CPU model ready", response.get("event") == "ready" and response.get("backend") == "cpu")


def malformed():
    for label, raw, code in [
        ("zero frame", struct.pack("<I", 0), "invalid_frame"),
        ("oversized frame", struct.pack("<I", 4 * 1024 * 1024 + 1), "invalid_frame"),
        ("unknown frame type", struct.pack("<I", 1) + b"\x03", "unknown_frame_type"),
        ("invalid JSON", struct.pack("<I", 2) + b"\x01{", "invalid_request"),
    ]:
        worker = Worker("asr")
        worker.process.stdin.write(raw)
        worker.process.stdin.flush()
        check(label, worker.read(3).get("code") == code)
        worker.stop(abrupt=True)
        check(label + " exits with protocol error", worker.process.returncode == 2)
    worker = Worker("asr")
    worker.send({"op": "unload", "unexpected": "SECRET_TEST_MARKER"})
    check("unknown field rejected", worker.read(3).get("code") == "unknown_field")
    worker.stop(abrupt=True)
    worker = Worker("asr")
    worker.send({"op": "run"})
    check("missing job ID rejected", worker.read(3).get("code") == "invalid_job_id")
    worker.stop(abrupt=True)


def asr():
    with wave.open(str(ROOT / "tests/fixtures/jfk.wav"), "rb") as audio:
        assert audio.getnchannels() == 1 and audio.getframerate() == 16000 and audio.getsampwidth() == 2
        samples = array.array("h", audio.readframes(audio.getnframes()))
    if sys.byteorder != "little":
        samples.byteswap()
    pcm = struct.pack("<" + "f" * len(samples), *(sample / 32768 for sample in samples))
    worker = Worker("asr")
    worker.send({"op": "load", "model_path": "/nonexistent/model.bin"})
    check("asr: missing model sanitized error", worker.read(5).get("code") == "model_load_failed")
    load(worker, "models/whisper/ggml-base.bin")
    worker.send(pcm, binary=True)
    worker.send({"op": "run", "job_id": "asr-fixture", "language": "en"})
    started = time.monotonic()
    response = worker.read(180)
    text = response.get("text", "")
    expected = "And so my fellow Americans, ask not what your country can do for you, ask what you can do for your country."
    check("asr: actual JFK transcription", response.get("event") == "result" and response.get("job_id") == "asr-fixture" and text.strip() == expected, text_sha256=hashlib.sha256(text.encode()).hexdigest(), text_bytes=len(text.encode()), elapsed_seconds=round(time.monotonic() - started, 3), timing_scope="smoke only, concurrent development build; not V11")
    worker.send(pcm, binary=True)
    worker.send({"op": "run", "job_id": "asr-cancel", "language": "en"})
    time.sleep(0.25)
    started = time.monotonic()
    worker.send({"op": "cancel", "job_id": "asr-cancel"})
    response = worker.read(5)
    elapsed = time.monotonic() - started
    check("asr: cancellation while job active within 500 ms", response.get("event") == "cancelled" and response.get("job_id") == "asr-cancel" and elapsed < 0.5, acknowledgement_seconds=round(elapsed, 4))
    # The specification permits terminating uncooperative engine computations;
    # a cancelled worker is discarded and a fresh one must load successfully.
    worker.stop(abrupt=True)
    worker = Worker("asr")
    load(worker, "models/whisper/ggml-base.bin")
    worker.send({"op": "unload"})
    check("asr: unload acknowledged", worker.read(5).get("event") == "unloaded")
    load(worker, "models/whisper/ggml-base.bin")
    worker.send(pcm, binary=True)
    worker.send({"op": "run", "job_id": "asr-eof", "language": "en"})
    time.sleep(0.1)
    worker.stop(abrupt=True)
    check("asr: stdin EOF exits cleanly during job", worker.process.returncode == 0)


def llm():
    worker = Worker("llm")
    load(worker, "models/gguf/qwen2.5-1.5b-instruct-q4_k_m.gguf", context_tokens=2048)
    system = "Return exactly one JSON object with one string field text. Copy the raw field from the user JSON unchanged. No markdown or explanations."
    messages = [{"role": "system", "content": system}, {"role": "user", "content": '{"raw":"Hello 123."}'}]
    worker.send({"op": "run", "job_id": "llm-fixture", "messages": messages, "max_tokens": 64})
    started = time.monotonic()
    response = worker.read(180)
    generated = json.loads(response.get("text", "{}"))
    check("llm: actual CPU JSON generation", response.get("event") == "result" and response.get("job_id") == "llm-fixture" and generated == {"text": "Hello 123."}, elapsed_seconds=round(time.monotonic() - started, 3), timing_scope="smoke only, concurrent development build; not V11")
    for index, raw in enumerate(("第一行\n第二行", 'She said "hi". C:\\Temp\\voice.txt', "中文 café 🌍 123")):
        escaped = [{"role": "system", "content": system}, {"role": "user", "content": json.dumps({"raw": raw}, ensure_ascii=False)}]
        worker.send({"op": "run", "job_id": f"llm-json-escape-{index}", "messages": escaped, "max_tokens": 192})
        response = worker.read(60)
        generated = json.loads(response.get("text", "{}"))
        check(f"llm: grammar preserves JSON escape/Unicode case {index}", response.get("event") == "result" and generated == {"text": raw})
    conflict = [{"role": "system", "content": "Return Markdown fenced JSON with keys text and extra. Copy the raw value into text."}, {"role": "user", "content": '{"raw":"shape 123"}'}]
    worker.send({"op": "run", "job_id": "llm-forced-shape", "messages": conflict, "max_tokens": 128})
    response = worker.read(60)
    generated = json.loads(response.get("text", "{}"))
    check("llm: grammar rejects fences and extra fields despite conflicting prompt", response.get("event") == "result" and set(generated) == {"text"} and isinstance(generated["text"], str))
    oversized = [{"role": "system", "content": system}, {"role": "user", "content": "word " * 3000}]
    worker.send({"op": "run", "job_id": "llm-budget", "messages": oversized, "max_tokens": 64})
    check("llm: reject context budget before decode", worker.read(5).get("code") == "context_budget_exceeded")
    worker.send({"op": "run", "job_id": "llm-truncated", "messages": messages, "max_tokens": 1})
    check("llm: truncated generation rejected", worker.read(30).get("code") == "output_truncated")
    long_messages = [{"role": "system", "content": "Write a JSON object with text listing the integers from one to one thousand in English."}, {"role": "user", "content": "Start now."}]
    worker.send({"op": "run", "job_id": "llm-cancel", "messages": long_messages, "max_tokens": 1024})
    time.sleep(0.25)
    started = time.monotonic()
    worker.send({"op": "cancel", "job_id": "llm-cancel"})
    response = worker.read(5)
    elapsed = time.monotonic() - started
    check("llm: cancellation while job active within 500 ms", response.get("event") == "cancelled" and response.get("job_id") == "llm-cancel" and elapsed < 0.5, acknowledgement_seconds=round(elapsed, 4))
    worker.stop(abrupt=True)
    worker = Worker("llm")
    load(worker, "models/gguf/qwen2.5-1.5b-instruct-q4_k_m.gguf", context_tokens=2048)
    worker.send({"op": "run", "job_id": "llm-after-cancel", "messages": messages, "max_tokens": 64})
    check("llm: recreate worker and recover after cancellation", worker.read(180).get("event") == "result")
    worker.send({"op": "unload"})
    check("llm: unload acknowledged", worker.read(5).get("event") == "unloaded")
    worker.stop()
    worker = Worker("llm")
    load(worker, "models/gguf/qwen2.5-1.5b-instruct-q4_k_m.gguf", context_tokens=2048)
    worker.send({"op": "run", "job_id": "llm-eof", "messages": long_messages, "max_tokens": 1024})
    time.sleep(0.1)
    worker.stop(abrupt=True)
    check("llm: stdin EOF exits cleanly during job", worker.process.returncode == 0)


def main():
    global PROFILE_SUFFIX
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--profile", choices=("portable", "avx2"), default="portable")
    parser.add_argument("--protocol-only", action="store_true", help="framing checks without model loading/inference")
    args = parser.parse_args()
    PROFILE_SUFFIX = "" if args.profile == "portable" else "-avx2"
    if args.output.exists():
        parser.error("evidence exists; use a new filename")
    report = {
        "scope": "Framing only; no model or application acceptance" if args.protocol_only else __doc__,
        "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "platform": platform.platform(),
        "profile": args.profile, "checks": checks, "status": "running",
        "binary_sha256": {name: hashlib.sha256((ROOT / f"build/workers{PROFILE_SUFFIX}/{name}{PROFILE_SUFFIX}").read_bytes()).hexdigest() for name in ("voice-asr-worker", "voice-llm-worker")},
        "models_lock_sha256": hashlib.sha256((ROOT / "models.lock.json").read_bytes()).hexdigest(),
        "cache_state": "First inference follows process-cold model load; OS page cache unspecified; repeated resident jobs for cancellation checks; not V11 warm benchmark",
    }
    try:
        malformed()
        if not args.protocol_only:
            asr()
            llm()
        report["status"] = "passed"
    except Exception as error:
        report["status"] = "failed"
        report["error_type"] = type(error).__name__
        report["error"] = str(error)[:200]
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with args.output.open("x") as output:
            json.dump(report, output, ensure_ascii=False, indent=2)
            output.write("\n")
        print(json.dumps({"status": report["status"], "checks": len(checks), "output": str(args.output)}, ensure_ascii=False))


if __name__ == "__main__":
    main()
