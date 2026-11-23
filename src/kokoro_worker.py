"""Produce a 24 kHz mono PCM preview or chapter with explicit local assets."""

from importlib.metadata import version
import os
from pathlib import Path
import re
import sys

os.environ.pop("PHONEMIZER_ESPEAK_LIBRARY", None)

model, voices, voice, requirements, *chapter = sys.argv[1:]
if chapter and len(chapter) != 2:
    raise SystemExit("Chapter rendering requires a script path and a WAV output path")
pins = Path(requirements).read_text(encoding="utf-8") if chapter else requirements
for pin in pins.splitlines():
    requirement, _, marker = pin.partition(";")
    if marker and sys.platform != "win32":
        continue
    name, expected = requirement.split("==")
    if version(name) != expected:
        raise SystemExit(f"Kokoro dependency {name} must be {expected}")

import io
import wave

import numpy as np
from kokoro_onnx import Kokoro

engine = Kokoro(model, voices)
if engine.sess.get_providers() != ["CPUExecutionProvider"]:
    raise SystemExit("Kokoro CPU provider unavailable")
if voice not in engine.get_voices():
    raise SystemExit("Voice is absent from the selected voice pack")
if chapter:
    script, destination = map(Path, chapter)
    paragraphs = [
        part.strip()
        for part in re.split(r"\n[ \t]*\n+", script.read_text(encoding="utf-8-sig"))
        if part.strip()
    ]
    if not paragraphs:
        raise SystemExit("Chapter script is empty")
    sample_rate = 24000
    silence = bytes(int(sample_rate * 0.35) * 2)
    with destination.open("xb") as output, wave.open(output, "wb") as wav:
        wav.setparams((1, 2, sample_rate, 0, "NONE", "not compressed"))
        frames = 0
        for index, paragraph in enumerate(paragraphs):
            samples, rate = engine.create(paragraph, voice, speed=1.0, lang="en-us")
            if rate != sample_rate or not samples.size or not np.isfinite(samples).all():
                raise SystemExit(f"Kokoro returned invalid audio for paragraph {index + 1}")
            pcm = np.rint(np.clip(samples, -1, 1) * 32767).astype("<i2")
            if not pcm.any():
                raise SystemExit(f"Kokoro returned silent audio for paragraph {index + 1}")
            frames += samples.size + (len(silence) // 2 if index else 0)
            if frames > sample_rate * 3 * 60 * 60:
                raise SystemExit("Kokoro chapter audio exceeds three hours")
            if index:
                wav.writeframesraw(silence)
            wav.writeframesraw(pcm.tobytes())
    sys.exit(0)

text = sys.stdin.buffer.read().decode("utf-8").strip()
samples, rate = engine.create(text, voice, speed=1.0, lang="en-us", trim=False)
if rate != 24000 or not 0 < samples.size <= rate * 120 or not np.isfinite(samples).all():
    raise SystemExit("Kokoro returned invalid or oversized audio")
pcm = np.rint(np.clip(samples, -1, 1) * 32767).astype("<i2")
if not pcm.any():
    raise SystemExit("Kokoro returned silent audio")

output = io.BytesIO()
with wave.open(output, "wb") as wav:
    wav.setparams((1, 2, rate, 0, "NONE", "not compressed"))
    wav.writeframes(pcm.tobytes())
sys.stdout.buffer.write(output.getvalue())
