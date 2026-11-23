import importlib.metadata
import json
import os
import sys
import sysconfig
from dataclasses import asdict
from pathlib import Path

import numpy


def openai_transcript(model_path, audio, language):
    import whisper

    model = whisper.load_model(model_path, device="cpu")
    result = model.transcribe(
        audio,
        language=language,
        fp16=False,
        verbose=None,
        temperature=0.0,
        word_timestamps=True,
    )
    return result["language"], result["segments"], {
        "whisper_version": importlib.metadata.version("openai-whisper"),
        "torch_version": importlib.metadata.version("torch"),
    }


def faster_model(model_path, device, compute_type):
    if device == "cuda" and sys.platform == "win32":
        nvidia = Path(sysconfig.get_path("purelib")) / "nvidia"
        bins = [nvidia / component / "bin" for component in ("cublas", "cudnn", "cuda_nvrtc")]
        os.environ["PATH"] = os.pathsep.join(
            [str(path) for path in bins if path.is_dir()] + [os.environ.get("PATH", "")]
        )

    from faster_whisper import WhisperModel

    return WhisperModel(
        str(Path(model_path).parent), device=device, compute_type=compute_type,
        cpu_threads=8, local_files_only=True,
    )


def faster_narration(model_path, audio, language, device, compute_type):
    model = faster_model(model_path, device, compute_type)
    segments, info = model.transcribe(
        audio,
        language=language,
        temperature=0.0,
        beam_size=5,
        condition_on_previous_text=False,
        no_speech_threshold=None,
        word_timestamps=True,
        vad_filter=False,
    )
    return info.language, (asdict(segment) for segment in segments), {
        "faster_whisper_version": importlib.metadata.version("faster-whisper"),
        "ctranslate2_version": importlib.metadata.version("ctranslate2"),
    }


def main():
    model_path, language, sample_limit, engine, device, compute_type = sys.argv[1:]
    limit = 2 * int(sample_limit)
    pcm = sys.stdin.buffer.read(limit + 1)
    if not pcm or len(pcm) > limit or len(pcm) % 2:
        raise ValueError("Expected up to one hour of 16 kHz mono PCM")
    audio = numpy.frombuffer(pcm, dtype="<i2").astype(numpy.float32) / 32768.0
    language = None if language == "auto" else language
    if engine == "open-ai":
        detected, raw_segments, versions = openai_transcript(model_path, audio, language)
    else:
        detected, raw_segments, versions = faster_narration(
            model_path, audio, language, device, compute_type
        )
    segments = [
        {
            "start": segment["start"],
            "end": segment["end"],
            "text": segment["text"].strip(),
            "words": [
                {**word, "word": word["word"].strip()}
                for word in segment.get("words") or []
                if word["word"].strip()
            ],
        }
        for segment in raw_segments
        if segment["text"].strip()
    ]
    report = {
        "samples": len(audio),
        "language": detected,
        "segments": segments,
        **versions,
        "numpy_version": importlib.metadata.version("numpy"),
    }
    print(json.dumps(report, allow_nan=False))


if __name__ == "__main__":
    main()
