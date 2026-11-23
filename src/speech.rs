use crate::{SCHEMA_VERSION, checked_root, inspect_asset, invalid, recording};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Write},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::Duration,
};

const WORKER: &str = include_str!("kokoro_worker.py");
const REQUIREMENTS: &str = include_str!("../kokoro-requirements.txt");
const TEXT_BYTES: u64 = 1024;
const WAV_BYTES: u64 = 44 + 24_000 * 2 * 120;

fn digest(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

pub fn preview(root: &Path, relative: &Path, voice: &str) -> io::Result<Value> {
    if voice.is_empty()
        || voice.len() > 64
        || !voice
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
    {
        return Err(invalid("Expected a Kokoro voice ID"));
    }
    let root = checked_root(root)?;
    let (source, text) = inspect_asset(&root, relative, &mut |file, metadata, sha256| {
        if metadata.len() > TEXT_BYTES {
            return Err(invalid("Speech preview text exceeds 1024 bytes"));
        }
        let mut text = String::new();
        file.take(TEXT_BYTES + 1).read_to_string(&mut text)?;
        if text.trim().is_empty()
            || text
                .chars()
                .any(|ch| ch.is_control() && !matches!(ch, '\r' | '\n' | '\t'))
            || digest(&text) != sha256
        {
            return Err(invalid("Expected unchanged, nonempty UTF-8 text"));
        }
        Ok(text)
    })?;
    let (python, python_sha256) = recording::engine("INTAKE_KOKORO_PYTHON")?;
    let (model, model_sha256) = recording::engine("INTAKE_KOKORO_MODEL")?;
    let (voices, voices_sha256) = recording::engine("INTAKE_KOKORO_VOICES")?;
    let mut child = Command::new(&python)
        .args(["-I", "-B", "-c", WORKER])
        .arg(&model)
        .arg(&voices)
        .args([voice, REQUIREMENTS])
        .env("ONNX_PROVIDER", "CPUExecutionProvider")
        .env_remove("PHONEMIZER_ESPEAK_LIBRARY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped Kokoro output");
    let input = child
        .stdin
        .take()
        .expect("piped Kokoro input")
        .write_all(text.as_bytes());
    let output = thread::spawn(move || {
        let mut audio = Vec::new();
        stdout.take(WAV_BYTES + 1).read_to_end(&mut audio)?;
        Ok::<_, io::Error>(audio)
    });
    let status = recording::wait(&mut child, "Kokoro", Duration::from_secs(120));
    let audio = output
        .join()
        .map_err(|_| io::Error::other("Kokoro reader failed"))??;
    status?;
    input?;
    if audio.len() <= 44
        || audio.len() as u64 > WAV_BYTES
        || audio.len() % 2 != 0
        || !audio.starts_with(b"RIFF")
        || &audio[8..16] != b"WAVEfmt "
        || &audio[16..40] != b"\x10\0\0\0\x01\0\x01\0\xc0\x5d\0\0\x80\xbb\0\0\x02\0\x10\0data"
        || audio[4..8] != ((audio.len() - 8) as u32).to_le_bytes()
        || audio[40..44] != ((audio.len() - 44) as u32).to_le_bytes()
    {
        return Err(invalid("Invalid Kokoro PCM WAV"));
    }
    for (path, expected) in [
        (&python, &python_sha256),
        (&model, &model_sha256),
        (&voices, &voices_sha256),
    ] {
        if recording::file_digest(path)? != *expected {
            return Err(invalid("A Kokoro runtime asset changed during preview"));
        }
    }
    Ok(json!({
        "schema_version": SCHEMA_VERSION, "source": source,
        "engine": {
            "name": "kokoro-onnx", "provider": "CPUExecutionProvider",
            "requirements_sha256": digest(REQUIREMENTS), "worker_sha256": digest(WORKER),
            "python_sha256": python_sha256, "model_sha256": model_sha256,
            "voices_sha256": voices_sha256, "voice": voice, "language": "en-us",
            "speed": 1.0, "trim": false
        },
        "audio": {
            "format": "wav-pcm-s16le", "channels": 1, "sample_rate_hz": 24000,
            "frames": (audio.len() - 44) / 2, "sha256": digest(&audio),
            "wav_base64": STANDARD.encode(&audio)
        }
    }))
}
