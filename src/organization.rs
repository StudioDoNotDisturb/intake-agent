use crate::{
    Asset, Output, Plan, build_plan, caption_path, inspect_selection, invalid, portable_component,
    scan,
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde::{Deserialize, Serialize};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, Metadata},
    io::{self, Cursor, Read, Seek},
    path::{Path, PathBuf},
    time::{Duration, UNIX_EPOCH},
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Decision {
    file: PathBuf,
    date: String,
    category: Option<String>,
    name: Option<String>,
    expected_sha256: Option<String>,
}

pub(crate) fn date_parts(date: &str) -> io::Result<[&str; 3]> {
    let bad_date = || invalid("Expected a valid Gregorian filing date in YYYY-MM-DD format");
    let [year, month, day]: [&str; 3] = date
        .split('-')
        .collect::<Vec<_>>()
        .try_into()
        .map_err(|_| bad_date())?;
    if [year.len(), month.len(), day.len()] != [4, 2, 2]
        || ![year, month, day]
            .iter()
            .all(|part| part.bytes().all(|b| b.is_ascii_digit()))
    {
        return Err(bad_date());
    }
    let y: u32 = year.parse().map_err(|_| bad_date())?;
    let m: u32 = month.parse().map_err(|_| bad_date())?;
    let d: u32 = day.parse().map_err(|_| bad_date())?;
    let days = match m {
        4 | 6 | 9 | 11 => 30,
        2 if y.is_multiple_of(4) && (!y.is_multiple_of(100) || y.is_multiple_of(400)) => 29,
        2 => 28,
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        _ => 0,
    };
    if y == 0 || !(1..=days).contains(&d) {
        return Err(bad_date());
    }
    Ok([year, month, day])
}

fn filing_directory(date: &str, category: Option<&str>) -> io::Result<PathBuf> {
    let mut directory = PathBuf::new();
    if let Some(category) = category {
        if !portable_component(category) || category == "." || category == ".." {
            return Err(invalid("Expected a single safe category name"));
        }
        directory.push(category);
    }
    directory.extend(date_parts(date)?);
    Ok(directory)
}

pub fn organize(source: &Path, destination: &Path, decisions: &Path) -> io::Result<Plan> {
    let decisions: Vec<Decision> = read_decisions(decisions)?;
    let directories = decisions
        .iter()
        .map(|decision| filing_directory(&decision.date, decision.category.as_deref()))
        .collect::<io::Result<Vec<_>>>()?;
    let files: Vec<_> = decisions
        .iter()
        .map(|decision| decision.file.clone())
        .collect();
    let mut filing = directories.into_iter().zip(decisions);
    build_plan(scan(source, &files)?, destination, |asset| {
        let (directory, decision) = filing
            .next()
            .ok_or_else(|| invalid("Missing filing decision"))?;
        require_reviewed_hash(asset, decision.expected_sha256.as_deref())?;
        Ok((directory, Output::Filing(decision.name)))
    })
}

fn read_decisions<T: serde::de::DeserializeOwned>(path: &Path) -> io::Result<T> {
    const MAX_BYTES: u64 = 1024 * 1024;
    let mut data = Vec::new();
    File::open(path)?
        .take(MAX_BYTES + 1)
        .read_to_end(&mut data)?;
    if data.len() as u64 > MAX_BYTES {
        return Err(invalid("Organization decisions exceed 1 MiB"));
    }
    serde_json::from_slice(&data)
        .map_err(|error| invalid(format!("Invalid organization decisions: {error}")))
}

fn require_reviewed_hash(asset: &Asset, expected: Option<&str>) -> io::Result<()> {
    if expected.is_some_and(|hash| hash != asset.sha256) {
        return Err(invalid(format!(
            "Reviewed SHA-256 no longer matches {}",
            asset.relative_path.display()
        )));
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CaptionDecision {
    file: PathBuf,
    caption: String,
    expected_sha256: Option<String>,
}

pub fn caption_plan(root: &Path, decisions: &Path) -> io::Result<Plan> {
    let decisions: Vec<CaptionDecision> = read_decisions(decisions)?;
    let files: Vec<_> = decisions
        .iter()
        .map(|decision| decision.file.clone())
        .collect();
    let mut decisions = decisions.into_iter();
    build_plan(scan(root, &files)?, root, |asset| {
        let decision = decisions
            .next()
            .ok_or_else(|| invalid("Missing caption decision"))?;
        require_reviewed_hash(asset, decision.expected_sha256.as_deref())?;
        let mut directory = asset.relative_path.clone();
        directory.pop();
        if directory.as_os_str().is_empty() {
            directory.push(".");
        }
        Ok((directory, Output::Caption(decision.caption)))
    })
}

#[derive(Serialize)]
pub struct PhotoEvidence {
    schema_version: u32,
    root: PathBuf,
    photos: Vec<Photo>,
}

#[derive(Serialize)]
struct Photo {
    asset: Asset,
    evidence: DateEvidence,
}

#[derive(Serialize)]
struct DateEvidence {
    modified_unix_seconds: Option<u64>,
    exif_status: &'static str,
    dates: Vec<ExifDate>,
    filing_date: Option<String>,
    capture_date_conflict: bool,
}

#[derive(Serialize)]
struct ExifDate {
    field: &'static str,
    raw: Option<String>,
    offset: Option<String>,
    date: Option<String>,
}

pub fn photo_evidence(root: &Path, selected: &[PathBuf]) -> io::Result<PhotoEvidence> {
    let (scan, evidence) = inspect_selection(
        root,
        selected,
        |file, metadata, _| read_evidence(file, metadata),
        |_, _| Ok(()),
    )?;
    Ok(PhotoEvidence {
        schema_version: scan.schema_version,
        root: scan.root,
        photos: scan
            .assets
            .into_iter()
            .zip(evidence)
            .map(|(asset, evidence)| Photo { asset, evidence })
            .collect(),
    })
}

#[derive(Serialize)]
pub struct PhotoSuggestion {
    schema_version: u32,
    root: PathBuf,
    asset: Asset,
    model: String,
    installed_model_sha256: String,
    suggestion: Suggestion,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Suggestion {
    caption: String,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    category: Option<String>,
    #[serde(deserialize_with = "Deserialize::deserialize")]
    confidence: Option<f64>,
    conflicts: Vec<String>,
}

#[derive(Deserialize)]
struct ModelInventory {
    models: Vec<InstalledModel>,
}

#[derive(Deserialize)]
struct InstalledModel {
    model: String,
    digest: String,
    remote_model: Option<String>,
    remote_host: Option<String>,
}

#[derive(Deserialize)]
struct Generation {
    model: String,
    response: String,
    done: bool,
    done_reason: String,
}

pub fn photo_suggest(root: &Path, file: &Path, model: &str) -> io::Result<PhotoSuggestion> {
    photo_suggest_at(root, file, model, 11434, Duration::from_secs(90))
}

pub(crate) fn photo_suggest_at(
    root: &Path,
    file: &Path,
    model: &str,
    port: u16,
    generate_timeout: Duration,
) -> io::Result<PhotoSuggestion> {
    const MAX_IMAGE: u64 = 16 * 1024 * 1024;
    let valid_part = |part: &str| {
        !part.is_empty()
            && part
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte))
    };
    if model.len() > 160
        || !model.split_once(':').is_some_and(|(name, tag)| {
            valid_part(name) && valid_part(tag) && !tag.to_ascii_lowercase().contains("cloud")
        })
    {
        return Err(invalid(
            "Expected an installed local Ollama model in name:tag form",
        ));
    }
    let (mut scan, mut images) = inspect_selection(
        root,
        &[file.to_owned()],
        |file, metadata, _| {
            if metadata.len() > MAX_IMAGE {
                return Err(invalid("Ollama photo input exceeds 16 MiB"));
            }
            let mut bytes = Vec::new();
            file.take(MAX_IMAGE + 1).read_to_end(&mut bytes)?;
            file.rewind()?;
            if bytes.len() as u64 > MAX_IMAGE
                || !(bytes.starts_with(b"\x89PNG\r\n\x1a\n") || bytes.starts_with(b"\xff\xd8\xff"))
            {
                return Err(invalid("Expected a PNG or JPEG photo up to 16 MiB"));
            }
            Ok(bytes)
        },
        |_, _| Ok(()),
    )?;
    let asset = scan.assets.remove(0);
    let bytes = images.remove(0);
    if format!("{:x}", Sha256::digest(&bytes)) != asset.sha256 {
        return Err(invalid("Photo changed while preparing Ollama input"));
    }

    let url = format!("http://127.0.0.1:{port}/api");
    let agent = |timeout| {
        ureq::Agent::config_builder()
            .proxy(None)
            .max_redirects(0)
            .max_redirects_will_error(true)
            .timeout_global(Some(timeout))
            .build()
            .new_agent()
    };
    let control = agent(Duration::from_secs(5));
    let installed_digest = || {
        let body = control
            .get(format!("{url}/tags"))
            .call()
            .and_then(|mut response| {
                response
                    .body_mut()
                    .with_config()
                    .limit(1024 * 1024)
                    .read_to_vec()
            })
            .map_err(|error| io::Error::other(format!("Ollama model inventory failed: {error}")))?;
        let inventory: ModelInventory =
            serde_json::from_slice(&body).map_err(|_| invalid("Invalid Ollama model inventory"))?;
        let entry = inventory
            .models
            .into_iter()
            .find(|entry| entry.model == model)
            .ok_or_else(|| invalid("Requested model is missing from the local Ollama inventory"))?;
        if entry.remote_model.is_some_and(|value| !value.is_empty())
            || entry.remote_host.is_some_and(|value| !value.is_empty())
            || entry.digest.len() != 64
            || !entry.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(invalid(
                "Expected a local Ollama model with a SHA-256 digest",
            ));
        }
        Ok(entry.digest)
    };
    let digest = installed_digest()?;
    let schema = json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "caption": {"type": "string"},
            "category": {"type": ["string", "null"]},
            "confidence": {"type": ["number", "null"], "minimum": 0, "maximum": 1},
            "conflicts": {"type": "array", "items": {"type": "string"}}
        },
        "required": ["caption", "category", "confidence", "conflicts"]
    });
    let prompt = format!(
        "Describe only visible image content. Suggest one short filing category or null. \
         Report your confidence from 0 to 1 or null and list ambiguities in conflicts. \
         Return JSON matching this schema: {schema}"
    );
    let request = serde_json::to_vec(&json!({
        "model": model, "prompt": prompt, "images": [STANDARD.encode(bytes)],
        "stream": false, "format": schema, "options": {"temperature": 0, "num_predict": 512}
    }))?;
    let body = agent(generate_timeout)
        .post(format!("{url}/generate"))
        .header("Content-Type", "application/json")
        .send(request.as_slice())
        .and_then(|mut response| {
            response
                .body_mut()
                .with_config()
                .limit(64 * 1024)
                .read_to_vec()
        })
        .map_err(|error| io::Error::other(format!("Ollama generation failed: {error}")))?;
    let response: Generation =
        serde_json::from_slice(&body).map_err(|_| invalid("Invalid Ollama generation response"))?;
    if !response.done || response.done_reason != "stop" || response.model != model {
        return Err(invalid(
            "Ollama did not finish the requested model's generation",
        ));
    }
    let suggestion: Suggestion = serde_json::from_str(&response.response)
        .map_err(|_| invalid("Invalid Ollama suggestion fields"))?;
    caption_path(&asset.relative_path, &suggestion.caption)?;
    if suggestion.category.as_deref().is_some_and(|category| {
        category.len() > 64 || !portable_component(category) || matches!(category, "." | "..")
    }) || suggestion
        .confidence
        .is_some_and(|value| !(0.0..=1.0).contains(&value))
        || suggestion.conflicts.len() > 8
        || suggestion.conflicts.iter().any(|conflict| {
            conflict.trim().is_empty()
                || conflict.len() > 256
                || conflict.chars().any(char::is_control)
        })
    {
        return Err(invalid(
            "Ollama suggestion exceeds organization field limits",
        ));
    }
    if installed_digest()? != digest {
        return Err(invalid("Installed Ollama model changed during generation"));
    }
    Ok(PhotoSuggestion {
        schema_version: scan.schema_version,
        root: scan.root,
        asset,
        model: model.to_owned(),
        installed_model_sha256: digest,
        suggestion,
    })
}

fn read_evidence(file: &mut File, metadata: &Metadata) -> io::Result<DateEvidence> {
    const MAX_EXIF_INPUT: u64 = 32 * 1024 * 1024;
    let mut report = DateEvidence {
        modified_unix_seconds: metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs()),
        exif_status: "size_limit",
        dates: Vec::new(),
        filing_date: None,
        capture_date_conflict: false,
    };
    if metadata.len() > MAX_EXIF_INPUT {
        return Ok(report);
    }
    let mut bytes = Vec::new();
    file.take(MAX_EXIF_INPUT + 1).read_to_end(&mut bytes)?;
    file.rewind()?;
    if bytes.len() as u64 > MAX_EXIF_INPUT {
        return Ok(report);
    }
    let exif = match exif::Reader::new().read_from_container(&mut Cursor::new(bytes)) {
        Ok(exif) => exif,
        Err(exif::Error::NotFound(_)) => {
            report.exif_status = "absent";
            return Ok(report);
        }
        Err(_) => {
            report.exif_status = "unreadable";
            return Ok(report);
        }
    };
    report.exif_status = "read";
    let ascii = |tag| {
        let field = exif.get_field(tag, exif::In::PRIMARY)?;
        match &field.value {
            exif::Value::Ascii(values) if values.len() == 1 => {
                String::from_utf8(values[0].clone()).ok()
            }
            _ => None,
        }
    };
    for (field, tag, offset) in [
        (
            "original",
            exif::Tag::DateTimeOriginal,
            exif::Tag::OffsetTimeOriginal,
        ),
        (
            "digitized",
            exif::Tag::DateTimeDigitized,
            exif::Tag::OffsetTimeDigitized,
        ),
        ("image_modified", exif::Tag::DateTime, exif::Tag::OffsetTime),
    ] {
        if exif.get_field(tag, exif::In::PRIMARY).is_some() {
            let raw = ascii(tag);
            let date = raw.as_deref().and_then(exif_date);
            report.dates.push(ExifDate {
                field,
                raw,
                offset: ascii(offset),
                date,
            });
        }
    }
    let date_for = |field| {
        report
            .dates
            .iter()
            .find(|e| e.field == field)
            .and_then(|e| e.date.as_deref())
    };
    let original = date_for("original");
    let digitized = date_for("digitized");
    report.capture_date_conflict = matches!((original, digitized), (Some(a), Some(b)) if a != b);
    if !report.capture_date_conflict {
        report.filing_date = original.map(str::to_owned);
    }
    Ok(report)
}

fn exif_date(raw: &str) -> Option<String> {
    let timestamp = exif::DateTime::from_ascii(raw.as_bytes()).ok()?;
    if raw.len() != 19 || timestamp.hour > 23 || timestamp.minute > 59 || timestamp.second > 60 {
        return None;
    }
    let date = format!(
        "{:04}-{:02}-{:02}",
        timestamp.year, timestamp.month, timestamp.day
    );
    date_parts(&date).ok()?;
    Some(date)
}

#[derive(Serialize)]
pub struct Duplicates {
    schema_version: u32,
    root: PathBuf,
    groups: Vec<Vec<Asset>>,
}

pub fn duplicates(root: &Path, selected: &[PathBuf]) -> io::Result<Duplicates> {
    let scan = scan(root, selected)?;
    let mut by_hash: BTreeMap<_, Vec<_>> = BTreeMap::new();
    for asset in scan.assets {
        let key = (asset.bytes, asset.sha256.clone());
        by_hash.entry(key).or_default().push(asset);
    }
    Ok(Duplicates {
        schema_version: scan.schema_version,
        root: scan.root,
        groups: by_hash
            .into_values()
            .filter(|group| group.len() > 1)
            .collect(),
    })
}
