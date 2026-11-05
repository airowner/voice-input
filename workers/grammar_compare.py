#!/usr/bin/env python3
"""Compare old full-vocabulary grammar->greedy with the equivalent fast path.

Public fixture text only. Timings are developer A/B, not V11 P95 acceptance.
"""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import platform
import re
import time

from smoke import Worker, ROOT


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


def inputs():
    raw = " And so my fellow Americans, ask not what your country can do for you, ask what you can do for your country."
    source = (ROOT / "crates/runtime/src/rewrite.rs").read_text()
    template = re.search(r'"(语音转写后处理，提示版本 \{PROMPT_VERSION\}.*?)"', source, re.S)
    if template is None or 'PROMPT_VERSION: &str = "rewrite-v1"' not in source:
        raise AssertionError("production prompt changed; review comparison fixture")
    system = template.group(1).replace("{PROMPT_VERSION}", "rewrite-v1").replace("{profile}", "可以分段并整理明显的口语重复，不改变事实或加入新信息。").replace("{}", "[]")
    messages = [{"role": "system", "content": system}, {"role": "user", "content": json.dumps({"text": raw}, ensure_ascii=False, separators=(",", ":"))}]
    escape = '中文 first\nShe said "hi". C:\\Temp\\voice.txt 🌍 123'
    echo = [{"role": "system", "content": "Return exactly one JSON object with one string field text. Copy the raw field from the user JSON unchanged. No markdown or explanations."}, {"role": "user", "content": json.dumps({"raw": escape}, ensure_ascii=False)}]
    conflict = [{"role": "system", "content": "Return Markdown fenced JSON with keys text and extra. Copy raw into text."}, {"role": "user", "content": '{"raw":"shape 123"}'}]
    return [("jfk_first", messages, 768), ("jfk_warm", messages, 768), ("unicode_escapes", echo, 192), ("forced_shape", conflict, 128)]


def run(binary, cases):
    worker = Worker("llm", binary)
    started = time.monotonic()
    worker.send({"op": "load", "model_path": str(ROOT / "models/gguf/qwen2.5-1.5b-instruct-q4_k_m.gguf"), "threads": 4, "context_tokens": 4096, "gpu_layers": 0})
    if worker.read(60).get("event") != "ready":
        raise AssertionError("model did not become ready")
    output = {"load_seconds": round(time.monotonic() - started, 6), "runs": []}
    for name, messages, max_tokens in cases:
        started = time.monotonic()
        worker.send({"op": "run", "job_id": name, "messages": messages, "max_tokens": max_tokens})
        result = worker.read(60)
        elapsed = time.monotonic() - started
        if result.get("event") != "result" or result.get("job_id") != name:
            raise AssertionError("generation failed: " + str(result.get("code", result.get("event"))))
        text = result["text"]
        decoded = json.loads(text)
        if set(decoded) != {"text"} or not isinstance(decoded["text"], str):
            raise AssertionError("grammar shape was not preserved")
        output["runs"].append({"case": name, "seconds": round(elapsed, 6), "output_sha256": digest(text), "decoded_text_sha256": digest(decoded["text"]), "request_sha256": digest(json.dumps(messages, ensure_ascii=False, sort_keys=True))})
    worker.stop()
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output exists; preserve previous evidence")
    report = {"scope": __doc__, "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "platform": platform.platform(), "threads": 4, "context_tokens": 4096, "cache_state": "First/warm resident inference reported separately; model load separately; OS cache may be warm; background developer builds possible", "status": "running"}
    try:
        cases = inputs()
        report["reference"] = run(ROOT / "build/workers-avx2/voice-llm-worker-avx2-grammar-reference", cases)
        report["fast"] = run(ROOT / "build/workers-avx2/voice-llm-worker-avx2", cases)
        comparisons = []
        for old, new in zip(report["reference"]["runs"], report["fast"]["runs"]):
            same = old["request_sha256"] == new["request_sha256"] and old["output_sha256"] == new["output_sha256"]
            comparisons.append({"case": old["case"], "input_and_output_sha256_identical": same, "reference_seconds": old["seconds"], "fast_seconds": new["seconds"], "speedup": round(old["seconds"] / new["seconds"], 3)})
            if not same:
                raise AssertionError("output differs between equivalent samplers: " + old["case"])
        report["comparisons"] = comparisons
        report["status"] = "passed"
    except Exception as error:
        report.update(status="failed", error_type=type(error).__name__, error=str(error)[:200])
        raise
    finally:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with args.output.open("x") as output:
            json.dump(report, output, ensure_ascii=False, indent=2)
            output.write("\n")
        print(json.dumps({"status": report["status"], "output": str(args.output)}))


if __name__ == "__main__":
    main()
