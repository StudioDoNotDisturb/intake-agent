use crate::{
    Asset, Output, Plan, SCHEMA_VERSION, build_plan, inspect_selection, invalid,
    organization::date_parts, scan,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
#[cfg(any(windows, target_os = "linux"))]
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::File,
    io::{self, Read},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant, UNIX_EPOCH},
};

const PROBE_LIMIT: u64 = 256 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(30);
pub(crate) const CONTAINERS: &str = "mov,matroska,avi,mpegts,mpeg,flv,asf,ogg,mxf,mp3,wav,flac,aac";

#[derive(Deserialize, Serialize)]
pub struct VideoInventory {
    schema_version: u32,
    root: PathBuf,
    clips: Vec<Clip>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    ffprobe_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    reused_probes: Option<usize>,
}

impl VideoInventory {
    pub fn complete(&self) -> bool {
        self.clips.iter().all(|clip| clip.probe_status == "ok")
    }
}

#[derive(Deserialize, Serialize)]
struct Clip {
    asset: Asset,
    probe_status: String,
    ffprobe: Option<Value>,
    audio_present: Option<bool>,
    dates: Vec<DateEvidence>,
    modified_unix_seconds: Option<u64>,
    capture_date: Option<String>,
    date_confidence: String,
    capture_date_conflict: bool,
    #[serde(default)]
    live_photo_evidence: LivePhotoEvidence,
}

#[derive(Default, Deserialize, Serialize)]
struct LivePhotoEvidence {
    content_identifier: Option<String>,
    mebx_stream: bool,
}

#[derive(Deserialize, Serialize)]
struct DateEvidence {
    source: String,
    raw: String,
    date: Option<String>,
}

pub fn inventory(root: &Path, selected: &[PathBuf], ffprobe: &Path) -> io::Result<VideoInventory> {
    let mut result = inventory_with(root, selected, ffprobe, &BTreeMap::new(), |_, _| Ok(()))?;
    result.reused_probes = None;
    Ok(result)
}

#[cfg(any(windows, target_os = "linux"))]
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ProbeRecord<T> {
    engine_sha256: String,
    source_sha256: String,
    report: T,
}

#[cfg(any(windows, target_os = "linux"))]
pub fn resume(
    root: &Path,
    selected: &[PathBuf],
    ffprobe: &Path,
    path: &Path,
) -> io::Result<VideoInventory> {
    check_inventory(selected, ffprobe)?;
    let canonical = crate::checked_root(root)?;
    let mut digest = Sha256::new();
    io::copy(&mut File::open(ffprobe)?, &mut digest)?;
    let engine = format!("{:x}", digest.finalize());
    let (mut file, records) = crate::execution::open_checkpoint::<ProbeRecord<Value>>(
        path,
        &canonical,
        b"{\"video_checkpoint\":1}\n",
    )?;
    let mut cached = BTreeMap::new();
    for record in records {
        if !valid_report(&record.report) {
            return Err(invalid("Invalid ffprobe report in checkpoint"));
        }
        if record.engine_sha256 == engine {
            cached.insert(record.source_sha256, record.report);
        }
    }
    let mut result = inventory_with(root, selected, ffprobe, &cached, |asset, report| {
        crate::execution::append_checkpoint(
            &mut file,
            &ProbeRecord {
                engine_sha256: engine.clone(),
                source_sha256: asset.sha256.clone(),
                report,
            },
        )
    })?;
    result.ffprobe_sha256 = Some(engine);
    Ok(result)
}

fn check_inventory(selected: &[PathBuf], ffprobe: &Path) -> io::Result<()> {
    if !ffprobe.is_absolute() {
        return Err(invalid(
            "INTAKE_FFPROBE must be an absolute executable path",
        ));
    }
    if !(1..=32).contains(&selected.len()) {
        return Err(invalid("Select 1 to 32 videos per inventory batch"));
    }
    Ok(())
}

fn inventory_with(
    root: &Path,
    selected: &[PathBuf],
    ffprobe: &Path,
    cached: &BTreeMap<String, Value>,
    mut save: impl FnMut(&Asset, &Value) -> io::Result<()>,
) -> io::Result<VideoInventory> {
    check_inventory(selected, ffprobe)?;
    let mut reused = 0;
    let (scan, evidence) = inspect_selection(
        root,
        selected,
        |file, metadata, sha256| {
            let previous = cached.get(sha256);
            let report = match previous {
                Some(report) => Ok(report.clone()),
                None => probe(ffprobe, file.try_clone()?),
            };
            let modified = metadata.modified()?.duration_since(UNIX_EPOCH).ok();
            Ok((
                report,
                modified.map(|value| value.as_secs()),
                previous.is_some(),
            ))
        },
        |asset, (report, _, reused_probe)| {
            if *reused_probe {
                reused += 1;
            } else if let Ok(report) = report {
                save(asset, report)?;
            }
            Ok(())
        },
    )?;
    Ok(VideoInventory {
        schema_version: scan.schema_version,
        root: scan.root,
        clips: scan
            .assets
            .into_iter()
            .zip(evidence)
            .map(|(asset, (report, modified, _))| clip_from_probe(asset, report, modified))
            .collect(),
        ffprobe_sha256: None,
        reused_probes: Some(reused),
    })
}

pub fn archive_plan(
    source: &Path,
    destination: &Path,
    inventory: &Path,
    selected: &[PathBuf],
) -> io::Result<Plan> {
    const INVENTORY_LIMIT: u64 = 32 * 1024 * 1024;
    let mut bytes = Vec::new();
    File::open(inventory)?
        .take(INVENTORY_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > INVENTORY_LIMIT || !(1..=32).contains(&selected.len()) {
        return Err(invalid(
            "Expected up to 32 selected videos and a report up to 32 MiB",
        ));
    }
    let report: VideoInventory = serde_json::from_slice(&bytes)?;
    let current = scan(source, selected)?;
    if report.schema_version != SCHEMA_VERSION
        || report.root != current.root
        || !(1..=32).contains(&report.clips.len())
    {
        return Err(invalid(
            "Video inventory version, root or batch size mismatch",
        ));
    }
    build_plan(current, destination, |asset| {
        let mut matches = report
            .clips
            .iter()
            .filter(|clip| clip.asset.sha256 == asset.sha256);
        let clip = matches
            .next()
            .filter(|clip| clip.asset == *asset)
            .ok_or_else(|| {
                invalid(format!(
                    "Inventory is stale or missing {}",
                    asset.relative_path.display()
                ))
            })?;
        if matches.next().is_some() {
            return Err(invalid("Content duplicates require individual review"));
        }
        let raw_report = clip
            .ffprobe
            .as_ref()
            .filter(|value| valid_report(value))
            .ok_or_else(|| invalid("Archive selection requires a valid ffprobe report"))?;
        let evidence = clip_from_probe(
            asset.clone(),
            Ok(raw_report.clone()),
            clip.modified_unix_seconds,
        );
        if evidence.live_photo_evidence.content_identifier.is_some()
            || evidence.live_photo_evidence.mebx_stream
        {
            return Err(invalid(format!(
                "Live Photo evidence requires separate review for {}",
                asset.relative_path.display()
            )));
        }
        let date = evidence
            .capture_date
            .as_deref()
            .filter(|_| {
                evidence.probe_status == "ok"
                    && !evidence.capture_date_conflict
                    && matches!(evidence.date_confidence.as_str(), "high" | "medium")
            })
            .ok_or_else(|| {
                invalid(format!(
                    "Capture date or probe needs review for {}",
                    asset.relative_path.display()
                ))
            })?;
        let [year, month, _] = date_parts(date)?;
        if !("1990"..="2100").contains(&year) {
            return Err(invalid("Archive capture year is outside 1990 to 2100"));
        }
        Ok((
            Path::new("originals/by-date")
                .join(year)
                .join(format!("{year}-{month}")),
            Output::Filing(None),
        ))
    })
}

pub(crate) fn probe(engine: &Path, file: File) -> Result<Value, &'static str> {
    let mut worker = Command::new(engine)
        .args([
            "-v",
            "error",
            "-max_alloc",
            "67108864",
            "-protocol_whitelist",
            "fd",
            "-format_whitelist",
            CONTAINERS,
            "-show_program_version",
            "-show_format",
            "-show_streams",
            "-of",
            "json",
            "-i",
            "fd:",
        ])
        .stdin(Stdio::from(file))
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "ffprobe_unavailable")?;
    let output = worker.stdout.take().expect("piped ffprobe output");
    let reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        output.take(PROBE_LIMIT + 1).read_to_end(&mut bytes)?;
        Ok::<_, io::Error>(bytes)
    });
    let deadline = Instant::now() + PROBE_TIMEOUT;
    let status = loop {
        match worker.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if Instant::now() < deadline => thread::sleep(Duration::from_millis(20)),
            other => {
                let _ = worker.kill();
                let _ = worker.wait();
                break Err(if other.is_ok() {
                    "ffprobe_timeout"
                } else {
                    "ffprobe_wait_failed"
                });
            }
        }
    };
    let output = reader
        .join()
        .map_err(|_| "ffprobe_output_failed")?
        .map_err(|_| "ffprobe_output_failed")?;
    let status = status?;
    if output.len() as u64 > PROBE_LIMIT {
        return Err("ffprobe_output_limit");
    }
    if !status.success() {
        return Err("ffprobe_failed");
    }
    let report: Value = serde_json::from_slice(&output).map_err(|_| "ffprobe_invalid_output")?;
    if !valid_report(&report) {
        return Err("ffprobe_invalid_output");
    }
    Ok(report)
}

fn valid_report(report: &Value) -> bool {
    report["program_version"]["version"].as_str().is_some()
        && report["format"]["format_name"].as_str().is_some()
        && report["streams"].as_array().is_some_and(|streams| {
            streams.iter().all(|stream| {
                stream["index"].as_u64().is_some() && stream["codec_type"].as_str().is_some()
            })
        })
}

fn clip_from_probe(
    asset: Asset,
    report: Result<Value, &'static str>,
    modified: Option<u64>,
) -> Clip {
    let mut dates = Vec::new();
    let mut live_photo_evidence = LivePhotoEvidence::default();
    let (probe_status, ffprobe, audio_present) = match report {
        Ok(report) => {
            let streams = report["streams"].as_array().expect("validated probe");
            live_photo_evidence = LivePhotoEvidence {
                content_identifier: std::iter::once(&report["format"])
                    .chain(streams)
                    .filter_map(|scope| scope["tags"].as_object())
                    .flat_map(|tags| tags.iter())
                    .find_map(|(key, value)| {
                        if key.eq_ignore_ascii_case("com.apple.quicktime.content.identifier") {
                            value
                                .as_str()
                                .filter(|id| !id.trim().is_empty())
                                .map(str::to_owned)
                        } else {
                            None
                        }
                    }),
                mebx_stream: streams
                    .iter()
                    .any(|stream| stream["codec_tag_string"] == "mebx"),
            };
            add_tags(&mut dates, "container", &report["format"]["tags"]);
            for stream in streams {
                add_tags(
                    &mut dates,
                    &format!("stream:{}", stream["index"]),
                    &stream["tags"],
                );
            }
            let video = streams.iter().any(|stream| {
                stream["codec_type"] == "video" && stream["disposition"]["attached_pic"] != 1
            });
            let audio = streams.iter().any(|stream| stream["codec_type"] == "audio");
            (
                if video { "ok" } else { "no_video_stream" },
                Some(report),
                Some(audio),
            )
        }
        Err(status) => (status, None, None),
    };
    let embedded = dates.iter().find_map(|date| date.date.clone());
    dates.extend(path_dates(&asset.relative_path));
    let first = dates.iter().find_map(|date| date.date.as_ref());
    let conflict = first.is_some_and(|first| {
        dates
            .iter()
            .filter_map(|date| date.date.as_ref())
            .any(|date| date != first)
    });
    let confidence = if conflict {
        "conflict"
    } else if embedded.is_some() {
        "high"
    } else if first.is_some() {
        "medium"
    } else if modified.is_some() {
        "low"
    } else {
        "unknown"
    };
    Clip {
        asset,
        probe_status: probe_status.into(),
        ffprobe,
        audio_present,
        capture_date: if conflict { None } else { first.cloned() },
        dates,
        modified_unix_seconds: modified,
        date_confidence: confidence.into(),
        capture_date_conflict: conflict,
        live_photo_evidence,
    }
}

fn add_tags(dates: &mut Vec<DateEvidence>, scope: &str, tags: &Value) {
    for field in ["creation_time", "com.apple.quicktime.creationdate", "date"] {
        if let Some(raw) = tags[field].as_str() {
            let date = raw.get(..10).filter(|date| {
                matches!(raw.as_bytes().get(10), None | Some(b'T' | b' '))
                    && date_parts(date).is_ok()
            });
            dates.push(DateEvidence {
                source: format!("{scope}:{field}"),
                raw: raw.to_owned(),
                date: date.map(str::to_owned),
            });
        }
    }
}

fn path_dates(path: &Path) -> impl Iterator<Item = DateEvidence> {
    let path = path.to_string_lossy().replace('\\', "/");
    [10, 8].into_iter().flat_map(move |length| {
        let path = path.clone();
        (0..path.len()).filter_map(move |start| {
            let end = start + length;
            let raw = path.get(start..end)?;
            let bytes = path.as_bytes();
            if start
                .checked_sub(1)
                .is_some_and(|i| bytes[i].is_ascii_digit())
                || bytes.get(end).is_some_and(u8::is_ascii_digit)
            {
                return None;
            }
            let date = match raw.as_bytes() {
                [_, _, _, _, a, _, _, b, _, _] if a == b && b"-_./".contains(a) => {
                    format!("{}-{}-{}", raw.get(..4)?, raw.get(5..7)?, raw.get(8..)?)
                }
                digits if digits.len() == 8 && digits.iter().all(u8::is_ascii_digit) => {
                    format!("{}-{}-{}", &raw[..4], &raw[4..6], &raw[6..])
                }
                _ => return None,
            };
            let [year, _, _] = date_parts(&date).ok()?;
            if !("1990"..="2100").contains(&year) {
                return None;
            }
            Some(DateEvidence {
                source: "path".into(),
                raw: raw.into(),
                date: Some(date),
            })
        })
    })
}
