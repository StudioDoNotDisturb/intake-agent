use crate::{
    Output, Plan, build_plan, checked_root, inspect_asset, invalid, validate_relative, video,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    fs::File,
    io::{self, Read, Seek},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const MP3_RATES: [u32; 9] = [48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000];

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mp3Quality {
    Archival,
    High,
    Portable,
}

impl Mp3Quality {
    pub fn parse(value: &str) -> io::Result<Self> {
        match value {
            "archival" => Ok(Self::Archival),
            "high" => Ok(Self::High),
            "portable" => Ok(Self::Portable),
            _ => Err(invalid("Expected MP3 quality archival, high or portable")),
        }
    }

    #[cfg(any(windows, target_os = "linux"))]
    fn options(self) -> [&'static str; 2] {
        match self {
            Self::Archival => ["-q:a", "0"],
            Self::High => ["-q:a", "2"],
            Self::Portable => ["-b:a", "160k"],
        }
    }
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioRecipe {
    recipe_version: u32,
    pub stream_index: u32,
    pub sample_rate_hz: u32,
    pub channels: u8,
    pub quality: Mp3Quality,
    pub ffmpeg_sha256: String,
    pub ffprobe_sha256: String,
}

pub(crate) fn valid_mp3_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp3"))
        && validate_relative(path).is_ok()
}

pub(crate) fn valid_mp4_path(path: &Path) -> bool {
    path.extension()
        .is_some_and(|ext| ext.eq_ignore_ascii_case("mp4"))
        && validate_relative(path).is_ok()
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ClipRecipe {
    recipe_version: u32,
    start_ms: u64,
    end_ms: u64,
    video_stream: u32,
    audio_stream: Option<u32>,
    ffmpeg_sha256: String,
    ffprobe_sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    transcript: Option<TranscriptLink>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TranscriptLink {
    audio_sha256: String,
    transcript_sha256: String,
    extraction_plan_sha256: String,
    extraction_action_id: u32,
    audio_stream: u32,
}

#[cfg(any(windows, target_os = "linux"))]
impl ClipRecipe {
    pub(crate) fn valid(&self) -> bool {
        self.recipe_version == 1
            && valid_clip_range(self.start_ms, self.end_ms)
            && self.audio_stream != Some(self.video_stream)
            && crate::execution::valid_digest(&self.ffmpeg_sha256)
            && crate::execution::valid_digest(&self.ffprobe_sha256)
            && self.transcript.as_ref().is_none_or(|link| {
                self.audio_stream == Some(link.audio_stream)
                    && link.extraction_action_id > 0
                    && [
                        &link.audio_sha256,
                        &link.transcript_sha256,
                        &link.extraction_plan_sha256,
                    ]
                    .iter()
                    .all(|hash| crate::execution::valid_digest(hash))
            })
    }

    pub(crate) fn engines(&self) -> io::Result<(PathBuf, PathBuf)> {
        checked_engines(&self.ffmpeg_sha256, &self.ffprobe_sha256)
    }
}

fn valid_clip_range(start_ms: u64, end_ms: u64) -> bool {
    end_ms <= 604_800_000 && (100..=600_000).contains(&end_ms.saturating_sub(start_ms))
}

#[cfg(any(windows, target_os = "linux"))]
impl AudioRecipe {
    pub(crate) fn valid(&self) -> bool {
        self.recipe_version == 1
            && MP3_RATES.contains(&self.sample_rate_hz)
            && matches!(self.channels, 1 | 2)
            && [&self.ffmpeg_sha256, &self.ffprobe_sha256]
                .iter()
                .all(|digest| crate::execution::valid_digest(digest))
    }

    pub(crate) fn engines(&self) -> io::Result<(PathBuf, PathBuf)> {
        checked_engines(&self.ffmpeg_sha256, &self.ffprobe_sha256)
    }
}

#[cfg(any(windows, target_os = "linux"))]
fn checked_engines(
    ffmpeg_expected: &str,
    ffprobe_expected: &str,
) -> io::Result<(PathBuf, PathBuf)> {
    let (ffmpeg, ffmpeg_hash) = engine("INTAKE_FFMPEG")?;
    let (ffprobe, ffprobe_hash) = engine("INTAKE_FFPROBE")?;
    if ffmpeg_hash != ffmpeg_expected || ffprobe_hash != ffprobe_expected {
        return Err(invalid(
            "FFmpeg or ffprobe does not match the approved recipe",
        ));
    }
    Ok((ffmpeg, ffprobe))
}

fn duration_seconds(value: &serde_json::Value) -> Option<f64> {
    value
        .as_str()?
        .parse::<f64>()
        .ok()
        .filter(|duration| duration.is_finite() && *duration > 0.0)
}

fn stream_index(stream: &serde_json::Value) -> io::Result<u32> {
    stream["index"]
        .as_u64()
        .and_then(|index| index.try_into().ok())
        .ok_or_else(|| invalid("Invalid media stream index"))
}

fn default_audio(streams: &[serde_json::Value]) -> Option<&serde_json::Value> {
    streams
        .iter()
        .filter(|stream| stream["codec_type"] == "audio")
        .find(|stream| stream["disposition"]["default"] == 1)
        .or_else(|| {
            streams
                .iter()
                .find(|stream| stream["codec_type"] == "audio")
        })
}

pub(crate) fn engine(setting: &str) -> io::Result<(PathBuf, String)> {
    let path = env::var_os(setting)
        .map(PathBuf::from)
        .filter(|path| path.is_absolute())
        .ok_or_else(|| invalid(format!("Set {setting} to a trusted absolute file path")))?;
    let hash = file_digest(&path).map_err(|error| {
        io::Error::new(error.kind(), format!("Could not read {setting}: {error}"))
    })?;
    Ok((path, hash))
}

pub(crate) fn file_digest(path: &Path) -> io::Result<String> {
    let mut file = File::open(path)?;
    if !file.metadata()?.is_file() {
        return Err(invalid("Expected a regular local file"));
    }
    let mut hash = Sha256::new();
    io::copy(&mut file, &mut hash)?;
    Ok(format!("{:x}", hash.finalize()))
}

fn ffmpeg_command(engine: &Path, source: File, formats: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(engine);
    cmd.args([
        "-v",
        "error",
        "-xerror",
        "-nostdin",
        "-y",
        "-max_alloc",
        "67108864",
        "-protocol_whitelist",
        "fd",
        "-format_whitelist",
        formats,
        "-i",
        "fd:",
    ])
    .args(args)
    .stdin(Stdio::from(source))
    .stderr(Stdio::null());
    cmd
}

fn terminate(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

pub(crate) fn wait(child: &mut Child, name: &str, timeout: Duration) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => return Ok(()),
            Ok(Some(status)) => {
                return Err(io::Error::other(format!("{name} exited with {status}")));
            }
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            result => {
                terminate(child);
                return match result {
                    Err(error) => Err(error),
                    _ => Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("{name} timed out"),
                    )),
                };
            }
        }
    }
}

pub fn audio_plan(
    source: &Path,
    file: &Path,
    destination: &Path,
    output: &Path,
    ordinal: Option<usize>,
    quality: Mp3Quality,
) -> io::Result<Plan> {
    if !valid_mp3_path(output) {
        return Err(invalid("Expected a safe relative MP3 output path"));
    }
    let root = checked_root(source)?;
    let (_, ffmpeg_sha256) = engine("INTAKE_FFMPEG")?;
    let (ffprobe, ffprobe_sha256) = engine("INTAKE_FFPROBE")?;
    let (asset, report) = inspect_asset(&root, file, &mut |input, _, _| {
        video::probe(&ffprobe, input.try_clone()?).map_err(invalid)
    })?;
    let streams = report["streams"].as_array().expect("validated probe");
    let stream = if let Some(ordinal) = ordinal {
        streams
            .iter()
            .filter(|stream| stream["codec_type"] == "audio")
            .nth(ordinal)
            .ok_or_else(|| invalid("Requested audio track is absent"))?
    } else {
        default_audio(streams)
            .ok_or_else(|| invalid("No audio stream found in the selected recording"))?
    };
    let rate: u32 = stream["sample_rate"]
        .as_str()
        .and_then(|rate| rate.parse().ok())
        .filter(|rate| *rate > 0)
        .ok_or_else(|| invalid("Audio stream sample rate is unavailable"))?;
    let channels = stream["channels"]
        .as_u64()
        .filter(|channels| *channels > 0)
        .ok_or_else(|| invalid("Audio stream channel count is unavailable"))?;
    let recipe = AudioRecipe {
        recipe_version: 1,
        stream_index: stream_index(stream)?,
        sample_rate_hz: *MP3_RATES
            .iter()
            .min_by_key(|candidate| candidate.abs_diff(rate))
            .expect("supported rates"),
        channels: if channels > 1 { 2 } else { 1 },
        quality,
        ffmpeg_sha256,
        ffprobe_sha256,
    };
    let directory = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = output
        .file_name()
        .and_then(|name| name.to_str())
        .expect("validated output");
    let scan = crate::Scan {
        schema_version: crate::SCHEMA_VERSION,
        root,
        assets: vec![asset],
    };
    build_plan(scan, destination, |_| {
        Ok((
            directory.to_owned(),
            Output::Audio(name.into(), recipe.clone()),
        ))
    })
}

#[cfg(any(windows, target_os = "linux"))]
mod render {
    use super::*;

    const MAX_OUTPUT: u64 = 1024 * 1024 * 1024;

    fn ffmpeg(engine: &Path, source: File, output: File, args: &[&str]) -> io::Result<()> {
        let mut child = ffmpeg_command(engine, source, video::CONTAINERS, args)
            .args(["-fs", &MAX_OUTPUT.to_string(), "fd:"])
            .stdout(Stdio::from(output))
            .spawn()?;
        wait(&mut child, "FFmpeg", Duration::from_secs(600))
    }

    fn probe_output(probe: &Path, output: &mut File) -> io::Result<serde_json::Value> {
        if output.metadata()?.len() >= MAX_OUTPUT - 1024 * 1024 {
            return Err(invalid("FFmpeg output reached the size limit"));
        }
        output.rewind()?;
        let report = video::probe(probe, output.try_clone()?).map_err(invalid)?;
        output.rewind()?;
        Ok(report)
    }

    pub(crate) fn mp3(recipe: &AudioRecipe, source: &File, output: &mut File) -> io::Result<()> {
        let (engine, probe) = recipe.engines()?;
        let quality = recipe.quality.options();
        let options = [
            "-map",
            &format!("0:{}", recipe.stream_index),
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-c:a",
            "libmp3lame",
            "-ar",
            &recipe.sample_rate_hz.to_string(),
            "-ac",
            &recipe.channels.to_string(),
            quality[0],
            quality[1],
            "-f",
            "mp3",
        ];
        ffmpeg(&engine, source.try_clone()?, output.try_clone()?, &options)?;
        let report = probe_output(&probe, output)?;
        let streams = report["streams"].as_array().expect("validated probe");
        let duration = report["format"]["duration"]
            .as_str()
            .and_then(|duration| duration.parse::<f64>().ok());
        if report["format"]["format_name"] != "mp3"
            || streams.len() != 1
            || streams[0]["codec_type"] != "audio"
            || streams[0]["codec_name"] != "mp3"
            || streams[0]["channels"] != recipe.channels
            || streams[0]["sample_rate"]
                .as_str()
                .and_then(|rate| rate.parse::<u32>().ok())
                != Some(recipe.sample_rate_hz)
            || !duration.is_some_and(|duration| duration.is_finite() && duration > 0.0)
        {
            return Err(invalid(
                "FFmpeg output does not match the approved MP3 recipe",
            ));
        }
        Ok(())
    }

    pub(crate) fn mp4(recipe: &ClipRecipe, source: &File, output: &mut File) -> io::Result<()> {
        let (engine, probe) = recipe.engines()?;
        let seconds = |ms: u64| format!("{}.{:03}", ms / 1000, ms % 1000);
        let start = seconds(recipe.start_ms);
        let duration = seconds(recipe.end_ms - recipe.start_ms);
        let video_map = format!("0:{}", recipe.video_stream);
        let audio_map = recipe.audio_stream.map(|index| format!("0:{index}"));
        let mut args = vec![
            "-ss",
            &start,
            "-t",
            &duration,
            "-map",
            &video_map,
            "-map_metadata",
            "-1",
            "-map_chapters",
            "-1",
            "-c:v",
            "libx264",
            "-preset",
            "medium",
            "-crf",
            "18",
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-pix_fmt",
            "yuv420p",
            "-fps_mode",
            "passthrough",
        ];
        if let Some(audio_map) = &audio_map {
            args.extend(["-map", audio_map, "-c:a", "aac", "-b:a", "192k"]);
        }
        args.extend(["-f", "mp4"]);
        ffmpeg(&engine, source.try_clone()?, output.try_clone()?, &args)?;
        let report = probe_output(&probe, output)?;
        let streams = report["streams"].as_array().expect("validated probe");
        let expected_duration = (recipe.end_ms - recipe.start_ms) as f64 / 1000.0;
        let valid_duration = duration_seconds(&report["format"]["duration"])
            .is_some_and(|actual| (actual - expected_duration).abs() <= 0.25);
        if !report["format"]["format_name"]
            .as_str()
            .is_some_and(|formats| formats.split(',').any(|format| format == "mp4"))
            || !valid_duration
            || streams.len() != 1 + usize::from(recipe.audio_stream.is_some())
            || streams[0]["codec_name"] != "h264"
            || !streams[0]["width"].as_u64().is_some_and(|width| width > 0)
            || !streams[0]["height"]
                .as_u64()
                .is_some_and(|height| height > 0)
            || (recipe.audio_stream.is_some() && streams[1]["codec_name"] != "aac")
        {
            return Err(invalid(
                "FFmpeg output does not match the approved MP4 recipe",
            ));
        }
        Ok(())
    }
}

#[cfg(any(windows, target_os = "linux"))]
pub(crate) use render::mp3 as render_mp3;
#[cfg(any(windows, target_os = "linux"))]
pub(crate) use render::mp4 as render_mp4;

const TRANSCRIPT_BYTES: u64 = 16 * 1024 * 1024;
const SAMPLE_RATE: u64 = 16_000;
const MAX_SAMPLES: u64 = SAMPLE_RATE * 3600;

#[derive(Clone, Copy, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
enum WhisperBackend {
    #[default]
    OpenAi,
    FasterWhisperNarration,
    FasterWhisperNarrationCuda,
}

impl WhisperBackend {
    fn recipe_version(self) -> u32 {
        match self {
            Self::OpenAi => 1,
            Self::FasterWhisperNarration => 2,
            Self::FasterWhisperNarrationCuda => 3,
        }
    }

    fn runtime(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::OpenAi => ("open-ai", "cpu", "float32"),
            Self::FasterWhisperNarration => ("faster-whisper", "cpu", "int8"),
            Self::FasterWhisperNarrationCuda => ("faster-whisper", "cuda", "int8_float16"),
        }
    }
}

fn model_components(
    backend: WhisperBackend,
    model: &Path,
) -> io::Result<BTreeMap<&'static str, String>> {
    let mut components = BTreeMap::new();
    if !matches!(backend, WhisperBackend::OpenAi) {
        if model.file_name().and_then(|name| name.to_str()) != Some("model.bin") {
            return Err(invalid(
                "Select model.bin for the Faster Whisper narration recipe",
            ));
        }
        for name in [
            "config.json",
            "preprocessor_config.json",
            "tokenizer.json",
            "vocabulary.json",
        ] {
            components.insert(name, file_digest(&model.with_file_name(name))?);
        }
    }
    Ok(components)
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WhisperWord {
    start: f64,
    end: f64,
    word: String,
    probability: f64,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WhisperSegment {
    start: f64,
    end: f64,
    text: String,
    words: Vec<WhisperWord>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct WhisperReport {
    #[serde(skip_serializing_if = "Option::is_none")]
    whisper_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    torch_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    faster_whisper_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    ctranslate2_version: Option<String>,
    numpy_version: String,
    samples: u64,
    language: String,
    segments: Vec<WhisperSegment>,
}

fn language_code(value: &str) -> bool {
    (2..=3).contains(&value.len()) && value.bytes().all(|c| c.is_ascii_lowercase())
}

impl WhisperReport {
    fn validate(&self, expected_language: &str, backend: WhisperBackend) -> io::Result<()> {
        let duration = self.samples as f64 / SAMPLE_RATE as f64;
        // Whisper operates on 20 ms frames. Allow one frame past the last PCM sample.
        let audio_end = duration + 0.02;
        // Whisper adds local word offsets to window starts. Equal boundaries can differ
        // by floating-point roundoff while still referring to the same audio instant.
        const ROUNDING_SECONDS: f64 = 1e-9;
        let bounds = |start: f64, end: f64, after: f64, before: f64| {
            start.is_finite() && end.is_finite() && start >= after && end >= start && end <= before
        };
        let mut previous_segment_end = 0.0;
        let mut previous_word_end = 0.0;
        let (versions, other_versions) = match backend {
            WhisperBackend::OpenAi => (
                [&self.whisper_version, &self.torch_version],
                [&self.faster_whisper_version, &self.ctranslate2_version],
            ),
            WhisperBackend::FasterWhisperNarration | WhisperBackend::FasterWhisperNarrationCuda => {
                (
                    [&self.faster_whisper_version, &self.ctranslate2_version],
                    [&self.whisper_version, &self.torch_version],
                )
            }
        };
        if !(1..=MAX_SAMPLES).contains(&self.samples)
            || !language_code(&self.language)
            || (expected_language != "auto" && self.language != expected_language)
            || other_versions.iter().any(|version| version.is_some())
            || [
                versions[0].as_deref(),
                versions[1].as_deref(),
                Some(&self.numpy_version),
            ]
            .into_iter()
            .any(|version| {
                version.is_none_or(|version| {
                    version.is_empty()
                        || version.len() > 80
                        || !version.bytes().all(|c| c.is_ascii_graphic())
                })
            })
        {
            return Err(invalid(
                "Invalid Whisper engine, PCM count or language report",
            ));
        }
        for segment in &self.segments {
            if segment.text.trim().is_empty()
                || !bounds(segment.start, segment.end, previous_segment_end, audio_end)
            {
                return Err(invalid("Invalid Whisper segment text or timeline"));
            }
            previous_segment_end = segment.end;
            // Word and segment timestamps are estimated separately. A word can cross a
            // segment boundary while remaining in order and overlapping its own segment.
            for word in &segment.words {
                if word.word.trim().is_empty()
                    || !bounds(
                        word.start,
                        word.end,
                        (previous_word_end - ROUNDING_SECONDS).max(0.0),
                        audio_end,
                    )
                    || word.end + ROUNDING_SECONDS < segment.start
                    || word.start > segment.end + ROUNDING_SECONDS
                    || !(0.0..=1.0).contains(&word.probability)
                {
                    return Err(invalid("Invalid Whisper word or timeline"));
                }
                previous_word_end = word.end;
            }
        }
        Ok(())
    }
}

pub fn transcribe(root: &Path, relative: &Path, language: &str) -> io::Result<serde_json::Value> {
    if !valid_mp3_path(relative) || !(language == "auto" || language_code(language)) {
        return Err(invalid(
            "Select a relative MP3 and use auto or a lowercase language code",
        ));
    }
    let root = checked_root(root)?;
    let (ffmpeg, ffmpeg_sha256) = engine("INTAKE_FFMPEG")?;
    let (python, python_sha256) = engine("INTAKE_WHISPER_PYTHON")?;
    let (model, model_sha256) = engine("INTAKE_WHISPER_MODEL")?;
    let backend = match env::var("INTAKE_WHISPER_BACKEND") {
        Err(env::VarError::NotPresent) => WhisperBackend::OpenAi,
        Ok(name) if name == "faster-whisper-narration" => WhisperBackend::FasterWhisperNarration,
        Ok(name) if name == "faster-whisper-narration-cuda" => {
            WhisperBackend::FasterWhisperNarrationCuda
        }
        _ => return Err(invalid("Unknown INTAKE_WHISPER_BACKEND")),
    };
    let components = model_components(backend, &model)?;
    let (source, report) = inspect_asset(&root, relative, &mut |file, _, sha256| {
        let report = whisper(
            &ffmpeg,
            file.try_clone()?,
            &python,
            &model,
            language,
            backend,
        )?;
        report.validate(language, backend)?;
        file.rewind()?;
        let mut digest = Sha256::new();
        io::copy(file, &mut digest)?;
        if format!("{:x}", digest.finalize()) != sha256 {
            return Err(invalid("MP3 changed during transcription"));
        }
        Ok(report)
    })?;
    if file_digest(&model)? != model_sha256 || model_components(backend, &model)? != components {
        return Err(invalid("Whisper model changed during transcription"));
    }
    let (_, device, compute_type) = backend.runtime();
    let mut inference = serde_json::json!({
        "device": device,
        "word_timestamps": true,
        "requested_language": language,
        "pcm_rate_hz": SAMPLE_RATE,
        "model_file": model.file_name().unwrap_or_default(),
        "model_sha256": model_sha256,
        "python_sha256": python_sha256,
        "ffmpeg_sha256": ffmpeg_sha256,
    });
    if !matches!(backend, WhisperBackend::OpenAi) {
        inference["backend"] = serde_json::json!(backend);
        inference["compute_type"] = serde_json::json!(compute_type);
        inference["beam_size"] = serde_json::json!(5);
        inference["condition_on_previous_text"] = serde_json::json!(false);
        inference["vad_filter"] = serde_json::json!(false);
        inference["no_speech_threshold"] = serde_json::Value::Null;
        inference["temperature"] = serde_json::json!(0);
        if matches!(backend, WhisperBackend::FasterWhisperNarration) {
            inference["cpu_threads"] = serde_json::json!(8);
        }
        inference["model_components_sha256"] = serde_json::json!(components);
    }
    Ok(serde_json::json!({
        "schema_version": 1,
        "recipe_version": backend.recipe_version(),
        "source": source,
        "root": root,
        "engine": inference,
        "transcript": report
    }))
}

fn whisper(
    ffmpeg: &Path,
    source: File,
    python: &Path,
    model: &Path,
    language: &str,
    backend: WhisperBackend,
) -> io::Result<WhisperReport> {
    let mut decode = ffmpeg_command(
        ffmpeg,
        source,
        "mp3",
        &[
            "-map",
            "0:a:0",
            "-t",
            "3600.001",
            "-ac",
            "1",
            "-ar",
            "16000",
            "-c:a",
            "pcm_s16le",
            "-f",
            "s16le",
            "pipe:1",
        ],
    )
    .stdout(Stdio::piped())
    .spawn()?;
    let pcm = decode.stdout.take().expect("piped FFmpeg output");
    let (engine, device, compute_type) = backend.runtime();
    let worker = Command::new(python)
        .args(["-I", "-c", include_str!("whisper_worker.py")])
        .arg(model)
        .arg(language)
        .arg(MAX_SAMPLES.to_string())
        .args([engine, device, compute_type])
        .env("HF_HUB_OFFLINE", "1")
        .stdin(Stdio::from(pcm))
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn();
    let result = worker.and_then(|mut worker| {
        let stdout = worker.stdout.take().expect("piped Whisper output");
        let reader = thread::spawn(move || {
            let mut bytes = Vec::new();
            stdout.take(TRANSCRIPT_BYTES + 1).read_to_end(&mut bytes)?;
            Ok::<_, io::Error>(bytes)
        });
        let status = wait(&mut worker, "Whisper", Duration::from_secs(1800));
        let bytes = reader
            .join()
            .map_err(|_| io::Error::other("Whisper output failed"))??;
        if bytes.len() as u64 > TRANSCRIPT_BYTES {
            return Err(invalid("Whisper result exceeds 16 MiB"));
        }
        status?;
        Ok(serde_json::from_slice(&bytes)?)
    });
    if result.is_err() {
        terminate(&mut decode);
    } else {
        wait(&mut decode, "FFmpeg", Duration::from_secs(30))?;
    }
    result
}

fn read_limited(path: &Path, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(invalid(format!("Input exceeds {limit} bytes")));
    }
    Ok(bytes)
}

#[derive(Deserialize)]
struct SavedTranscript {
    schema_version: u32,
    recipe_version: u32,
    root: PathBuf,
    source: crate::Asset,
    engine: TranscriptEngine,
    transcript: WhisperReport,
}

#[derive(Deserialize)]
struct TranscriptEngine {
    requested_language: String,
    pcm_rate_hz: u64,
    #[serde(default)]
    backend: WhisperBackend,
}

fn read_transcript(path: &Path) -> io::Result<(SavedTranscript, String)> {
    let bytes = read_limited(path, 64 * 1024 * 1024)?;
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let saved: SavedTranscript = serde_json::from_slice(&bytes)?;
    if saved.schema_version != 1
        || saved.recipe_version != saved.engine.backend.recipe_version()
        || saved.engine.pcm_rate_hz != SAMPLE_RATE
        || !saved.root.is_absolute()
        || !valid_mp3_path(&saved.source.relative_path)
        || saved.source.sha256.len() != 64
        || !saved.source.sha256.bytes().all(|c| c.is_ascii_hexdigit())
    {
        return Err(invalid("Invalid saved intake transcript provenance"));
    }
    saved
        .transcript
        .validate(&saved.engine.requested_language, saved.engine.backend)?;
    Ok((saved, digest))
}

#[cfg(any(windows, target_os = "linux"))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ClipSelection {
    clips: Vec<SelectedClip>,
    transcript: Option<SelectedTranscript>,
}

#[cfg(any(windows, target_os = "linux"))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedClip {
    path: PathBuf,
    start_ms: u64,
    end_ms: u64,
}

#[cfg(any(windows, target_os = "linux"))]
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SelectedTranscript {
    report: PathBuf,
    extraction_journal: PathBuf,
    action_id: u32,
}

#[cfg(any(windows, target_os = "linux"))]
impl SelectedTranscript {
    fn resolve(&self, original: &crate::Asset) -> io::Result<TranscriptLink> {
        if !self.report.is_absolute() || !self.extraction_journal.is_absolute() {
            return Err(invalid(
                "Use absolute paths for the transcript and extraction journal",
            ));
        }
        let (saved, transcript_sha256) = read_transcript(&self.report)?;
        let receipt = crate::execution::receipt(&self.extraction_journal)?;
        let entry = receipt["actions"]
            .as_array()
            .and_then(|actions| {
                actions
                    .iter()
                    .find(|entry| entry["action"]["id"] == self.action_id)
            })
            .ok_or_else(|| invalid("Extraction action is absent from the receipt"))?;
        let action: crate::Action = serde_json::from_value(entry["action"].clone())?;
        let derived = &entry["last_durable_state"]["derived"];
        if entry["status"] != "completed"
            || entry["last_durable_state"]["method"] != "extract_audio"
            || action.placement != crate::Placement::ExtractAudio
            || action.source.bytes != original.bytes
            || action.source.sha256 != original.sha256
            || derived["sha256"] != saved.source.sha256
            || derived["bytes"] != saved.source.bytes
        {
            return Err(invalid(
                "Transcript and extraction receipt do not link to the selected recording",
            ));
        }
        let audio = action
            .audio
            .ok_or_else(|| invalid("Extraction recipe is absent"))?;
        Ok(TranscriptLink {
            audio_sha256: saved.source.sha256,
            transcript_sha256,
            extraction_plan_sha256: receipt["plan_sha256"]
                .as_str()
                .ok_or_else(|| invalid("Extraction receipt is missing its plan hash"))?
                .into(),
            extraction_action_id: self.action_id,
            audio_stream: audio.stream_index,
        })
    }
}

#[cfg(any(windows, target_os = "linux"))]
pub fn clip_plan(
    source: &Path,
    recording: &Path,
    destination: &Path,
    selection: &Path,
) -> io::Result<Plan> {
    let selection: ClipSelection = serde_json::from_slice(&read_limited(selection, 64 * 1024)?)?;
    if !(1..=32).contains(&selection.clips.len()) {
        return Err(invalid(
            "Select 1 to 32 explicit clip ranges for the package",
        ));
    }
    let root = checked_root(source)?;
    if root == checked_root(destination)? {
        return Err(invalid(
            "Use a distinct destination root for the clip package",
        ));
    }
    let (_, ffmpeg_sha256) = engine("INTAKE_FFMPEG")?;
    let (ffprobe, ffprobe_sha256) = engine("INTAKE_FFPROBE")?;
    let (asset, report) = inspect_asset(&root, recording, &mut |input, _, _| {
        video::probe(&ffprobe, input.try_clone()?).map_err(invalid)
    })?;
    let streams = report["streams"].as_array().expect("validated probe");
    let video = streams
        .iter()
        .find(|stream| {
            stream["codec_type"] == "video" && stream["disposition"]["attached_pic"] != 1
        })
        .ok_or_else(|| invalid("No motion video stream found in the selected recording"))?;
    let duration = duration_seconds(&video["duration"])
        .or_else(|| duration_seconds(&report["format"]["duration"]))
        .ok_or_else(|| invalid("Source video duration is unavailable"))?;
    let transcript = selection
        .transcript
        .as_ref()
        .map(|selected| selected.resolve(&asset))
        .transpose()?;
    let audio = if let Some(link) = &transcript {
        if !streams
            .iter()
            .any(|stream| stream["codec_type"] == "audio" && stream["index"] == link.audio_stream)
        {
            return Err(invalid("Linked audio stream is absent from the recording"));
        }
        Some(link.audio_stream)
    } else {
        default_audio(streams).map(stream_index).transpose()?
    };
    let mut options = Vec::with_capacity(selection.clips.len());
    let video_stream = stream_index(video)?;
    for clip in selection.clips {
        if !valid_mp4_path(&clip.path)
            || !valid_clip_range(clip.start_ms, clip.end_ms)
            || (clip.end_ms as f64 / 1000.0) > duration
        {
            return Err(invalid(
                "Select a safe .mp4 path and a 100 ms to 10 minute range within the source video",
            ));
        }
        let directory = clip
            .path
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."))
            .to_owned();
        let name = clip
            .path
            .file_name()
            .and_then(|name| name.to_str())
            .expect("validated output");
        options.push((
            directory,
            Output::Clip(
                name.into(),
                ClipRecipe {
                    recipe_version: 1,
                    start_ms: clip.start_ms,
                    end_ms: clip.end_ms,
                    video_stream,
                    audio_stream: audio,
                    ffmpeg_sha256: ffmpeg_sha256.clone(),
                    ffprobe_sha256: ffprobe_sha256.clone(),
                    transcript: transcript.clone(),
                },
            ),
        ));
    }
    let scan = crate::Scan {
        schema_version: crate::SCHEMA_VERSION,
        root,
        assets: vec![asset; options.len()],
    };
    let mut options = options.into_iter();
    build_plan(scan, destination, |_| {
        Ok(options.next().expect("one recipe per selection"))
    })
}

fn parse_marker(line: &str) -> Option<(f64, &str)> {
    fn digit_count(text: &str, min: usize, max: usize) -> bool {
        (min..=max).contains(&text.len()) && text.bytes().all(|c| c.is_ascii_digit())
    }
    let (mut clock, mut label) = line.split_once(',')?;
    let (mut fraction, mut decimal) = ("", false);
    if let Some((whole, digits)) = clock.split_once('.') {
        clock = whole;
        fraction = digits.trim_end();
        decimal = true;
    } else if let Some((digits, rest)) = label.split_once(',')
        && digit_count(digits, 1, 3)
    {
        fraction = digits;
        label = rest;
        decimal = true;
    }
    let parts: Vec<_> = clock.trim().split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [minutes, seconds] if digit_count(minutes, 1, 2) => (0, *minutes, *seconds),
        [hours, minutes, seconds] if digit_count(hours, 1, 2) && digit_count(minutes, 2, 2) => {
            (hours.parse::<u32>().ok()?, *minutes, *seconds)
        }
        _ => return None,
    };
    if !digit_count(seconds, 2, 2) || (decimal && !digit_count(fraction, 1, 3)) {
        return None;
    }
    let minutes = minutes.parse::<u32>().ok()?;
    let seconds = seconds.parse::<u32>().ok()?;
    let label = label.trim();
    if minutes >= 60 || seconds >= 60 || label.is_empty() || label.chars().any(char::is_control) {
        return None;
    }
    let subsecond = fraction.parse::<u32>().unwrap_or_default() as f64
        / f64::from(10_u32.pow(fraction.len() as u32));
    Some((
        f64::from(hours * 3600 + minutes * 60 + seconds) + subsecond,
        label,
    ))
}

fn topic_end(segments: &[WhisperSegment], first: usize) -> usize {
    const TRANSITIONS: [&str; 8] = [
        "anyway", "but ", "next", "now ", "okay", "so ", "then ", "well ",
    ];
    let start = segments[first].start;
    for (index, pair) in segments[first..].windows(2).enumerate() {
        let [current, next] = pair else {
            unreachable!()
        };
        let duration = current.end - start;
        let ending = current
            .text
            .trim_end_matches(['"', '\''])
            .ends_with(['.', '?', '!']);
        let next_text = next.text.to_lowercase();
        let transition = TRANSITIONS.iter().any(|start| next_text.starts_with(start));
        if duration >= 90.0
            || (duration >= 10.0 && (next.start - current.end >= 2.25 || ending || transition))
        {
            return index + first;
        }
    }
    segments.len() - 1
}

pub fn align_notes(notes: &Path, transcript: &Path) -> io::Result<(serde_json::Value, bool)> {
    let (saved, transcript_sha256) = read_transcript(transcript)?;
    let bytes = read_limited(notes, 64 * 1024)?;
    let notes_sha256 = format!("{:x}", Sha256::digest(&bytes));
    let notes = std::str::from_utf8(&bytes)
        .map_err(|_| invalid("Rough notes must be UTF-8"))?
        .trim_start_matches('\u{feff}');
    let segments = &saved.transcript.segments;
    let duration = saved.transcript.samples as f64 / SAMPLE_RATE as f64;
    let mut proposals = Vec::new();
    let mut complete = true;
    for (index, line) in notes
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
    {
        let line_number = index + 1;
        let Some((seconds, label)) = parse_marker(line) else {
            complete = false;
            proposals.push(serde_json::json!({"line": line_number, "status": "invalid_marker"}));
            continue;
        };
        let Some(first) = segments.iter().position(|segment| segment.end >= seconds) else {
            complete = false;
            proposals.push(serde_json::json!({
                "line": line_number, "status": "unmatched", "label": label, "rough_seconds": seconds
            }));
            continue;
        };
        let last = topic_end(segments, first);
        let start = (segments[first].start - 1.5).floor().max(0.0);
        let end = (segments[last].end.min(segments[first].start + 90.0) + 2.0)
            .ceil()
            .min(duration);
        proposals.push(serde_json::json!({
            "line": line_number, "status": "review", "label": label, "rough_seconds": seconds,
            "start_seconds": start, "end_seconds": end, "first_segment": first, "last_segment": last,
        }));
    }
    if proposals.is_empty() {
        return Err(invalid("Select at least one rough marker"));
    }
    Ok((
        serde_json::json!({
            "schema_version": 1,
            "recipe_version": 1,
            "notes_sha256": notes_sha256,
            "transcript_sha256": transcript_sha256,
            "audio_sha256": saved.source.sha256,
            "timing": "approximate_audio_seconds",
            "proposals": proposals
        }),
        complete,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_whisper_timing_and_word_bounds() {
        let mut report = WhisperReport {
            whisper_version: Some("20250625".into()),
            torch_version: Some("2.12.0".into()),
            faster_whisper_version: None,
            ctranslate2_version: None,
            numpy_version: "2.4.6".into(),
            language: "en".into(),
            samples: SAMPLE_RATE,
            segments: vec![
                WhisperSegment {
                    start: 0.0,
                    end: 0.4,
                    text: "Hello".into(),
                    words: vec![WhisperWord {
                        start: 0.1,
                        end: 0.5,
                        word: "Hello".into(),
                        probability: 0.9,
                    }],
                },
                WhisperSegment {
                    start: 0.6,
                    end: 0.9,
                    text: "world".into(),
                    words: vec![WhisperWord {
                        start: 0.5 - f64::EPSILON,
                        end: 0.8,
                        word: "world".into(),
                        probability: 0.9,
                    }],
                },
            ],
        };
        let backend = WhisperBackend::OpenAi;
        assert!(report.validate("en", backend).is_ok());
        assert!(report.validate("de", backend).is_err());
        assert!(
            report
                .validate("en", WhisperBackend::FasterWhisperNarration)
                .is_err()
        );
        report.segments[1].words[0].start = 0.49;
        assert!(report.validate("auto", backend).is_err());
        report.segments[1].words[0].start = 0.5;
        report.segments[1].words[0].end = 0.55;
        assert!(report.validate("auto", backend).is_err());
        report.segments[1].words[0].end = 1.03;
        assert!(report.validate("auto", backend).is_err());
        report.segments[1].words[0].end = 0.8;
        report.segments[1].start = 0.39;
        assert!(report.validate("auto", backend).is_err());
    }

    #[test]
    fn parses_legacy_rough_notes_and_fractional_times() {
        assert_eq!(
            parse_marker("12:30, A title, with a comma"),
            Some((750.0, "A title, with a comma"))
        );
        assert_eq!(parse_marker("1:02:03,250, Title"), Some((3723.25, "Title")));
        assert_eq!(parse_marker("0:02.2, Title"), Some((2.2, "Title")));
        assert!(parse_marker("2:60, Invalid clock").is_none());
        assert!(parse_marker("2:00., Invalid fraction").is_none());
    }
}
