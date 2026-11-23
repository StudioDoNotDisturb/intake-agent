"""Build and resolve a word-timed narration review queue from local evidence."""

import argparse
import ctypes
from ctypes import wintypes
from dataclasses import asdict
import hashlib
import importlib.metadata
import json
import math
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
import unicodedata
import wave

import numpy as np


TOKEN = re.compile(r"[^\W_]+(?:['’][^\W_]+)*")
MAX_REVIEW_SECONDS = 3 * 60 * 60


def surfaces(text):
    return TOKEN.findall(unicodedata.normalize("NFKC", text))


def key(word):
    return word.casefold().replace("’", "'")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def read(path):
    return json.loads(path.read_text(encoding="utf-8-sig"))


def read_hashed(path):
    data = path.read_bytes()
    return json.loads(data.decode("utf-8-sig")), hashlib.sha256(data).hexdigest()


def timed_words(report):
    words = []
    for index, segment in enumerate(report["transcript"]["segments"], start=1):
        tokens = surfaces(segment["text"])
        letters, timings = [], []
        for source in segment.get("words") or []:
            start, end = float(source["start"]), float(source["end"])
            if not (math.isfinite(start) and math.isfinite(end)):
                raise ValueError(f"Transcript segment {index} has non-finite word timings")
            end = max(start, end)
            for token in surfaces(source["word"]):
                visible = key(token).replace("'", "")
                letters.append(visible)
                timings.extend([(start, end)] * len(visible))
        expected = [key(token).replace("'", "") for token in tokens]
        if "".join(letters) != "".join(expected):
            raise ValueError(f"Transcript segment {index} has missing or inconsistent word timings")
        offset = 0
        for token, visible in zip(tokens, expected):
            final = offset + len(visible)
            words.append({"word": token, "start": timings[offset][0], "end": timings[final - 1][1]})
            offset = final
    return words


def alignment(reference, hypothesis):
    """Return the original literal WER alignment as contiguous review ranges."""
    ids = {word: index for index, word in enumerate(set(reference + hypothesis))}
    observed = np.array([ids[word] for word in hypothesis])
    columns = np.arange(len(hypothesis) + 1, dtype=np.int32)
    distances = np.empty((len(reference) + 1, len(columns)), dtype=np.int32)
    distances[0] = columns
    for row_index, word in enumerate(reference, 1):
        previous, row = distances[row_index - 1], distances[row_index]
        row[0] = row_index
        row[1:] = np.minimum(previous[1:] + 1, previous[:-1] + (observed != ids[word]))
        row[:] = np.minimum.accumulate(row - columns) + columns

    ranges = []
    left, right = len(reference), len(hypothesis)
    end = (left, right)
    edits = 0
    while left or right:
        if left and right and reference[left - 1] == hypothesis[right - 1]:
            if edits:
                ranges.append((left, end[0], right, end[1], edits))
            left, right = left - 1, right - 1
            end, edits = (left, right), 0
            continue
        distance = distances[left, right]
        if left and right and distance == distances[left - 1, right - 1] + 1:
            left, right = left - 1, right - 1
        elif left and distance == distances[left - 1, right] + 1:
            left -= 1
        else:
            right -= 1
        edits += 1
    if edits:
        ranges.append((0, end[0], 0, end[1], edits))
    return list(reversed(ranges))


def score(reference, words):
    edits = sum(span[-1] for span in alignment(list(map(key, reference)), word_keys(words)))
    return score_count(edits, len(reference))


def score_count(edits, total):
    return {"edits": edits, "reference_words": total, "wer_pct": round(100 * edits / total, 3)}


def word_keys(words):
    return [key(word["word"]) for word in words]


def suggested_words(path, receipt_path, report):
    suggestion = read(path)
    if (suggestion["receipt_sha256"] != digest(receipt_path)
            or suggestion["audio_sha256"] != report["source"]["sha256"]):
        raise ValueError("Suggestions were generated for a different receipt")
    words = timed_words(report)
    changed = set()
    for change in suggestion["proposals"]:
        index = change["index"]
        target = change["rule"]["word"]
        if index in changed or not 0 <= index < len(words) or change["before"] != words[index] or surfaces(target) != [target]:
            raise ValueError("Suggestion does not match the original timed word")
        words[index]["word"] = target
        changed.add(index)
    if words != suggestion["words"]:
        raise ValueError("Suggested words do not match the recorded proposals")
    return words, len(suggestion["proposals"])


def plan(args):
    report = read(args.receipt)
    audio_digest = digest(args.audio)
    if audio_digest != report["source"]["sha256"]:
        raise ValueError("Audio hash does not match the receipt")
    reference = surfaces(args.reference.read_text(encoding="utf-8-sig"))
    if not reference:
        raise ValueError("The reference has no words")
    words = timed_words(report)
    original = None
    if args.suggestion:
        original = score(reference, words)
        words, _ = suggested_words(args.suggestion, args.receipt, report)
    duration = report["transcript"]["samples"] / 16000
    alternate = []
    if args.alternate:
        other = read(args.alternate)
        if other["source"]["sha256"] != audio_digest or other["transcript"]["samples"] != report["transcript"]["samples"]:
            raise ValueError("Alternate evidence describes different audio")
        alternate = [word for segment in other["transcript"]["segments"] for word in (segment.get("words") or [])]

    ranges = alignment(list(map(key, reference)), word_keys(words))
    baseline = score_count(sum(span[-1] for span in ranges), len(reference))
    items = []
    for index, (start, end, first, last, edits) in enumerate(ranges, 1):
        context = words[max(0, first - 2):last + 2]
        clip_start = max(0, (context[0]["start"] if context else 0) - 1)
        clip_end = min(duration, (context[-1]["end"] if context else 0) + 1)
        if clip_end <= clip_start:
            raise ValueError("Review range has no playable audio")
        item = {
            "id": index, "edits": edits, "first": first, "last": last,
            "ref_start": start, "ref_end": end,
            "expected": reference[start:end], "recognized": words[first:last],
            "context": reference[max(0, start - 3):end + 3],
            "clip_start": clip_start, "clip_end": clip_end,
            "alternate": [
                word for word in alternate if word["start"] < clip_end and word["end"] > clip_start
            ],
        }
        if args.clips:
            clip = args.clips / f"{index:03}.wav"
            subprocess.run(
                [str(args.ffmpeg), "-nostdin", "-v", "error", "-n", "-ss", str(clip_start),
                 "-i", str(args.audio), "-t", str(clip_end - clip_start),
                 "-map", "0:a:0", "-ar", "16000", "-ac", "1", str(clip)],
                check=True,
            )
            item.update(clip=str(clip), clip_sha256=digest(clip))
        items.append(item)
    return {
        "schema": 1, "audio_sha256": audio_digest, "receipt_sha256": digest(args.receipt),
        "reference_sha256": digest(args.reference),
        "alternate_sha256": digest(args.alternate) if args.alternate else None,
        "suggestion_sha256": digest(args.suggestion) if args.suggestion else None,
        "duration": duration, "reference": reference, "words": words,
        "original": original if original is not None else baseline,
        "baseline": baseline, "items": items,
    }


def replacement(item, decision):
    recognized, expected = item["recognized"], item["expected"]
    if decision["action"] == "retain":
        return recognized
    if decision["action"] != "replace":
        raise ValueError("Expected replace or retain")
    timings = decision.get("timings")
    if timings is None and len(recognized) == len(expected):
        timings = [[word["start"], word["end"]] for word in recognized]
    if not expected:
        timings = []
    if timings is None or len(timings) != len(expected):
        raise ValueError("A changed word count requires explicit word timings")
    previous = item["clip_start"]
    result = []
    for word, (start, end) in zip(expected, timings):
        if not (math.isfinite(start) and math.isfinite(end) and previous <= start <= end <= item["clip_end"]):
            raise ValueError("Review word timings fall outside the clip or are out of order")
        result.append({"word": word, "start": start, "end": end})
        previous = start
    return result


def apply(args):
    queue, queue_hash = read_hashed(args.queue)
    review, decisions_hash = read_hashed(args.decisions)
    if review["queue_sha256"] != queue_hash:
        raise ValueError("Decisions were made for a different review queue")
    decisions = {entry["id"]: entry for entry in review["decisions"]}
    if len(decisions) != len(review["decisions"]) or decisions.keys() - {item["id"] for item in queue["items"]}:
        raise ValueError("Unknown or duplicate review decision")
    cursor = 0
    corrected, pending, resolved = [], [], []
    learned, learned_rules = {}, {}
    for item in queue["items"]:
        corrected.extend(queue["words"][cursor:item["first"]])
        cursor = item["last"]
        decision = decisions.get(item["id"])
        if decision is None:
            corrected.extend(item["recognized"])
            pending.append(item["id"])
            continue
        if not decision["evidence"].strip():
            raise ValueError("A review decision requires supporting evidence")
        corrected.extend(replacement(item, decision))
        resolved.append(decision)
        if decision.get("learn"):
            if (decision["action"] != "replace"
                    or not (1 <= len(item["recognized"]) <= 4)
                    or not (1 <= len(item["expected"]) <= 4)):
                raise ValueError("Only explicit 1-to-4 word replacements may be learned")
            rec_keys = word_keys(item["recognized"])
            exp_keys = [key(word) for word in item["expected"]]
            rule_key = " ".join(rec_keys) + " => " + " ".join(exp_keys)
            entry = learned_rules.setdefault(
                rule_key,
                {"recognized": rec_keys, "expected": list(item["expected"]), "expected_keys": exp_keys, "evidence_ids": []},
            )
            entry["evidence_ids"].append(item["id"])
            if len(rec_keys) == 1 and len(exp_keys) == 1:
                source = rec_keys[0]
                target = item["expected"][0]
                if source in learned and learned[source]["word"] != target:
                    raise ValueError(f"Conflicting learned corrections for {source}")
                learned.setdefault(source, {"word": target, "evidence_ids": []})["evidence_ids"].append(item["id"])
    corrected.extend(queue["words"][cursor:])
    if any(right["start"] < left["start"] for left, right in zip(corrected, corrected[1:])):
        raise ValueError("Corrected word timings are out of sequence")
    return {
        "queue_sha256": queue_hash, "decisions_sha256": decisions_hash,
        "audio_sha256": queue["audio_sha256"], "reference_sha256": queue["reference_sha256"],
        "original": queue.get("original", queue["baseline"]),
        "baseline": queue["baseline"], "corrected": score(queue["reference"], corrected),
        "words": corrected, "pending_ids": pending, "resolved": resolved,
        "learned": learned, "learned_rules": learned_rules,
    }


def suggest(args):
    report, prior = read(args.receipt), read(args.review)
    if report["source"]["sha256"] == prior["audio_sha256"]:
        raise ValueError("Evaluate learned corrections on different audio")
    words = timed_words(report)
    changes = []
    for index, word in enumerate(words):
        rule = prior["learned"].get(key(word["word"]))
        if rule is None:
            continue
        changes.append({"index": index, "before": word.copy(), "rule": rule})
        word["word"] = rule["word"]
    return {
        "audio_sha256": report["source"]["sha256"], "receipt_sha256": digest(args.receipt),
        "learned_review_sha256": digest(args.review), "proposals": changes, "words": words,
    }


def evaluate(args):
    report = read(args.receipt)
    words, proposal_count = suggested_words(args.suggestion, args.receipt, report)
    reference = surfaces(args.reference.read_text(encoding="utf-8-sig"))
    if not reference:
        raise ValueError("The reference has no words")
    original = score(reference, timed_words(report))
    candidate = score(reference, words)
    return {
        "receipt_sha256": digest(args.receipt), "reference_sha256": digest(args.reference),
        "suggestion_sha256": digest(args.suggestion), "proposals": proposal_count,
        "baseline": original, "suggested": candidate,
        "edit_reduction": original["edits"] - candidate["edits"],
    }


def _get_faster_model(model_path, model_cache=None):
    from whisper_worker import faster_model

    if model_path.name != "model.bin":
        raise ValueError("Select the CTranslate2 model.bin file")
    if model_cache is None:
        return faster_model(model_path, "cuda", "int8_float16")
    cache_key = str(model_path.resolve())
    model = model_cache.get(cache_key)
    if model is None:
        model = faster_model(model_path, "cuda", "int8_float16")
        model_cache[cache_key] = model
    return model


def _transcribe_narration(model_path, pcm, language, model_cache=None):
    from whisper_worker import faster_narration

    if model_cache is None:
        return faster_narration(model_path, pcm, language, "cuda", "int8_float16")
    model = _get_faster_model(model_path, model_cache)
    segments, info = model.transcribe(
        pcm, language=language, temperature=0.0, beam_size=5,
        condition_on_previous_text=False, no_speech_threshold=None,
        word_timestamps=True, vad_filter=False,
    )
    versions = {
        "faster_whisper_version": importlib.metadata.version("faster-whisper"),
        "ctranslate2_version": importlib.metadata.version("ctranslate2"),
    }
    return info.language, (asdict(segment) for segment in segments), versions


def _run_rerun(queue, queue_hash, review, review_hash, model_path, language, *, include_retained=False, model_cache=None):
    if queue_hash != review["queue_sha256"]:
        raise ValueError("The review belongs to a different queue")
    pending = set(review["pending_ids"])
    if include_retained:
        pending.update(item["id"] for item in review["resolved"] if item["action"] == "retain")
    evidence = []
    model = _get_faster_model(model_path, model_cache) if pending else None
    for item in queue["items"]:
        if item["id"] not in pending:
            continue
        clip = Path(item["clip"])
        if digest(clip) != item["clip_sha256"]:
            raise ValueError(f"Clip hash mismatch for item {item['id']}")
        with wave.open(str(clip)) as stream:
            if (stream.getnchannels(), stream.getsampwidth(), stream.getframerate()) != (1, 2, 16000):
                raise ValueError("Expected 16 kHz mono PCM review clips")
            pcm = np.frombuffer(stream.readframes(stream.getnframes()), dtype="<i2").astype(np.float32) / 32768
        segments, _ = model.transcribe(
            pcm, language=language, temperature=0, beam_size=10, word_timestamps=True,
            vad_filter=False, condition_on_previous_text=False, no_speech_threshold=None,
        )
        evidence.append({
            "id": item["id"], "clip_sha256": item["clip_sha256"],
            "chapter_offset_seconds": item["clip_start"], "segments": [asdict(segment) for segment in segments],
        })
    return {
        "queue_sha256": queue_hash, "review_sha256": review_hash,
        "model_sha256": digest(model_path), "device": "cuda", "beam_size": 10, "clips": evidence,
    }


def rerun(args):
    queue, review = read(args.queue), read(args.review)
    return _run_rerun(
        queue, digest(args.queue), review, digest(args.review),
        args.model, args.language, include_retained=getattr(args, "include_retained", False),
    )


def write_json(path, value, *, create=False):
    with tempfile.NamedTemporaryFile(dir=path.parent, delete=False, mode="w", encoding="utf-8") as stream:
        temporary = Path(stream.name)
        try:
            json.dump(value, stream, ensure_ascii=False, allow_nan=False, indent=2)
            stream.flush()
            os.fsync(stream.fileno())
            stream.close()
            if create:
                try:
                    os.link(temporary, path)
                except FileExistsError:
                    pass
            else:
                temporary.replace(path)
        finally:
            stream.close()
            temporary.unlink(missing_ok=True)


def decode(ffmpeg, audio, rate):
    raw = subprocess.check_output([
        str(ffmpeg), "-nostdin", "-v", "error", "-i", str(audio), "-map", "0:a:0",
        "-t", str(MAX_REVIEW_SECONDS + 0.1),
        "-ar", str(rate), "-ac", "1", "-f", "s16le", "-",
    ])
    if not 0 < len(raw) <= 2 * rate * MAX_REVIEW_SECONDS:
        raise ValueError("Expected audio greater than zero and at most three hours")
    return np.frombuffer(raw, dtype="<i2")


def patch(args):
    """Render a duration-preserving repair on a new local WAV."""
    rate = 24000
    if not all(math.isfinite(value) for value in (args.start, args.end, args.donor_start)):
        raise ValueError("Repair times must be finite")
    source_hash, donor_hash = digest(args.audio), digest(args.donor)
    source = decode(args.ffmpeg, args.audio, rate).copy()
    donor = decode(args.ffmpeg, args.donor, rate)
    start, end, offset = (round(value * rate) for value in (args.start, args.end, args.donor_start))
    length = end - start
    if not 0 <= start < end <= len(source) or not 0 <= offset <= len(donor) - length:
        raise ValueError("The repair and donor must cover the same in-bounds sample count")
    fade = min(round(0.015 * rate), length // 2)
    replacement_audio = donor[offset:offset + length].astype(np.float64)
    if fade:
        weights = np.linspace(0, 1, fade)
        replacement_audio[:fade] = source[start:start + fade] * (1 - weights) + replacement_audio[:fade] * weights
        replacement_audio[-fade:] = source[end - fade:end] * weights + replacement_audio[-fade:] * (1 - weights)
    source[start:end] = np.rint(replacement_audio).astype("<i2")
    if args.output.suffix.lower() != ".wav" or (source_hash, donor_hash) != (digest(args.audio), digest(args.donor)):
        raise ValueError("Expected a new WAV and unchanged repair inputs")
    with tempfile.TemporaryDirectory(dir=args.output.parent) as temporary:
        rendered = Path(temporary) / "repair.wav"
        with wave.open(str(rendered), "wb") as output:
            output.setparams((1, 2, rate, 0, "NONE", "not compressed"))
            output.writeframes(source.tobytes())
        os.link(rendered, args.output)
    return {"original_sha256": source_hash, "donor_sha256": donor_hash,
            "candidate_sha256": digest(args.output), "sample_rate": rate, "modified_samples": [start, end]}


def _load_learned_rules(paths):
    rules = {}
    for path in paths:
        resolved = path.resolve(strict=True)
        if not resolved.is_file() or resolved.is_symlink():
            raise ValueError(f"Expected a regular learned-rules file: {path}")
        data, file_hash = read_hashed(resolved)
        source_rules = data.get("rules") if "rules" in data else data.get("learned_rules")
        if source_rules is None and "learned" in data:
            source_rules = {}
            for src, entry in data["learned"].items():
                target = entry["word"]
                exp_keys = [key(target)]
                rule_key = f"{src} => {exp_keys[0]}"
                source_rules[rule_key] = {
                    "recognized": [src], "expected": [target],
                    "expected_keys": exp_keys, "evidence_ids": entry.get("evidence_ids", []),
                }
        if not isinstance(source_rules, dict):
            raise ValueError(f"Invalid learned rules file: {path}")
        for rule_key, entry in source_rules.items():
            rules[rule_key] = {**entry, "source_file": str(resolved), "source_sha256": file_hash}
    return rules


def _contains_subsequence(haystack, needle):
    if not needle or len(needle) > len(haystack):
        return False
    width = len(needle)
    return any(haystack[idx:idx + width] == needle for idx in range(len(haystack) - width + 1))


def _carried_words(item):
    expected = item["expected"]
    recognized = item["recognized"]
    if len(expected) == len(recognized):
        return [{"word": word, "start": rec["start"], "end": rec["end"]} for word, rec in zip(expected, recognized)]
    span_start = recognized[0]["start"] if recognized else item["clip_start"]
    span_end = max(span_start, recognized[-1]["end"] if recognized else item["clip_end"])
    step = (span_end - span_start) / len(expected)
    return [
        {"word": word, "start": round(span_start + idx * step, 4), "end": round(span_start + (idx + 1) * step, 4)}
        for idx, word in enumerate(expected)
    ]


def _apply_carried_evidence(queue, base_result, rerun_data, rerun_hash, learned_rules):
    if not learned_rules or not base_result["pending_ids"] or not rerun_data:
        return base_result
    clips_by_id = {entry["id"]: entry for entry in rerun_data.get("clips", [])}
    words = queue["words"]
    pending_set = set(base_result["pending_ids"])
    carried_map, suggested = {}, []
    for item in queue["items"]:
        if item["id"] not in pending_set:
            continue
        rec_keys = word_keys(item["recognized"])
        exp_keys = [key(w) for w in item["expected"]]
        if not rec_keys or not exp_keys:
            continue
        rule_key = " ".join(rec_keys) + " => " + " ".join(exp_keys)
        rule = learned_rules.get(rule_key)
        if rule is None:
            continue
        clip_entry = clips_by_id.get(item["id"])
        if clip_entry is None:
            continue
        rerun_tokens = [key(tok) for seg in clip_entry.get("segments", []) for tok in surfaces(seg["text"])]
        left_ctx = word_keys(words[max(0, item["first"] - 2):item["first"]])
        right_ctx = word_keys(words[item["last"]:min(len(words), item["last"] + 2)])
        target_seq = left_ctx + exp_keys + right_ctx
        confirmed = _contains_subsequence(rerun_tokens, target_seq) if (left_ctx or right_ctx) else (rerun_tokens == exp_keys)
        if confirmed:
            carried_map[item["id"]] = {
                "id": item["id"], "rule_key": rule_key,
                "prior_source": {k: rule[k] for k in ("source_chapter", "source_file", "source_sha256") if k in rule},
                "new_evidence": {
                    "clip_sha256": item["clip_sha256"],
                    "rerun_sha256": rerun_hash,
                    "confirmed_sequence": target_seq,
                },
            }
        else:
            suggested.append({
                "id": item["id"], "rule_key": rule_key,
                "prior_source": {k: rule[k] for k in ("source_chapter", "source_file", "source_sha256") if k in rule},
                "clip_sha256": item["clip_sha256"], "rerun_sha256": rerun_hash,
                "confirmed": False, "observed_rerun": rerun_tokens,
            })
    if not carried_map and not suggested:
        return base_result
    resolved_by_id = {entry["id"]: entry for entry in base_result["resolved"]}
    cursor, corrected, pending = 0, [], []
    for item in queue["items"]:
        corrected.extend(words[cursor:item["first"]])
        cursor = item["last"]
        if item["id"] in carried_map:
            corrected.extend(_carried_words(item))
        elif item["id"] in resolved_by_id:
            corrected.extend(replacement(item, resolved_by_id[item["id"]]))
        else:
            corrected.extend(item["recognized"])
            pending.append(item["id"])
    corrected.extend(words[cursor:])
    if any(right["start"] < left["start"] for left, right in zip(corrected, corrected[1:])):
        raise ValueError("Carried word timings are out of sequence")
    return {
        **base_result,
        "corrected": score(queue["reference"], corrected),
        "words": corrected,
        "pending_ids": pending,
        "carried": list(carried_map.values()),
        "carried_ids": list(carried_map.keys()),
        "suggested": suggested,
        "suggested_ids": [entry["id"] for entry in suggested],
    }


def prepare_round(args, folder, audio, reference, identity, *, model_cache=None):
    pcm = decode(args.ffmpeg, audio, 16000)
    language, segments, versions = _transcribe_narration(
        args.model, pcm.astype(np.float32) / 32768, args.language, model_cache=model_cache,
    )
    report = {"source": {"sha256": identity["audio_sha256"]},
              "transcript": {"samples": len(pcm), "segments": list(segments), "language": language},
              "recipe": identity, "versions": versions}
    if digest(audio) != identity["audio_sha256"]:
        raise ValueError("Audio changed during blind transcription")
    with tempfile.TemporaryDirectory(dir=folder.parent) as scratch:
        ready = Path(scratch) / "round"
        clips = ready / "clips"
        clips.mkdir(parents=True)
        receipt = ready / "blind.json"
        write_json(receipt, report)
        queue = plan(argparse.Namespace(receipt=receipt, reference=reference, audio=audio,
                                        alternate=None, suggestion=None, clips=clips, ffmpeg=args.ffmpeg))
        if digest(audio) != identity["audio_sha256"] or queue["reference_sha256"] != identity["reference_sha256"]:
            raise ValueError("Chapter inputs changed during review setup")
        for item in queue["items"]:
            item["clip"] = str(folder / "clips" / Path(item["clip"]).name)
        write_json(ready / "queue.json", queue)
        write_json(ready / "inputs.json", identity)
        # Publishing the complete directory prevents half-extracted queues from becoming resumable.
        ready.rename(folder)


def review_round(args, audio, reference, folder, identity, *, learned_rules=None, model_cache=None):
    if not folder.exists():
        prepare_round(args, folder, audio, reference, identity, model_cache=model_cache)
    queue_path = folder / "queue.json"
    receipt = folder / "blind.json"
    queue = read(queue_path)
    if (read(folder / "inputs.json") != identity or queue["receipt_sha256"] != digest(receipt)
            or read(receipt)["recipe"] != identity):
        raise ValueError("Review round has inconsistent input bindings")
    current = plan(argparse.Namespace(receipt=receipt, reference=reference, audio=audio,
                                      alternate=None, suggestion=None, clips=None, ffmpeg=None))
    if ({**queue, "items": [{k: v for k, v in item.items() if k not in {"clip", "clip_sha256"}}
                            for item in queue["items"]]} != current
            or current["reference_sha256"] != identity["reference_sha256"]):
        raise ValueError("Review queue no longer matches the blind receipt and script")
    for item in queue["items"]:
        clip = folder / "clips" / f"{item['id']:03}.wav"
        if Path(item["clip"]) != clip or digest(clip) != item["clip_sha256"]:
            raise ValueError(f"Review clip changed: {clip}")
    decisions = folder / "decisions.json"
    if not decisions.exists():
        write_json(decisions, {"queue_sha256": digest(queue_path), "decisions": []}, create=True)
    base_result = apply(argparse.Namespace(queue=queue_path, decisions=decisions))
    # A decision snapshot gets its own result and rerun, so stale evidence cannot satisfy a new round.
    suffix = base_result["decisions_sha256"]
    evidence = folder / f"rerun-{suffix}.json"
    if base_result["corrected"]["edits"] and not evidence.exists():
        base_digest = hashlib.sha256(json.dumps(base_result, sort_keys=True).encode()).hexdigest()
        write_json(evidence, _run_rerun(
            queue, base_result["queue_sha256"], base_result, base_digest,
            args.model, args.language, include_retained=True, model_cache=model_cache,
        ))
    rerun_data, rerun_hash = read_hashed(evidence) if evidence.exists() else (None, None)
    result = _apply_carried_evidence(queue, base_result, rerun_data, rerun_hash, learned_rules or {})
    if learned_rules:
        learned_digest = hashlib.sha256(json.dumps(learned_rules, sort_keys=True).encode()).hexdigest()[:12]
        review = folder / f"review-{suffix}-{learned_digest}.json"
    else:
        review = folder / f"review-{suffix}.json"
    write_json(review, result)
    if digest(decisions) != suffix:
        raise ValueError("Decisions changed during review; rerun the job")
    return result, review, evidence, queue


def stage_delivery(destination, result, sources):
    """Write the verified flat delivery set to a fresh staging folder for Rust plan/apply."""
    expected_names = {filename for filename, _, _ in sources} | {"job.json"}
    with tempfile.TemporaryDirectory(dir=destination.parent) as scratch:
        staging = Path(scratch) / "stage"
        staging.mkdir()
        write_json(staging / "job.json", result)
        job_hash = digest(staging / "job.json")
        if destination.exists():
            receipt = destination / "job.json"
            if (not destination.is_dir() or destination.is_symlink()
                    or not receipt.is_file() or receipt.is_symlink() or digest(receipt) != job_hash
                    or {entry.name for entry in destination.iterdir()} != expected_names
                    or any(not (destination / filename).is_file() or (destination / filename).is_symlink()
                           or digest(destination / filename) != expected
                           for filename, _, expected in sources)):
                raise ValueError("Existing staging path does not match this job; choose a fresh path")
            return job_hash
        for filename, path, expected in sources:
            target = staging / filename
            shutil.copyfile(path, target)
            if digest(target) != expected:
                raise ValueError(f"Staged input changed: {path}")
        staging.rename(destination)
    return job_hash


def renderer_binding(config):
    config = config.resolve(strict=True)
    if not config.is_file() or config.is_symlink():
        raise ValueError("Renderer config must be a regular local file")
    renderer = read(config)
    command = renderer.get("command") if isinstance(renderer, dict) else None
    if (not isinstance(command, list) or not command
            or any(not isinstance(part, str) or not part for part in command)
            or not Path(command[0]).is_absolute()
            or (os.name == "nt" and Path(command[0]).suffix.lower() != ".exe")
            or command[1:].count("{script}") != 1 or command[1:].count("{audio}") != 1):
        raise ValueError("Renderer config needs an absolute executable and one {script} and {audio} argument")
    inputs = renderer.get("inputs", [])
    if not isinstance(inputs, list) or any(not isinstance(path, str) or not path for path in inputs):
        raise ValueError("Renderer inputs must be a list of file paths")
    files = {Path(os.path.abspath(config.parent / path)) for path in inputs}
    files.add(Path(command[0]))
    files.update(path for path in map(Path, command[1:]) if path.is_absolute() and path.exists())
    for path in sorted(files):
        if not path.is_file() or path.is_symlink():
            raise ValueError(f"Renderer input must be a regular local file: {path}")
    return {"command": command, "files": {str(path): digest(path) for path in sorted(files)}}


def renderer_inputs_match(renderer):
    return all(
        Path(path).is_file() and not Path(path).is_symlink() and digest(Path(path)) == expected
        for path, expected in renderer["files"].items()
    )


def _run_renderer(command):
    if os.name != "nt":
        subprocess.run(command, check=True, stdin=subprocess.DEVNULL, stdout=sys.stderr)
        return

    class _IO_COUNTERS(ctypes.Structure):
        _fields_ = [("ReadOperationCount", ctypes.c_uint64), ("WriteOperationCount", ctypes.c_uint64),
                    ("OtherOperationCount", ctypes.c_uint64), ("ReadTransferCount", ctypes.c_uint64),
                    ("WriteTransferCount", ctypes.c_uint64), ("OtherTransferCount", ctypes.c_uint64)]

    class _BASIC_LIMITS(ctypes.Structure):
        _fields_ = [("PerProcessUserTimeLimit", wintypes.LARGE_INTEGER),
                    ("PerJobUserTimeLimit", wintypes.LARGE_INTEGER),
                    ("LimitFlags", wintypes.DWORD),
                    ("MinimumWorkingSetSize", ctypes.c_size_t),
                    ("MaximumWorkingSetSize", ctypes.c_size_t),
                    ("ActiveProcessLimit", wintypes.DWORD),
                    ("Affinity", ctypes.c_size_t),
                    ("PriorityClass", wintypes.DWORD),
                    ("SchedulingClass", wintypes.DWORD)]

    class _EXTENDED_LIMITS(ctypes.Structure):
        _fields_ = [("BasicLimitInformation", _BASIC_LIMITS), ("IoInfo", _IO_COUNTERS),
                    ("ProcessMemoryLimit", ctypes.c_size_t), ("JobMemoryLimit", ctypes.c_size_t),
                    ("PeakProcessMemoryUsed", ctypes.c_size_t), ("PeakJobMemoryUsed", ctypes.c_size_t)]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.CreateJobObjectW.restype = wintypes.HANDLE
    job_handle = kernel32.CreateJobObjectW(None, None)
    if not job_handle:
        raise OSError(ctypes.get_last_error(), "CreateJobObjectW failed")
    limits = _EXTENDED_LIMITS()
    limits.BasicLimitInformation.LimitFlags = 0x2000  # JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
    if not kernel32.SetInformationJobObject(job_handle, 9, ctypes.byref(limits), ctypes.sizeof(limits)):
        err = ctypes.get_last_error()
        kernel32.CloseHandle(job_handle)
        raise OSError(err, "SetInformationJobObject failed")
    proc = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=sys.stderr)
    try:
        kernel32.AssignProcessToJobObject(job_handle, wintypes.HANDLE(int(proc._handle)))
        code = proc.wait()
        if code:
            raise subprocess.CalledProcessError(code, command)
    finally:
        if proc.poll() is None:
            proc.kill()
            proc.wait()
        kernel32.CloseHandle(job_handle)


def synthesize_chapter(renderer, chapter_root, reference, reference_hash, ffmpeg):
    """Resume from a complete synthesis receipt, or render into an unpublished directory."""
    audio_name = "chapter.wav"
    command = renderer["command"]
    binding = {**renderer, "reference_sha256": reference_hash}
    if not renderer_inputs_match(renderer):
        raise ValueError("Renderer inputs changed before synthesis")
    if not chapter_root.exists():
        chapter_root.parent.mkdir(parents=True, exist_ok=True)
        with tempfile.TemporaryDirectory(dir=chapter_root.parent) as scratch:
            ready = Path(scratch) / "render"
            ready.mkdir()
            script = ready / "spoken.txt"
            shutil.copyfile(reference, script)
            audio = ready / audio_name
            substitutions = {"{script}": str(script), "{audio}": str(audio)}
            if digest(script) != reference_hash:
                raise ValueError("Approved script changed before synthesis")
            _run_renderer([substitutions.get(part, part) for part in command])
            if not audio.is_file() or audio.is_symlink():
                raise ValueError("The renderer did not produce a regular chapter WAV")
            decode(ffmpeg, audio, 16000)
            if digest(script) != reference_hash or not renderer_inputs_match(renderer):
                raise ValueError("Synthesis inputs changed during the render")
            write_json(ready / "receipt.json", {**binding, "audio_sha256": digest(audio)})
            ready.rename(chapter_root)
    audio = chapter_root / audio_name
    if read(chapter_root / "receipt.json") != {**binding, "audio_sha256": digest(audio)}:
        raise ValueError("Existing synthesis does not match the configuration, script and audio")
    return audio


def _extract_reference_span(text, start_idx, end_idx):
    normalized = unicodedata.normalize("NFKC", text)
    matches = list(TOKEN.finditer(normalized))
    if not (0 <= start_idx < end_idx <= len(matches)):
        raise ValueError("Reference token span out of range")
    end_char = matches[end_idx - 1].end()
    if end_char < len(normalized) and normalized[end_char] in ".!?":
        end_char += 1
    return normalized[matches[start_idx].start():end_char]


def _bounded_anchors(text, ref_start, ref_end, max_left, max_right):
    normalized = unicodedata.normalize("NFKC", text)
    matches = list(TOKEN.finditer(normalized))
    left_avail = 0
    for step in range(1, max_left + 1):
        gap = normalized[matches[ref_start - step].end():matches[ref_start - step + 1].start()]
        if any(ch in ".!?\n" for ch in gap):
            break
        left_avail = step
    if left_avail == 0 and max_left >= 1:
        left_avail = 1
    right_avail = 0
    for step in range(1, max_right + 1):
        right_avail = step
        if ref_end + step < len(matches):
            gap = normalized[matches[ref_end + step - 1].end():matches[ref_end + step].start()]
            if any(ch in ".!?\n" for ch in gap):
                break
    return left_avail, right_avail


def _read_pcm16(path, ffmpeg, target_rate=None, target_channels=None):
    if path.suffix.lower() == ".wav":
        with wave.open(str(path), "rb") as stream:
            channels, width, rate, frames_count, comptype, _ = stream.getparams()
            if width == 2 and comptype == "NONE" and (target_rate is None or rate == target_rate) and (target_channels is None or channels == target_channels):
                raw = stream.readframes(frames_count)
                arr = np.frombuffer(raw, dtype="<i2").reshape(-1, channels)
                return arr.copy(), rate, channels
    rate = target_rate or 24000
    mono = decode(ffmpeg, path, rate).astype("<i2")
    channels = target_channels or 1
    if channels == 1:
        return mono.reshape(-1, 1), rate, 1
    return np.repeat(mono.reshape(-1, 1), channels, axis=1), rate, channels


def _snap_quiet(samples, target_idx, lo, hi, rate):
    window = round(0.004 * rate)
    left = max(lo, target_idx - window)
    right = min(hi, target_idx + window)
    if right <= left:
        return max(lo, min(hi, target_idx))
    energy = np.sum(samples[left:right].astype(np.float64) ** 2, axis=1)
    return left + int(np.argmin(energy))


def _splice_wav_repairs(base_wav, splices, output_wav, ffmpeg):
    base, rate, channels = _read_pcm16(base_wav, ffmpeg)
    fade = round(0.012 * rate)
    for orig_start_s, orig_end_s, donor_wav, donor_start_s, donor_end_s in sorted(splices, key=lambda s: s[0], reverse=True):
        donor, _, _ = _read_pcm16(donor_wav, ffmpeg, target_rate=rate, target_channels=channels)
        o_start = _snap_quiet(base, round(orig_start_s * rate), 0, len(base), rate)
        o_end = _snap_quiet(base, round(orig_end_s * rate), o_start, len(base), rate)
        d_start = _snap_quiet(donor, round(donor_start_s * rate), 0, len(donor), rate)
        d_end = _snap_quiet(donor, round(donor_end_s * rate), d_start, len(donor), rate)
        segment = donor[d_start:d_end].astype(np.float64)
        if len(segment) > 0:
            edge = min(fade, len(segment) // 4)
            if edge > 0:
                weights = np.linspace(0.0, 1.0, edge)[:, None]
                if o_start >= edge and d_start >= edge:
                    lead_in = base[o_start - edge:o_start].astype(np.float64)
                    segment[:edge] = lead_in * (1.0 - weights) + segment[:edge] * weights
                else:
                    segment[:edge] *= weights
                if len(base) - o_end >= edge and len(donor) - d_end >= edge:
                    tail_out = base[o_end:o_end + edge].astype(np.float64)
                    segment[-edge:] = segment[-edge:] * (1.0 - weights) + tail_out * weights
                else:
                    segment[-edge:] *= (1.0 - weights)
        seg_i16 = np.clip(np.rint(segment), -32768, 32767).astype("<i2")
        base = np.concatenate([base[:o_start], seg_i16, base[o_end:]], axis=0)
    with wave.open(str(output_wav), "wb") as stream:
        stream.setparams((channels, 2, rate, 0, "NONE", "not compressed"))
        stream.writeframes(base.tobytes())


def _attempt_targeted_repair(args, renders_root, renderer, reference_text, reference_hash, base_audio, base_sha, queue, model_cache):
    items = queue["items"]
    if not (1 <= len(items) <= 4):
        return None, {"status": "skipped", "reason": "item_count_out_of_range", "items": len(items)}
    repair_dir = renders_root / f"repair-{base_sha[:12]}"
    if repair_dir.exists():
        receipt = read(repair_dir / "receipt.json")
        if (receipt.get("base_audio_sha256") != base_sha or receipt.get("renderer") != renderer
                or receipt.get("reference_sha256") != reference_hash):
            raise ValueError("Existing repair receipt does not match chapter inputs")
        if receipt["status"] == "applied":
            repaired = repair_dir / "chapter.wav"
            if not repaired.is_file() or digest(repaired) != receipt["audio_sha256"]:
                raise ValueError("Existing repaired WAV does not match its receipt")
            return repaired, receipt
        return None, receipt

    renders_root.mkdir(parents=True, exist_ok=True)
    words, reference = queue["words"], queue["reference"]
    duration = queue["duration"]
    with tempfile.TemporaryDirectory(dir=renders_root) as scratch:
        ready = Path(scratch) / "repair"
        ready.mkdir()
        splices, snippet_records = [], []
        reject_reason = None
        for idx, item in enumerate(items):
            ref_start, ref_end = item.get("ref_start"), item.get("ref_end")
            if ref_start is None or ref_end is None:
                reject_reason = "missing_reference_bounds"
                break
            prev_last = items[idx - 1]["last"] if idx > 0 else 0
            next_first = items[idx + 1]["first"] if idx + 1 < len(items) else len(words)
            max_left = min(8, item["first"] - prev_last, ref_start)
            max_right = min(8, next_first - item["last"], len(reference) - ref_end)
            left_avail, right_avail = _bounded_anchors(reference_text, ref_start, ref_end, max_left, max_right)
            if (left_avail < 1 and item["first"] != 0) or (right_avail < 1 and item["last"] != len(words)) or (left_avail + right_avail < 1):
                reject_reason = "insufficient_anchors"
                break
            snippet_text = _extract_reference_span(reference_text, ref_start - left_avail, ref_end + right_avail)
            snippet_tokens = surfaces(snippet_text)
            snippet_script = ready / f"snippet-{item['id']:03}.txt"
            donor_wav = ready / f"donor-{item['id']:03}.wav"
            snippet_script.write_text(snippet_text, encoding="utf-8")
            substitutions = {"{script}": str(snippet_script), "{audio}": str(donor_wav)}
            _run_renderer([substitutions.get(part, part) for part in renderer["command"]])
            if not donor_wav.is_file() or donor_wav.is_symlink():
                reject_reason = "donor_missing_wav"
                break
            donor_pcm = decode(args.ffmpeg, donor_wav, 16000)
            donor_lang, donor_segs, _ = _transcribe_narration(
                args.model, donor_pcm.astype(np.float32) / 32768, args.language, model_cache=model_cache,
            )
            donor_report = {"transcript": {"samples": len(donor_pcm), "segments": list(donor_segs), "language": donor_lang}}
            donor_words = timed_words(donor_report)
            donor_score = score(snippet_tokens, donor_words)
            expected_len = left_avail + len(item["expected"]) + right_avail
            if donor_score["edits"] != 0 or len(donor_words) != expected_len:
                reject_reason = f"donor_verification_edits_{donor_score['edits']}"
                break
            if left_avail > 0:
                left_end = words[item["first"] - 1]["end"]
                right_start = words[item["first"]]["start"] if item["recognized"] else words[item["last"]]["start"]
                orig_cut_start = (left_end + right_start) / 2
                donor_cut_start = (donor_words[left_avail - 1]["end"] + donor_words[left_avail]["start"]) / 2
            else:
                orig_cut_start, donor_cut_start = 0.0, 0.0
            donor_mid_end = left_avail + len(item["expected"])
            if right_avail > 0:
                left_end = words[item["last"] - 1]["end"] if item["recognized"] else words[item["first"] - 1]["end"]
                right_start = words[item["last"]]["start"]
                orig_cut_end = max(orig_cut_start, (left_end + right_start) / 2)
                donor_cut_end = (donor_words[donor_mid_end - 1]["end"] + donor_words[donor_mid_end]["start"]) / 2
            else:
                orig_cut_end, donor_cut_end = duration, len(donor_pcm) / 16000
            if not (0.0 <= orig_cut_start <= orig_cut_end <= duration and 0.0 <= donor_cut_start <= donor_cut_end):
                reject_reason = "invalid_splice_bounds"
                break
            splices.append((orig_cut_start, orig_cut_end, donor_wav, donor_cut_start, donor_cut_end))
            snippet_records.append({
                "id": item["id"], "snippet": snippet_text, "donor_sha256": digest(donor_wav),
                "orig_interval": [round(orig_cut_start, 4), round(orig_cut_end, 4)],
                "donor_interval": [round(donor_cut_start, 4), round(donor_cut_end, 4)],
            })
        if reject_reason is not None:
            receipt = {
                "status": "rejected", "reason": reject_reason,
                "base_audio_sha256": base_sha, "reference_sha256": reference_hash, "renderer": renderer,
            }
            write_json(ready / "receipt.json", receipt)
            ready.rename(repair_dir)
            return None, receipt
        out_wav = ready / "chapter.wav"
        _splice_wav_repairs(base_audio, splices, out_wav, args.ffmpeg)
        decode(args.ffmpeg, out_wav, 16000)
        receipt = {
            "status": "applied", "base_audio_sha256": base_sha, "reference_sha256": reference_hash,
            "renderer": renderer, "audio_sha256": digest(out_wav), "splices": snippet_records,
        }
        write_json(ready / "receipt.json", receipt)
        ready.rename(repair_dir)
    return repair_dir / "chapter.wav", receipt


def _synthesize_with_retries(args, chapter_id, binding, reference, reference_hash, learned_rules, model_cache):
    renders_root = args.workspace.resolve() / "renders" / chapter_id
    chapter_root = args.workspace.resolve() / chapter_id
    chapter_root.mkdir(exist_ok=True)
    reference_text = reference.read_text(encoding="utf-8-sig")
    model_files = ("model.bin", "config.json", "preprocessor_config.json", "tokenizer.json", "vocabulary.json")
    recipe = {"version": 1, "language": args.language, "device": "cuda", "compute_type": "int8_float16",
              "ffmpeg_sha256": digest(args.ffmpeg),
              "model": {name: digest(args.model.parent / name) for name in model_files}}

    def evaluate_audio(audio_path):
        audio_sha = digest(audio_path)
        identity = {**recipe, "audio_sha256": audio_sha, "reference_sha256": reference_hash}
        round_id = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
        folder = chapter_root / round_id
        result, review, evidence, queue = review_round(
            args, audio_path, reference, folder, identity,
            learned_rules=learned_rules, model_cache=model_cache,
        )
        return audio_path, audio_sha, identity, folder, result, review, evidence, queue

    attempts_log, seen_hashes = [], set()
    best = None

    def consider(kind, attempt_no, evaluated):
        nonlocal best
        audio_path, audio_sha, identity, folder, result, review, evidence, queue = evaluated
        edits = result["corrected"]["edits"]
        attempts_log.append({
            "attempt": attempt_no, "kind": kind, "audio": str(audio_path), "audio_sha256": audio_sha,
            "blind_edits": result["original"]["edits"], "reviewed_edits": edits,
        })
        candidate_tuple = (edits, len(attempts_log) - 1, audio_path, audio_sha, identity, folder, result, review, evidence)
        if best is None or (edits, len(attempts_log) - 1) < (best[0], best[1]):
            best = candidate_tuple

    attempt_1_audio = synthesize_chapter(binding["renderer"], renders_root / "attempt-001", reference, reference_hash, args.ffmpeg)
    seen_hashes.add(digest(attempt_1_audio))
    ev1 = evaluate_audio(attempt_1_audio)
    consider("render", 1, ev1)

    if best[0] > 0 and args.max_retries > 0 and not ev1[4]["resolved"]:
        repaired_wav, repair_receipt = _attempt_targeted_repair(
            args, renders_root, binding["renderer"], reference_text, reference_hash,
            ev1[0], ev1[1], ev1[7], model_cache,
        )
        if repaired_wav is not None:
            rep_sha = digest(repaired_wav)
            if rep_sha not in seen_hashes:
                seen_hashes.add(rep_sha)
                consider("targeted_repair", 1, evaluate_audio(repaired_wav))
        else:
            attempts_log[-1]["targeted_repair"] = repair_receipt.get("reason")

        for attempt_num in range(2, args.max_retries + 2):
            if best[0] == 0:
                break
            audio_k = synthesize_chapter(
                binding["renderer"], renders_root / f"attempt-{attempt_num:03}", reference, reference_hash, args.ffmpeg,
            )
            sha_k = digest(audio_k)
            if sha_k in seen_hashes:
                attempts_log.append({
                    "attempt": attempt_num, "kind": "render", "audio": str(audio_k),
                    "audio_sha256": sha_k, "note": "identical_to_prior_attempt",
                })
                break
            seen_hashes.add(sha_k)
            ev_k = evaluate_audio(audio_k)
            consider("render", attempt_num, ev_k)
            if best[0] > 0 and 1 <= len(ev_k[7]["items"]) <= 4:
                repaired_k, rep_receipt_k = _attempt_targeted_repair(
                    args, renders_root, binding["renderer"], reference_text, reference_hash,
                    ev_k[0], ev_k[1], ev_k[7], model_cache,
                )
                if repaired_k is not None:
                    rep_sha_k = digest(repaired_k)
                    if rep_sha_k not in seen_hashes:
                        seen_hashes.add(rep_sha_k)
                        consider("targeted_repair", attempt_num, evaluate_audio(repaired_k))
                else:
                    attempts_log[-1]["targeted_repair"] = rep_receipt_k.get("reason")

    for entry in attempts_log:
        entry["selected"] = entry.get("audio_sha256") == best[3] and entry.get("note") is None
    _, _, best_audio, best_sha, best_identity, best_folder, best_result, best_review, best_evidence = best
    return best_audio, best_sha, best_identity, best_folder, best_result, best_review, best_evidence, attempts_log


def job(args):
    manifest, manifest_hash = read_hashed(args.manifest)
    chapters = manifest["chapters"]
    if not isinstance(chapters, dict) or not chapters:
        raise ValueError("A job needs a nonempty chapter map")
    if args.model.name != "model.bin":
        raise ValueError("Select the local CTranslate2 model.bin file")
    if args.max_retries < 0:
        raise ValueError("--max-retries must be non-negative")
    model_files = ("model.bin", "config.json", "preprocessor_config.json", "tokenizer.json", "vocabulary.json")
    recipe = {"version": 1, "language": args.language, "device": "cuda", "compute_type": "int8_float16",
              "ffmpeg_sha256": digest(args.ffmpeg),
              "model": {name: digest(args.model.parent / name) for name in model_files}}
    workspace = args.workspace.resolve()
    workspace.mkdir(parents=True, exist_ok=True)
    if getattr(args, "stage", None):
        if args.stage.is_symlink():
            raise ValueError("Staging path must not be a symlink")
        args.stage = args.stage.resolve()
        if args.stage == workspace or args.stage.is_relative_to(workspace) or workspace.is_relative_to(args.stage):
            raise ValueError("Staging path must be separate from the workspace")
    renderer = renderer_binding(args.renderer) if args.renderer else None
    active_learned = _load_learned_rules(args.learned or [])
    selected, bindings = {}, {}
    for chapter_id, chapter in sorted(chapters.items()):
        if not re.fullmatch(r"[0-9]{3,8}", chapter_id):
            raise ValueError("Chapter IDs must be 3 to 8 digits")
        reference = (args.manifest.parent / chapter["reference"]).resolve(strict=True)
        if not reference.is_file() or reference.is_symlink():
            raise ValueError(f"Chapter {chapter_id} reference must be a regular local file")
        reference_hash = digest(reference)
        if chapter.get("reference_sha256", reference_hash) != reference_hash:
            raise ValueError(f"Chapter {chapter_id} differs from the approved script hash")
        if "audio" in chapter:
            original = (args.manifest.parent / chapter["audio"]).resolve(strict=True)
            if not original.is_file() or original.is_symlink():
                raise ValueError(f"Chapter {chapter_id} audio must be a regular local file")
        elif renderer:
            original = None
        else:
            raise ValueError(f"Chapter {chapter_id} needs audio or a renderer configuration")
        candidate = (args.manifest.parent / chapter["candidate"]).resolve(strict=True) if "candidate" in chapter else None
        if candidate and (not candidate.is_file() or candidate.is_symlink()):
            raise ValueError(f"Chapter {chapter_id} candidate must be a regular local file")
        audio = candidate or original
        if audio and audio.suffix.lower() not in {".wav", ".mp3", ".flac", ".m4a"}:
            raise ValueError("Select a WAV, MP3, FLAC or M4A chapter")
        selected[chapter_id] = (original, reference, candidate)
        binding = {"reference_sha256": reference_hash}
        if original:
            binding["original_sha256"] = digest(original)
        else:
            binding["renderer"] = renderer
        if original and chapter.get("audio_sha256", binding["original_sha256"]) != binding["original_sha256"]:
            raise ValueError(f"Chapter {chapter_id} differs from the synthesis handoff")
        if not original and "audio_sha256" in chapter:
            raise ValueError("An audio hash requires existing chapter audio")
        bindings[chapter_id] = {**binding, **{field: chapter[field] for field in
                                ("book_id", "plan_id", "chapter") if field in chapter}}
    roster = workspace / "job-inputs.json"
    if not roster.exists():
        # Create a single complete selection snapshot even if another job opens this workspace.
        write_json(roster, bindings, create=True)
    if read(roster) != bindings:
        raise ValueError("The selected chapters, original audio, scripts or renderer changed; start a new job")
    model_cache = {}
    status, sources, originals = {}, [], []
    for chapter_id, (original, reference, candidate) in selected.items():
        binding = dict(bindings[chapter_id])
        attempts_log = None
        if original is None and candidate is None:
            (original, original_sha, identity, folder, result, review, evidence, attempts_log) = _synthesize_with_retries(
                args, chapter_id, binding, reference, binding["reference_sha256"], active_learned, model_cache,
            )
            binding["original_sha256"] = original_sha
            audio = original
        else:
            if original is None:
                original = synthesize_chapter(
                    binding["renderer"], workspace / "renders" / chapter_id / "attempt-001",
                    reference, binding["reference_sha256"], args.ffmpeg,
                )
                binding["original_sha256"] = digest(original)
            audio = candidate or original
            identity = {**recipe, "audio_sha256": digest(audio), "reference_sha256": binding["reference_sha256"]}
            chapter_root = workspace / chapter_id
            chapter_root.mkdir(exist_ok=True)
            round_id = hashlib.sha256(json.dumps(identity, sort_keys=True).encode()).hexdigest()
            folder = chapter_root / round_id
            result, review, evidence, _ = review_round(
                args, audio, reference, folder, identity,
                learned_rules=active_learned, model_cache=model_cache,
            )
        originals.append((original, binding["original_sha256"]))
        for rule_key, entry in result.get("learned_rules", {}).items():
            active_learned[rule_key] = {
                **entry, "source_chapter": chapter_id, "decisions_sha256": result["decisions_sha256"],
            }
        ready = not result["pending_ids"] and result["corrected"]["edits"] == 0
        if ready:
            formatted = reference.read_text(encoding="utf-8-sig")
            if word_keys(result["words"]) != list(map(key, surfaces(formatted))):
                raise ValueError("Reviewed word sequence does not match the approved script")
            # Preserve the approved text bytes, including spacing, case and punctuation.
            sources.append((chapter_id + ".transcript.txt", reference, binding["reference_sha256"]))
        chapter_status = {
            "ready": ready, **binding, "audio_sha256": identity["audio_sha256"],
            "transcript": str(reference) if ready else None,
            "blind": result["original"], "reviewed": result["corrected"],
            "pending_ids": result["pending_ids"], "decisions": str(folder / "decisions.json"),
            "retained_ids": [item["id"] for item in result["resolved"] if item["action"] == "retain"],
            "carried_ids": result.get("carried_ids", []),
            "suggested_ids": result.get("suggested_ids", []),
            "review": str(review), "rerun": str(evidence) if evidence.exists() else None,
            "audio": str(audio),
        }
        if attempts_log is not None:
            chapter_status["synthesis_attempts"] = attempts_log
        status[chapter_id] = chapter_status
        sources.extend([(chapter_id + audio.suffix.lower(), audio, identity["audio_sha256"]),
                        (chapter_id + ".review.json", review, digest(review)),
                        (chapter_id + ".script.txt", reference, binding["reference_sha256"]),
                        (chapter_id + ".queue.json", folder / "queue.json", result["queue_sha256"]),
                        (chapter_id + ".decisions.json", folder / "decisions.json", result["decisions_sha256"]),
                        (chapter_id + ".blind.json", folder / "blind.json", digest(folder / "blind.json"))])
    if (manifest_hash != digest(args.manifest) or recipe["ffmpeg_sha256"] != digest(args.ffmpeg)
            or recipe["model"] != {name: digest(args.model.parent / name) for name in model_files}):
        raise ValueError("The job manifest, model or decoder changed during review")
    if (any(digest(path) != expected for _, path, expected in sources)
            or any(digest(path) != expected for path, expected in originals)):
        raise ValueError("Chapter inputs or decisions changed during review")
    result = {"ready": all(chapter["ready"] for chapter in status.values()),
              "source_run_id": manifest.get("source_run_id"), "chapters": status}
    if active_learned:
        learned_path = workspace / "learned.json"
        write_json(learned_path, {"version": 1, "rules": active_learned})
        result["learned_file"] = str(learned_path)
    if result["ready"] and getattr(args, "stage", None):
        stage_files = sorted([name for name, _, _ in sources] + ["job.json"])
        ids = " ".join(str(idx) for idx in range(1, len(stage_files) + 1))
        stage_key = hashlib.sha256(os.fsencode(args.stage)).hexdigest()
        receipt = workspace / f"stage-{stage_key}.json"
        next_commands = [
            "intake-agent narration-delivery-plan <stage-root> <delivery-root> <stage-receipt.json>",
            f"intake-agent verify <plan.json> <stage-root> <delivery-root> {ids}",
            f"intake-agent apply <plan.json> <stage-root> <delivery-root> <journal.jsonl> {ids}",
            "intake-agent receipt <journal.jsonl>",
            "intake-agent recover <journal.jsonl> <stage-root> <delivery-root>",
        ]
        result["stage"] = str(args.stage)
        result["stage_receipt"] = str(receipt)
        result["stage_files"] = stage_files
        result["next_commands"] = next_commands
        job_hash = stage_delivery(args.stage, result, sources)
        stage_receipt = {"version": 1, "stage": str(args.stage),
                         "files": {name: expected for name, _, expected in sources} | {"job.json": job_hash}}
        write_json(receipt, stage_receipt, create=True)
        if receipt.is_symlink() or read(receipt) != stage_receipt:
            raise ValueError("Staging receipt does not match the reviewed package")
        print(f"Staged verified delivery set: {args.stage}", file=sys.stderr)
        print(f"Staging receipt: {receipt}", file=sys.stderr)
        for line in next_commands:
            print(f"  {line}", file=sys.stderr)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    queue = commands.add_parser("plan")
    queue.add_argument("receipt", type=Path)
    queue.add_argument("reference", type=Path)
    queue.add_argument("audio", type=Path)
    queue.add_argument("--alternate", type=Path)
    queue.add_argument("--suggestion", type=Path)
    queue.add_argument("--clips", type=Path)
    queue.add_argument("--ffmpeg", type=Path)
    resolve = commands.add_parser("apply")
    resolve.add_argument("queue", type=Path)
    resolve.add_argument("decisions", type=Path)
    transfer = commands.add_parser("suggest")
    transfer.add_argument("receipt", type=Path)
    transfer.add_argument("review", type=Path)
    measure = commands.add_parser("evaluate")
    measure.add_argument("receipt", type=Path)
    measure.add_argument("suggestion", type=Path)
    measure.add_argument("reference", type=Path)
    retry = commands.add_parser("rerun")
    retry.add_argument("queue", type=Path)
    retry.add_argument("review", type=Path)
    retry.add_argument("model", type=Path)
    retry.add_argument("--language", default="en")
    run = commands.add_parser("job", help="Run/resume the CUDA zero-word-error narration gate")
    run.add_argument("manifest", type=Path)
    run.add_argument("workspace", type=Path)
    run.add_argument("--model", type=Path, required=True)
    run.add_argument("--ffmpeg", type=Path, required=True)
    run.add_argument("--language", default="en")
    run.add_argument("--renderer", type=Path, help="Local renderer config for chapters supplied as scripts")
    run.add_argument("--max-retries", type=int, default=2, help="Automatic synthesis retries after flagged discrepancies")
    run.add_argument("--learned", action="append", type=Path, default=[], help="Prior review or learned.json file to verify against new clips")
    run.add_argument("--stage", type=Path, help="New local staging directory for Rust plan/verify/apply delivery")
    repair = commands.add_parser("patch", help="Apply an equal-length donor interval to a new WAV")
    for name in ("audio", "donor", "output"):
        repair.add_argument(name, type=Path)
    repair.add_argument("--ffmpeg", type=Path, required=True)
    repair.add_argument("--start", type=float, required=True)
    repair.add_argument("--end", type=float, required=True)
    repair.add_argument("--donor-start", type=float, default=0.0)
    args = parser.parse_args()
    if args.command == "plan" and bool(args.clips) != bool(args.ffmpeg):
        parser.error("--clips and --ffmpeg must be supplied together")
    try:
        handlers = {"plan": plan, "apply": apply, "suggest": suggest, "evaluate": evaluate,
                    "rerun": rerun, "job": job, "patch": patch}
        result = handlers[args.command](args)
    except (ValueError, KeyError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Review failed: {error}\n")
    print(json.dumps(result, allow_nan=False))
    if args.command == "job" and not result["ready"]:
        parser.exit(2)


if __name__ == "__main__":
    main()
