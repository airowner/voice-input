#!/usr/bin/env python3
"""Small real-model prefix-cache equivalence comparison, not V11 P95."""
import argparse
import datetime
import hashlib
import json
from pathlib import Path
import time

from smoke import ROOT, Worker
from grammar_compare import inputs


def digest(text):
    return hashlib.sha256(text.encode()).hexdigest()


def load(binary):
    worker = Worker("llm", binary)
    worker.send({"op": "load", "model_path": str(ROOT / "models/gguf/qwen2.5-1.5b-instruct-q4_k_m.gguf"), "threads": 4, "context_tokens": 4096, "gpu_layers": 0})
    if worker.read(60).get("event") != "ready":
        raise AssertionError("model ready missing")
    return worker


def request(worker, name, messages, max_tokens=768):
    started = time.monotonic()
    worker.send({"op": "run", "job_id": name, "messages": messages, "max_tokens": max_tokens})
    value = worker.read(60)
    if value.get("event") != "result" or value.get("job_id") != name:
        raise AssertionError("generation failed: " + str(value.get("code", value.get("event"))))
    parsed = json.loads(value["text"])
    if set(parsed) != {"text"} or not isinstance(parsed["text"], str):
        raise AssertionError("JSON contract failed")
    return {"case": name, "seconds": round(time.monotonic() - started, 6), "input_sha256": digest(json.dumps(messages, ensure_ascii=False, sort_keys=True)), "output_sha256": digest(value["text"]), "decoded_text_sha256": digest(parsed["text"]), "cache": value["cache_probe"]}


def run(binary):
    public = inputs()[0][1]
    changed_user = [dict(message) for message in public]
    changed_user[1]["content"] = '{"text":" Hello 456."}'
    changed_system = [dict(message) for message in changed_user]
    changed_system[0]["content"] = changed_system[0]["content"].replace("可以分段并整理明显的口语重复，不改变事实或加入新信息。", "只做必要的错词、断句与无语义填充纠正，尽量保留原措辞和顺序。")
    worker = load(binary)
    rows = [request(worker, "first", public), request(worker, "same_user_warm", public), request(worker, "different_user", changed_user), request(worker, "different_system", changed_system)]
    worker.send({"op": "run", "job_id": "invalid", "messages": [], "max_tokens": 32})
    if worker.read(5).get("event") != "error":
        raise AssertionError("invalid request not rejected")
    rows.append(request(worker, "after_error", changed_user))
    long = [{"role": "system", "content": "Write a JSON object with text listing all integers from one to one thousand in English."}, {"role": "user", "content": "Start now."}]
    worker.send({"op": "run", "job_id": "cancel-cache", "messages": long, "max_tokens": 1024})
    time.sleep(0.25)
    started = time.monotonic()
    worker.send({"op": "cancel", "job_id": "cancel-cache"})
    if worker.read(5).get("event") != "cancelled" or time.monotonic() - started >= 0.5:
        raise AssertionError("cancellation exceeded 500 ms")
    worker.stop(abrupt=True)
    worker = load(binary)
    rows.append(request(worker, "after_cancel_rebuild", changed_user))
    worker.stop()
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    if args.output.exists():
        parser.error("output exists; preserve previous evidence")
    report = {"scope": __doc__, "captured_at_utc": datetime.datetime.now(datetime.timezone.utc).isoformat(), "reference": "Full-clear KV and full-vocabulary grammar mask, explicit 128-token prefill blocks", "candidate": "LCP rounded down to complete 128-token KV blocks and equivalent grammar greedy fast path", "prefill_block_tokens": 128, "cache_alignment_tokens": 128, "threads": 4, "context_tokens": 4096, "cache_state": "First and resident jobs distinguished; model load excluded; OS cache may be warm; no formal benchmark", "status": "running"}
    try:
        report["reference_runs"] = run(ROOT / "build/workers-avx2/voice-llm-worker-avx2-grammar-reference")
        report["cached_runs"] = run(ROOT / "build/workers-avx2/voice-llm-worker-avx2-cache-probe")
        report["comparisons"] = []
        for old, new in zip(report["reference_runs"], report["cached_runs"]):
            equal = old["input_sha256"] == new["input_sha256"] and old["output_sha256"] == new["output_sha256"]
            report["comparisons"].append({"case": old["case"], "input_and_output_sha256_identical": equal, "full_clear_seconds": old["seconds"], "cached_seconds": new["seconds"], "retained_prompt_tokens": new["cache"]["retained_prompt_tokens"]})
            if not equal:
                raise AssertionError("cache changes output: " + old["case"])
            if old["cache"]["retained_prompt_tokens"] != 0:
                raise AssertionError("reference unexpectedly retained KV")
        cached = {row["case"]: row for row in report["cached_runs"]}
        for name in ("first", "after_error", "after_cancel_rebuild"):
            if cached[name]["cache"]["retained_prompt_tokens"] != 0:
                raise AssertionError("invalid cache retention: " + name)
        warm = cached["same_user_warm"]["cache"]
        expected = ((warm["prompt_tokens"] - 1) // 128) * 128
        if warm["retained_prompt_tokens"] != expected:
            raise AssertionError("same prompt did not retain all complete blocks below n-1")
        user = cached["different_user"]["cache"]["retained_prompt_tokens"]
        system = cached["different_system"]["cache"]["retained_prompt_tokens"]
        if not (0 <= system < user <= warm["retained_prompt_tokens"]):
            raise AssertionError("changed user/system retained more than their common complete prefix blocks")
        if any(row["cache"]["retained_prompt_tokens"] % 128 for row in report["cached_runs"]):
            raise AssertionError("a partial prefill block was retained")
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
