#!/usr/bin/env python3
"""Explicit, bounded preparation of public CC-BY-4.0 FLEURS test clips.

Only the beginning of each large archive is streamed; never download the full
archive or parquet corpus. No microphone, personal data, TTS or model inference.
"""
import argparse
import csv
import hashlib
import io
import json
from pathlib import Path
import re
import subprocess
import tarfile
import urllib.request
import wave

ROOT = Path(__file__).resolve().parent
REVISION = "70bb2e84b976b7e960aa89f1c648e09c59f894dd"
BASE = f"https://huggingface.co/datasets/google/fleurs/resolve/{REVISION}"
ARCHIVE_BUDGET = 10 * 1024 * 1024
FIXTURE_BUDGET = 30 * 1024 * 1024
LICENSE = "CC-BY-4.0"


def digest(data):
    return hashlib.sha256(data).hexdigest()


class PrefixReader:
    def __init__(self, response):
        self.response = response
        self.bytes_read = 0

    def read(self, size):
        remaining = ARCHIVE_BUDGET - self.bytes_read
        if remaining <= 0:
            raise ValueError("archive prefix budget exhausted; do not fetch the whole corpus")
        data = self.response.read(min(size, remaining))
        self.bytes_read += len(data)
        return data


def categories(text):
    result = []
    if re.search(r"\d", text) or re.search(r"\b(?:one|two|three|hundred|percent|twentieth)\b", text, re.I):
        result.append("numbers")
    if any(part in text for part in ["不", "没", "无"]) or re.search(r"\b(?:not|no|never|cannot|without)\b|n['’]t\b", text, re.I):
        result.append("negation")
    # A selection aid only; the manifest states that proper-name coverage still
    # requires manual listening and reference review.
    if re.search(r"[A-Z][a-zA-Z]+", text):
        result.append("proper_name_candidate")
    return result


def wav_info(path):
    with wave.open(str(path)) as audio:
        return {"sample_rate": audio.getframerate(), "channels": audio.getnchannels(), "sample_width_bytes": audio.getsampwidth(), "duration_seconds": audio.getnframes() / audio.getframerate()}


def fetch_small(url, limit):
    with urllib.request.urlopen(url, timeout=30) as response:
        data = response.read(limit + 1)
    if len(data) > limit:
        raise ValueError("metadata exceeds budget")
    return data


def prepare_language(config, prefix):
    table_url = f"{BASE}/data/{config}/test.tsv"
    table = fetch_small(table_url, 1024 * 1024)
    rows = {}
    for index, row in enumerate(csv.reader(io.StringIO(table.decode()), delimiter="\t")):
        if len(row) >= 7:
            rows[row[1]] = {"sentence_id": row[0], "filename": row[1], "reference_text": row[2], "normalized_reference_text": row[3], "source_samples": int(row[5]), "speaker_gender_label": row[6], "tsv_row_index": index}
    archive_url = f"{BASE}/data/{config}/audio/test.tar.gz"
    request = urllib.request.Request(archive_url, headers={"Range": f"bytes=0-{ARCHIVE_BUDGET-1}"})
    selected = []
    sentence_ids = set()
    with urllib.request.urlopen(request, timeout=45) as response:
        stream = PrefixReader(response)
        with tarfile.open(mode="r|gz", fileobj=stream) as archive:
            for member in archive:
                name = Path(member.name).name
                row = rows.get(name)
                if not member.isfile() or row is None or row["sentence_id"] in sentence_ids:
                    continue
                if member.size > 2 * 1024 * 1024 or row["source_samples"] > 20 * 16000:
                    continue
                audio = archive.extractfile(member).read(member.size + 1)
                if len(audio) != member.size:
                    raise ValueError("incomplete source WAV")
                identifier = f"{prefix}{len(selected)+1:02d}"
                original = ROOT / "sources" / f"{identifier}-{name}"
                original.parent.mkdir(parents=True, exist_ok=True)
                original.write_bytes(audio)
                output = ROOT / f"{identifier}.wav"
                argv = ["ffmpeg", "-hide_banner", "-loglevel", "error", "-y", "-i", str(original), "-ar", "16000", "-ac", "1", "-c:a", "pcm_s16le", str(output)]
                subprocess.run(argv, check=True, timeout=20)
                selected.append({"id": identifier, "path": output.name, "kind": "public_human_recording", "languages": ["zh" if prefix == "zh" else "en"], "license": LICENSE, "license_url": "https://creativecommons.org/licenses/by/4.0/", "dataset": "google/fleurs", "revision": REVISION, "config": config, "split": "test", "source_archive": archive_url, "source_member": member.name, "source_tsv": table_url, "source_tsv_sha256": digest(table), "source_path": str(original.relative_to(ROOT)), "source_sha256": digest(audio), "sha256": digest(output.read_bytes()), "bytes": output.stat().st_size, "conversion": "ffmpeg to PCM signed 16-bit, 16 kHz, mono; no crop or content edit", "category_candidates": categories(row["reference_text"]), "manual_review": "pending", **row, **wav_info(output)})
                sentence_ids.add(row["sentence_id"])
                print(identifier, row["reference_text"], flush=True)
                if sum(path.stat().st_size for path in ROOT.rglob("*") if path.is_file()) > FIXTURE_BUDGET:
                    raise ValueError("fixture directory exceeds 30 MiB cap")
                if len(selected) == 6:
                    break
        compressed_bytes = stream.bytes_read
    if len(selected) != 6:
        raise ValueError("prefix did not contain six distinct sentences; record a gap rather than fetch the whole archive")
    return selected, {"config": config, "source_archive": archive_url, "compressed_prefix_bytes_read": compressed_bytes, "maximum_prefix_bytes": ARCHIVE_BUDGET, "full_archive_downloaded": False}


def combine(identifier, parts):
    output = ROOT / f"{identifier}.wav"
    with wave.open(str(output), "wb") as writer:
        writer.setparams((1, 2, 16000, 0, "NONE", "not compressed"))
        for index, part in enumerate(parts):
            if index:
                writer.writeframes(b"\0" * (16000 // 4 * 2))
            with wave.open(str(ROOT / part["path"])) as reader:
                writer.writeframes(reader.readframes(reader.getnframes()))
    return {"id": identifier, "path": output.name, "kind": "derived_cross_language_concatenation", "languages": ["zh", "en"], "license": LICENSE, "license_url": "https://creativecommons.org/licenses/by/4.0/", "components": [{"id": p["id"], "sha256": p["sha256"]} for p in parts], "reference_text": "\n".join(p["reference_text"] for p in parts), "edit": "Complete selected clips concatenated with 250 ms generated digital silence; different speakers; not natural code switching", "sha256": digest(output.read_bytes()), "bytes": output.stat().st_size, "manual_review": "pending", **wav_info(output)}


def main():
    ROOT.mkdir(parents=True, exist_ok=True)
    card = fetch_small(f"{BASE}/README.md", 512 * 1024)
    (ROOT / "FLEURS-DATASET-CARD.md").write_bytes(card)
    chinese, zh_transfer = prepare_language("cmn_hans_cn", "zh")
    english, en_transfer = prepare_language("en_us", "en")
    mixed = [combine("mixed01", [chinese[0], english[0]]), combine("mixed02", [english[1], chinese[1]])]
    silence = ROOT / "silence01.wav"
    with wave.open(str(silence), "wb") as writer:
        writer.setparams((1, 2, 16000, 0, "NONE", "not compressed")); writer.writeframes(b"\0" * 16000 * 2)
    silent_record = {"id": "silence01", "path": silence.name, "kind": "generated_digital_silence", "languages": [], "reference_text": "", "license": "CC0-1.0", "license_url": "https://creativecommons.org/publicdomain/zero/1.0/", "source": "Generated locally from exactly 16000 zero PCM samples; no captured audio", "sha256": digest(silence.read_bytes()), "bytes": silence.stat().st_size, "manual_review": "not_applicable_to_zero_signal", **wav_info(silence)}
    total = sum(path.stat().st_size for path in ROOT.rglob("*") if path.is_file())
    if total > FIXTURE_BUDGET:
        raise ValueError("fixture directory exceeds 30 MiB cap")
    manifest = {"schema_version": 1, "purpose": "V10 preparation only; ASR, dual LLM runs and manual usability scoring remain pending", "dataset_source": "https://huggingface.co/datasets/google/fleurs", "dataset_revision": REVISION, "dataset_card_sha256": digest(card), "attribution": "FLEURS: Few-shot Learning Evaluation of Universal Representations of Speech; Alexis Conneau and collaborators / Google Research and contributors; upstream dataset card and citation retained", "license": LICENSE, "speech_samples": 12, "derived_mixed_samples": 2, "silence_samples": 1, "total_bytes_before_manifest": total, "maximum_fixture_bytes": FIXTURE_BUDGET, "network_archives": [zh_transfer,en_transfer], "known_gaps": ["No verified spontaneous filler/disfluency recording", "No verified spoken filesystem-path recording", "Mixed fixtures join different speakers, not natural within-speaker code switching", "Names/category candidates require listening; no manual quality score exists", "No V10 pass claimed; no external real LLM provider was used"], "samples": chinese+english+mixed+[silent_record]}
    (ROOT / "manifest.json").write_text(json.dumps(manifest, ensure_ascii=False, indent=2)+"\n")
    print(json.dumps({"samples":15,"bytes":total,"archive_prefix_bytes_read":zh_transfer["compressed_prefix_bytes_read"]+en_transfer["compressed_prefix_bytes_read"]}))


def verify():
    manifest = json.loads((ROOT / "manifest.json").read_text())
    samples = manifest["samples"]
    assert len(samples) == 15
    assert len([s for s in samples if s["kind"] == "public_human_recording"]) == 12
    assert digest((ROOT / "FLEURS-DATASET-CARD.md").read_bytes()) == manifest["dataset_card_sha256"]
    for sample in samples:
        path = (ROOT / sample["path"]).resolve()
        assert path.is_relative_to(ROOT.resolve())
        assert path.stat().st_size == sample["bytes"]
        assert digest(path.read_bytes()) == sample["sha256"]
        info = wav_info(path)
        assert info["sample_rate"] == 16000 and info["channels"] == 1 and info["sample_width_bytes"] == 2
        assert info["duration_seconds"] == sample["duration_seconds"] <= 60
        if sample["kind"] == "public_human_recording":
            assert digest((ROOT / sample["source_path"]).read_bytes()) == sample["source_sha256"]
        if sample["kind"] == "generated_digital_silence":
            with wave.open(str(path)) as reader:
                assert not any(reader.readframes(reader.getnframes()))
    total = sum(p.stat().st_size for p in ROOT.rglob("*") if p.is_file())
    assert total <= FIXTURE_BUDGET
    result = {"scope": "fixture identity/format/size only; not ASR/LLM usability or V10 acceptance", "files_verified": 15, "original_sources_verified": 12, "sample_rate": 16000, "channels": 1, "pcm_bits": 16, "total_fixture_bytes": total, "maximum_fixture_bytes": FIXTURE_BUDGET, "audio_inference_performed": False, "human_scoring_performed": False}
    (ROOT / "verification.json").write_text(json.dumps(result, ensure_ascii=False, indent=2)+"\n")
    print(json.dumps(result))


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--verify-only", action="store_true", help="offline identity/format check; no network")
    args = parser.parse_args()
    verify() if args.verify_only else main()
