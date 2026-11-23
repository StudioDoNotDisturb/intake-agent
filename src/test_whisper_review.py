"""Regression tests for local narration review and delivery."""

import argparse
from dataclasses import dataclass
import hashlib
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch
import wave

import numpy as np

import whisper_review as review


@dataclass
class _FakeWord:
    start: float
    end: float
    word: str
    probability: float = 0.99


@dataclass
class _FakeSegment:
    id: int
    seek: int
    start: float
    end: float
    text: str
    tokens: list
    avg_logprob: float
    compression_ratio: float
    no_speech_prob: float
    words: list
    temperature: float = 0.0


@dataclass
class _FakeInfo:
    language: str = "en"


def _write_tone_wav(path, duration_s=1.0, rate=16000, freq=220.0):
    samples = int(round(duration_s * rate))
    t = np.arange(samples, dtype=np.float64) / rate
    pcm = (np.sin(2 * np.pi * freq * t) * 4000).astype("<i2")
    with wave.open(str(path), "wb") as stream:
        stream.setparams((1, 2, rate, 0, "NONE", "not compressed"))
        stream.writeframes(pcm.tobytes())


def _fake_segment(text, total_s=1.0):
    tokens = text.split()
    step = total_s / max(1, len(tokens))
    words = [_FakeWord(round(i * step, 3), round((i + 1) * step, 3), tok) for i, tok in enumerate(tokens)]
    return _FakeSegment(1, 0, 0.0, total_s, " " + text, [], -0.1, 1.0, 0.0, words)


class NarrationReviewTests(unittest.TestCase):
    def setUp(self):
        scratch = tempfile.TemporaryDirectory()
        self.addCleanup(scratch.cleanup)
        self.root = Path(scratch.name)

    def _setup_model_dir(self):
        model_dir = self.root / "model"
        model_dir.mkdir()
        for name in ("model.bin", "config.json", "preprocessor_config.json", "tokenizer.json", "vocabulary.json"):
            (model_dir / name).write_bytes(name.encode())
        return model_dir / "model.bin"

    def _fake_decode(self, _ffmpeg, audio, rate):
        with wave.open(str(audio), "rb") as stream:
            raw = stream.readframes(stream.getnframes())
            orig_rate = stream.getframerate()
        arr = np.frombuffer(raw, dtype="<i2")
        if orig_rate == rate:
            return arr
        out_len = max(1, round(len(arr) * rate / orig_rate))
        return np.zeros(out_len, dtype="<i2")

    def _fake_plan_clips(self, real_plan, args):
        result = real_plan(argparse.Namespace(
            receipt=args.receipt, reference=args.reference, audio=args.audio,
            alternate=args.alternate, suggestion=args.suggestion, clips=None, ffmpeg=None,
        ))
        if args.clips:
            for item in result["items"]:
                clip = args.clips / f"{item['id']:03}.wav"
                _write_tone_wav(clip, max(0.2, item["clip_end"] - item["clip_start"]))
                item.update(clip=str(clip), clip_sha256=review.digest(clip))
        return result

    def test_changed_renderer_script_or_asset_invalidates_binding(self):
        script = self.root / "render.py"
        asset = self.root / "voice.bin"
        script.write_text("version one", encoding="utf-8")
        asset.write_bytes(b"voice one")
        config = self.root / "renderer.json"
        config.write_text(json.dumps({
            "command": [sys.executable, str(script), "{script}", "{audio}"],
            "inputs": [asset.name],
        }), encoding="utf-8")
        binding = review.renderer_binding(config)
        self.assertTrue(review.renderer_inputs_match(binding))
        for path in (script, asset):
            original = path.read_bytes()
            path.write_bytes(b"changed")
            self.assertFalse(review.renderer_inputs_match(binding))
            self.assertNotEqual(review.renderer_binding(config), binding)
            path.write_bytes(original)

    def test_absent_word_timings_report_the_segment(self):
        report = {"transcript": {"segments": [
            {"text": "…", "words": None},
            {"text": "The river", "words": None},
        ]}}
        with self.assertRaisesRegex(ValueError, "segment 2"):
            review.timed_words(report)

    def test_split_contraction_keeps_the_original_text_and_timing(self):
        report = {"transcript": {"segments": [{
            "text": "Don’t", "words": [
                {"word": "Don", "start": 1.0, "end": 1.2},
                {"word": "’t", "start": 1.2, "end": 1.4},
            ],
        }]}}
        self.assertEqual(review.timed_words(report), [{"word": "Don’t", "start": 1.0, "end": 1.4}])

    def test_stage_delivery_writes_flat_set_and_refuses_mismatched_or_empty_dir(self):
        empty = self.root / "empty-stage"
        empty.mkdir()
        with self.assertRaisesRegex(ValueError, "choose a fresh path"):
            review.stage_delivery(empty, {}, [])
        self.assertEqual(list(empty.iterdir()), [])

        src = self.root / "001.txt"
        src.write_text("hello", encoding="utf-8")
        stage = self.root / "stage"
        sources = [("001.transcript.txt", src, review.digest(src))]
        result = {"ready": True, "stage": str(stage), "stage_files": ["001.transcript.txt", "job.json"]}
        review.stage_delivery(stage, result, sources)
        self.assertTrue((stage / "job.json").is_file())
        self.assertEqual((stage / "001.transcript.txt").read_text(encoding="utf-8"), "hello")

        # Idempotent repeat with identical stage succeeds; tampered or extra file fails.
        review.stage_delivery(stage, result, sources)
        (stage / "extra.txt").write_text("stray", encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "choose a fresh path"):
            review.stage_delivery(stage, result, sources)

    def test_job_stages_only_when_ready_and_prints_rust_commands(self):
        model = self._setup_model_dir()
        ffmpeg = self.root / "ffmpeg.exe"
        ffmpeg.write_bytes(b"ffmpeg")
        script = self.root / "ch1.txt"
        script.write_text("Ask Newport today.", encoding="utf-8")
        audio = self.root / "ch1.wav"
        _write_tone_wav(audio, 1.0, freq=220.0)
        manifest = self.root / "manifest.json"
        manifest.write_text(json.dumps({"chapters": {"001": {"audio": audio.name, "reference": script.name}}}), encoding="utf-8")

        class _MockModel:
            def transcribe(self, pcm, **kwargs):
                return [_fake_segment("Ask new port today", len(pcm) / 16000.0)], _FakeInfo()

        workspace = self.root / "work"
        stage = self.root / "stage"
        real_plan = review.plan
        with patch.object(review, "decode", side_effect=self._fake_decode), \
             patch.object(review, "plan", side_effect=lambda a: self._fake_plan_clips(real_plan, a)), \
             patch("whisper_worker.faster_model", return_value=_MockModel()):
            with self.assertRaisesRegex(ValueError, "separate from the workspace"):
                review.job(argparse.Namespace(
                    manifest=manifest, workspace=workspace, model=model, ffmpeg=ffmpeg,
                    language="en", renderer=None, max_retries=0, learned=[], stage=workspace / "nested",
                ))
            res_hold = review.job(argparse.Namespace(
                manifest=manifest, workspace=workspace, model=model, ffmpeg=ffmpeg,
                language="en", renderer=None, max_retries=0, learned=[], stage=stage,
            ))
            self.assertFalse(res_hold["ready"])
            self.assertFalse(stage.exists())

            d_path = Path(res_hold["chapters"]["001"]["decisions"])
            d = json.loads(d_path.read_text(encoding="utf-8"))
            d["decisions"] = [{"id": 1, "action": "replace", "timings": [[0.33, 0.66]], "evidence": "clear"}]
            d_path.write_text(json.dumps(d), encoding="utf-8")

            stderr = io.StringIO()
            with patch.object(sys, "stderr", stderr):
                res_ready = review.job(argparse.Namespace(
                    manifest=manifest, workspace=workspace, model=model, ffmpeg=ffmpeg,
                    language="en", renderer=None, max_retries=0, learned=[], stage=stage,
                ))
        self.assertTrue(res_ready["ready"])
        expected_files = {
            "001.wav", "001.transcript.txt", "001.script.txt", "001.queue.json",
            "001.decisions.json", "001.blind.json", "001.review.json", "job.json",
        }
        self.assertEqual({p.name for p in stage.iterdir()}, expected_files)
        receipt_path = Path(res_ready["stage_receipt"])
        self.assertEqual(receipt_path.parent, workspace.resolve())
        receipt = review.read(receipt_path)
        self.assertEqual(receipt["stage"], str(stage.resolve()))
        self.assertEqual(receipt["files"], {name: review.digest(stage / name) for name in expected_files})
        self.assertIn(f"Staged verified delivery set: {stage}", stderr.getvalue())
        self.assertIn("intake-agent narration-delivery-plan", stderr.getvalue())
        self.assertIn("intake-agent recover", stderr.getvalue())

    def test_windows_job_object_cleans_up_renderer_child_on_force_stop(self):
        if os.name != "nt":
            self.skipTest("Windows Job Object cleanup test")
        pid_file = self.root / "grandchild.pid"
        helper = self.root / "spawn_child.py"
        helper.write_text(
            "import pathlib, subprocess, sys, time\n"
            "child = subprocess.Popen([sys.executable, '-c', 'import time; time.sleep(30)'])\n"
            "pathlib.Path(sys.argv[1]).write_text(str(child.pid), encoding='utf-8')\n"
            "child.wait()\n",
            encoding="utf-8",
        )
        runner = self.root / "run_under_job.py"
        runner.write_text(
            "import sys\n"
            "import whisper_review as review\n"
            "review._run_renderer([sys.executable, sys.argv[1], sys.argv[2]])\n",
            encoding="utf-8",
        )
        env = os.environ.copy()
        env["PYTHONPATH"] = str(Path(review.__file__).parent)
        parent = subprocess.Popen([sys.executable, str(runner), str(helper), str(pid_file)], env=env)
        deadline = time.monotonic() + 10.0
        while not pid_file.exists() and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertTrue(pid_file.exists())
        child_pid = int(pid_file.read_text(encoding="utf-8").strip())
        parent.kill()
        parent.wait(timeout=5)
        time.sleep(0.2)
        probe = subprocess.run(
            ["tasklist", "/FI", f"PID eq {child_pid}", "/NH"],
            capture_output=True, text=True, check=False,
        )
        if str(child_pid) in probe.stdout:
            subprocess.run(["taskkill", "/F", "/PID", str(child_pid)], check=False)
            self.fail(f"Renderer grandchild {child_pid} survived parent kill")

    def test_multiword_learn_validation_and_empty_side_refusal(self):
        qpath = self.root / "queue.json"
        dpath = self.root / "decisions.json"
        words = [{"word": "new", "start": 0.1, "end": 0.3}, {"word": "port", "start": 0.3, "end": 0.6}]
        queue = {
            "reference": ["Newport", "came"], "words": words + [{"word": "came", "start": 0.7, "end": 1.0}],
            "audio_sha256": "a", "reference_sha256": "r",
            "baseline": {"edits": 2, "reference_words": 2, "wer_pct": 100.0},
            "items": [{"id": 1, "edits": 2, "first": 0, "last": 2, "expected": ["Newport"],
                       "recognized": words, "clip_start": 0.0, "clip_end": 1.2}],
        }
        qpath.write_text(json.dumps(queue), encoding="utf-8")
        qhash = review.digest(qpath)
        dpath.write_text(json.dumps({
            "queue_sha256": qhash,
            "decisions": [{"id": 1, "action": "replace", "timings": [[0.1, 0.5]], "evidence": "clear", "learn": True}],
        }), encoding="utf-8")
        applied = review.apply(argparse.Namespace(queue=qpath, decisions=dpath))
        self.assertEqual(applied["corrected"]["edits"], 0)
        self.assertIn("new port => newport", applied["learned_rules"])

        queue["items"][0]["recognized"] = []
        queue["items"][0]["last"] = 0
        qpath.write_text(json.dumps(queue), encoding="utf-8")
        dpath.write_text(json.dumps({
            "queue_sha256": review.digest(qpath),
            "decisions": [{"id": 1, "action": "replace", "timings": [[0.1, 0.2]], "evidence": "omitted", "learn": True}],
        }), encoding="utf-8")
        with self.assertRaisesRegex(ValueError, "1-to-4 word replacements"):
            review.apply(argparse.Namespace(queue=qpath, decisions=dpath))

    def test_targeted_repair_fixes_omission_and_deterministic_retry_stops(self):
        model = self._setup_model_dir()
        ffmpeg = self.root / "ffmpeg.exe"
        ffmpeg.write_bytes(b"ffmpeg")
        script = self.root / "ch1.txt"
        script.write_text("Alpha beta. The river was quiet that morning. The birds sang.", encoding="utf-8")
        manifest = self.root / "manifest.json"
        manifest.write_text(json.dumps({"chapters": {"001": {"reference": script.name}}}), encoding="utf-8")

        # Renderer omits "that morning" when full script ("Alpha beta") is passed, succeeds on sentence snippet.
        render_py = self.root / "render_glitch.py"
        render_py.write_text(
            "import pathlib, sys, wave, numpy as np\n"
            "txt = pathlib.Path(sys.argv[1]).read_text(encoding='utf-8')\n"
            "freq = 440.0 if 'Alpha beta' in txt else 880.0\n"
            "dur = 1.8 if freq == 440.0 else 0.9\n"
            "n = int(16000 * dur)\n"
            "t = np.arange(n) / 16000.0\n"
            "pcm = (np.sin(2 * np.pi * freq * t) * 4000).astype('<i2')\n"
            "with wave.open(sys.argv[2], 'wb') as w:\n"
            "    w.setparams((1, 2, 16000, 0, 'NONE', 'not compressed'))\n"
            "    w.writeframes(pcm.tobytes())\n",
            encoding="utf-8",
        )
        renderer_cfg = self.root / "renderer.json"
        renderer_cfg.write_text(json.dumps({
            "command": [sys.executable if sys.executable.lower().endswith(".exe") else "C:/Windows/System32/cmd.exe",
                        str(render_py), "{script}", "{audio}"],
        }), encoding="utf-8")
        if not sys.executable.lower().endswith(".exe"):
            self.skipTest("Requires .exe Python executable on Windows")

        class _MockModel:
            def transcribe(self, pcm, **kwargs):
                dur = len(pcm) / 16000.0
                if kwargs.get("beam_size") == 10:
                    return [_fake_segment("Alpha beta The river was quiet The birds sang", dur)], _FakeInfo()
                if abs(dur - 1.8) < 0.05:
                    return [_fake_segment("Alpha beta The river was quiet The birds sang", dur)], _FakeInfo()
                if abs(dur - 0.9) < 0.05:
                    return [_fake_segment("The river was quiet that morning The birds sang", dur)], _FakeInfo()
                return [_fake_segment("Alpha beta The river was quiet that morning The birds sang", dur)], _FakeInfo()

        real_plan = review.plan
        workspace = self.root / "work"
        # Leave an orphaned temp dir in renders to verify resume ignores it cleanly.
        (workspace / "renders" / "001" / "tmporphan").mkdir(parents=True)

        with patch.object(review, "decode", side_effect=self._fake_decode), \
             patch.object(review, "plan", side_effect=lambda a: self._fake_plan_clips(real_plan, a)), \
             patch("whisper_worker.faster_model", return_value=_MockModel()):
            args = argparse.Namespace(
                manifest=manifest, workspace=workspace, model=model, ffmpeg=ffmpeg,
                language="en", renderer=renderer_cfg, max_retries=2, learned=[], stage=None,
            )
            res = review.job(args)
        self.assertTrue(res["ready"])
        ch = res["chapters"]["001"]
        self.assertEqual(ch["reviewed"]["edits"], 0)
        kinds = [entry["kind"] for entry in ch["synthesis_attempts"]]
        self.assertEqual(kinds, ["render", "targeted_repair"])

    def test_deterministic_failing_renderer_stops_after_identical_retry(self):
        model = self._setup_model_dir()
        ffmpeg = self.root / "ffmpeg.exe"
        ffmpeg.write_bytes(b"ffmpeg")
        script = self.root / "ch1.txt"
        script.write_text("The river was quiet that morning. The birds sang.", encoding="utf-8")
        manifest = self.root / "manifest.json"
        manifest.write_text(json.dumps({"chapters": {"001": {"reference": script.name}}}), encoding="utf-8")
        render_py = self.root / "render_always_bad.py"
        render_py.write_text(
            "import sys, wave, numpy as np\n"
            "pcm = np.zeros(16000, dtype='<i2')\n"
            "with wave.open(sys.argv[2], 'wb') as w:\n"
            "    w.setparams((1, 2, 16000, 0, 'NONE', 'not compressed'))\n"
            "    w.writeframes(pcm.tobytes())\n",
            encoding="utf-8",
        )
        renderer_cfg = self.root / "renderer.json"
        renderer_cfg.write_text(json.dumps({
            "command": [sys.executable, str(render_py), "{script}", "{audio}"],
        }), encoding="utf-8")
        if not sys.executable.lower().endswith(".exe"):
            self.skipTest("Requires .exe Python executable on Windows")

        class _MockModel:
            def transcribe(self, pcm, **kwargs):
                dur = len(pcm) / 16000.0
                return [_fake_segment("The river was quiet The birds sang", dur)], _FakeInfo()

        real_plan = review.plan
        workspace = self.root / "work-det"
        with patch.object(review, "decode", side_effect=self._fake_decode), \
             patch.object(review, "plan", side_effect=lambda a: self._fake_plan_clips(real_plan, a)), \
             patch("whisper_worker.faster_model", return_value=_MockModel()):
            res = review.job(argparse.Namespace(
                manifest=manifest, workspace=workspace, model=model, ffmpeg=ffmpeg,
                language="en", renderer=renderer_cfg, max_retries=3, learned=[], stage=None,
            ))
        self.assertFalse(res["ready"])
        attempts = res["chapters"]["001"]["synthesis_attempts"]
        self.assertEqual(len(attempts), 2)
        self.assertEqual(attempts[1].get("note"), "identical_to_prior_attempt")

    def test_learned_carry_forward_requires_new_clip_rerun_confirmation(self):
        model = self._setup_model_dir()
        ffmpeg = self.root / "ffmpeg.exe"
        ffmpeg.write_bytes(b"ffmpeg")
        ch1_txt = self.root / "ch1.txt"
        ch2_txt = self.root / "ch2.txt"
        ch1_txt.write_text("Ask Newport today.", encoding="utf-8")
        ch2_txt.write_text("See Newport tomorrow.", encoding="utf-8")
        a1 = self.root / "ch1.wav"
        a2 = self.root / "ch2.wav"
        _write_tone_wav(a1, 1.0, freq=220.0)
        _write_tone_wav(a2, 1.5, freq=330.0)
        manifest = self.root / "manifest.json"
        manifest.write_text(json.dumps({
            "chapters": {
                "001": {"audio": a1.name, "reference": ch1_txt.name},
                "002": {"audio": a2.name, "reference": ch2_txt.name},
            }
        }), encoding="utf-8")

        class _MockModel:
            def __init__(self, confirm_ch2):
                self.confirm_ch2 = confirm_ch2

            def transcribe(self, pcm, **kwargs):
                dur = len(pcm) / 16000.0
                if kwargs.get("beam_size") == 10:
                    if dur > 1.2 or abs(dur - 1.5) < 0.1:
                        return [_fake_segment("See Newport tomorrow" if self.confirm_ch2 else "See someone else", dur)], _FakeInfo()
                    return [_fake_segment("Ask new port today", dur)], _FakeInfo()
                if abs(dur - 1.0) < 0.05:
                    return [_fake_segment("Ask new port today", dur)], _FakeInfo()
                return [_fake_segment("See new port tomorrow", dur)], _FakeInfo()

        real_plan = review.plan
        workspace_hold = self.root / "work-hold"
        args_hold = argparse.Namespace(
            manifest=manifest, workspace=workspace_hold, model=model, ffmpeg=ffmpeg,
            language="en", renderer=None, max_retries=0, learned=[], stage=None,
        )

        # Workspace 1: ch2 rerun does NOT confirm "See Newport tomorrow" -> must stay pending as suggestion.
        with patch.object(review, "decode", side_effect=self._fake_decode), \
             patch.object(review, "plan", side_effect=lambda a: self._fake_plan_clips(real_plan, a)), \
             patch("whisper_worker.faster_model", return_value=_MockModel(confirm_ch2=False)):
            res1 = review.job(args_hold)
            self.assertFalse(res1["ready"])
            d1_path = Path(res1["chapters"]["001"]["decisions"])
            d1 = json.loads(d1_path.read_text(encoding="utf-8"))
            d1["decisions"] = [{"id": 1, "action": "replace", "timings": [[0.33, 0.66]], "evidence": "verified audio", "learn": True}]
            d1_path.write_text(json.dumps(d1), encoding="utf-8")
            res2 = review.job(args_hold)
            self.assertFalse(res2["ready"])
            self.assertEqual(res2["chapters"]["002"]["suggested_ids"], [1])
            self.assertEqual(res2["chapters"]["002"]["carried_ids"], [])

        # Workspace 2 (via --learned from Workspace 1): ch2 rerun confirms "See Newport tomorrow" -> carries forward!
        ch2_only = self.root / "ch2-only.json"
        ch2_only.write_text(json.dumps({"chapters": {"002": {"audio": a2.name, "reference": ch2_txt.name}}}), encoding="utf-8")
        workspace_pass = self.root / "work-pass"
        with patch.object(review, "decode", side_effect=self._fake_decode), \
             patch.object(review, "plan", side_effect=lambda a: self._fake_plan_clips(real_plan, a)), \
             patch("whisper_worker.faster_model", return_value=_MockModel(confirm_ch2=True)):
            res3 = review.job(argparse.Namespace(
                manifest=ch2_only, workspace=workspace_pass, model=model, ffmpeg=ffmpeg,
                language="en", renderer=None, max_retries=0,
                learned=[workspace_hold / "learned.json"], stage=None,
            ))
            self.assertTrue(res3["ready"])
            self.assertEqual(res3["chapters"]["002"]["carried_ids"], [1])

    def test_kokoro_worker_chapter_handles_bom_whitespace_lines_and_espeak_env(self):
        mock_pkg = self.root / "mock_kokoro"
        mock_pkg.mkdir()
        seen_log = self.root / "seen.json"
        (mock_pkg / "kokoro_onnx.py").write_text(
            "import json, os, numpy as np\n"
            "class _Sess:\n"
            "    def get_providers(self): return ['CPUExecutionProvider']\n"
            "class Kokoro:\n"
            "    def __init__(self, m, v): self.sess = _Sess(); self.calls = []\n"
            "    def get_voices(self): return {'am_michael'}\n"
            "    def create(self, text, voice, speed=1.0, lang='en-us'):\n"
            "        self.calls.append(text)\n"
            f"        open({str(seen_log)!r}, 'w', encoding='utf-8').write(json.dumps({{'calls': self.calls, 'espeak': os.environ.get('PHONEMIZER_ESPEAK_LIBRARY')}}))\n"
            "        return np.array([0.25, -0.5], dtype=np.float32), 24000\n",
            encoding="utf-8",
        )
        req = self.root / "reqs.txt"
        req.write_text("", encoding="utf-8")
        script = self.root / "chapter.txt"
        script.write_bytes(b"\xef\xbb\xbfFirst paragraph.\r\n  \t \r\nSecond paragraph.")
        out_wav = self.root / "out.wav"
        worker = Path(review.__file__).parent / "kokoro_worker.py"
        env = os.environ.copy()
        env["PYTHONPATH"] = str(mock_pkg)
        env["PHONEMIZER_ESPEAK_LIBRARY"] = "C:/bogus/espeak.dll"
        subprocess.run(
            [sys.executable, str(worker), "m.onnx", "v.bin", "am_michael", str(req), str(script), str(out_wav)],
            check=True, env=env,
        )
        seen = json.loads(seen_log.read_text(encoding="utf-8"))
        self.assertEqual(seen["calls"], ["First paragraph.", "Second paragraph."])
        self.assertIsNone(seen["espeak"])
        with wave.open(str(out_wav), "rb") as stream:
            self.assertEqual((stream.getnchannels(), stream.getsampwidth(), stream.getframerate()), (1, 2, 24000))
            expected_frames = 2 + int(24000 * 0.35) + 2
            self.assertEqual(stream.getnframes(), expected_frames)


if __name__ == "__main__":
    unittest.main()
