use crate::{Asset, Output, Plan, SCHEMA_VERSION, Scan, build_plan, checked_root, inspect_asset};
use crate::{invalid, validate_relative, validate_root_pair};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    path::{Path, PathBuf},
};
use unicode_normalization::UnicodeNormalization;

#[derive(Deserialize)]
struct Edition {
    book_id: String,
    title: String,
    author: String,
    chapter_count: usize,
    total_bytes: u64,
    chapters: Vec<Chapter>,
    #[serde(default)]
    edition: String,
    provenance: Option<EditionProvenance>,
}

#[derive(Deserialize)]
struct EditionProvenance {
    edition: Option<String>,
}

#[derive(Deserialize, Eq, PartialEq, Serialize)]
struct Chapter {
    #[serde(alias = "number")]
    chapter: usize,
    file: String,
    bytes: u64,
    sha256: String,
}

#[derive(Serialize)]
pub struct ImportReview {
    schema_version: u32,
    book_id: String,
    title: String,
    author: String,
    metadata_sha256: String,
    chapter_count: usize,
    #[serde(flatten)]
    copies: Copies,
}

#[derive(Serialize)]
struct Copies {
    already_present: Vec<PathBuf>,
    plan: Option<Plan>,
}

pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    root: &Path,
    path: &Path,
) -> io::Result<(Asset, T)> {
    inspect_asset(root, path, &mut |file, info, _| {
        if info.len() > 8 * 1024 * 1024 {
            return Err(invalid(format!(
                "JSON record exceeds 8 MiB: {}",
                path.display()
            )));
        }
        serde_json::from_reader(file).map_err(io::Error::from)
    })
}

fn require_content(asset: &Asset, bytes: u64, sha256: &str) -> io::Result<()> {
    if asset.bytes != bytes || asset.sha256 != sha256 {
        return Err(invalid(format!(
            "Content does not match the import: {}",
            asset.relative_path.display()
        )));
    }
    Ok(())
}

fn checked_edition(source: &Path) -> io::Result<(Asset, Edition)> {
    let (metadata, edition) = read_json::<Edition>(source, Path::new("metadata.json"))?;
    if edition.book_id.trim().is_empty()
        || edition.title.trim().is_empty()
        || edition.author.trim().is_empty()
        || edition.chapters.is_empty()
        || edition.chapter_count != edition.chapters.len()
        || edition
            .chapters
            .iter()
            .try_fold(0u64, |sum, chapter| sum.checked_add(chapter.bytes))
            != Some(edition.total_bytes)
    {
        return Err(invalid(
            "Invalid audiobook metadata identity or chapter totals",
        ));
    }
    let mut paths = BTreeSet::new();
    for (index, chapter) in edition.chapters.iter().enumerate() {
        let path = PathBuf::from(chapter.file.replace('\\', "/"));
        validate_relative(&path)?;
        if chapter.chapter != index + 1
            || chapter.bytes == 0
            || path.parent() != Some(Path::new("chapters"))
            || !path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("mp3"))
            || !paths.insert(chapter.file.replace('\\', "/").to_lowercase())
        {
            return Err(invalid(format!(
                "Invalid chapter entry: {}",
                chapter.chapter
            )));
        }
    }
    Ok((metadata, edition))
}

fn chapter_asset(source: &Path, chapter: &Chapter) -> io::Result<Asset> {
    let path = PathBuf::from(chapter.file.replace('\\', "/"));
    let (asset, ()) = inspect_asset(source, &path, &mut |_, _, _| Ok(()))?;
    require_content(&asset, chapter.bytes, &chapter.sha256)?;
    Ok(asset)
}

/// Plan copies for one selected legacy audiobook directory. Preserve its metadata verbatim.
pub fn import_plan(source: &Path, destination: &Path) -> io::Result<ImportReview> {
    let source = checked_root(source)?;
    let (metadata, edition) = checked_edition(&source)?;
    let mut assets = edition
        .chapters
        .iter()
        .map(|chapter| chapter_asset(&source, chapter))
        .collect::<io::Result<Vec<_>>>()?;
    let metadata_sha256 = metadata.sha256.clone();
    assets.push(metadata);
    let copies = plan_copies(source, destination, assets, false)?;
    Ok(ImportReview {
        schema_version: SCHEMA_VERSION,
        book_id: edition.book_id,
        title: edition.title,
        author: edition.author,
        metadata_sha256,
        chapter_count: edition.chapter_count,
        copies,
    })
}

/// Export explicitly selected chapters into an existing listening folder.
pub fn listening_plan(book: &Path, destination: &Path, selected: &[usize]) -> io::Result<Value> {
    let book = checked_root(book)?;
    let (metadata, edition) = checked_edition(&book)?;
    let numbers: BTreeSet<_> = selected.iter().copied().collect();
    if selected.is_empty()
        || selected.len() != numbers.len()
        || numbers.contains(&0)
        || numbers.last() > Some(&edition.chapter_count)
    {
        return Err(invalid("Select unique chapter numbers from the edition"));
    }
    let chapters: Vec<_> = edition
        .chapters
        .iter()
        .filter(|chapter| numbers.contains(&chapter.chapter))
        .collect();
    let assets = chapters
        .iter()
        .map(|chapter| chapter_asset(&book, chapter))
        .collect::<io::Result<Vec<_>>>()?;
    let copies = plan_copies(book, destination, assets, true)?;
    Ok(json!({
        "schema_version": SCHEMA_VERSION, "book_id": edition.book_id,
        "title": edition.title, "author": edition.author,
        "metadata_sha256": metadata.sha256, "chapters": chapters,
        "already_present": copies.already_present, "plan": copies.plan
    }))
}

fn plan_copies(
    source: PathBuf,
    destination: &Path,
    assets: Vec<Asset>,
    flat: bool,
) -> io::Result<Copies> {
    let target = checked_root(destination)?;
    if validate_root_pair(&source, &target)? {
        return Err(invalid("An import requires a separate destination root"));
    }
    let mut already_present = Vec::new();
    let mut missing = Vec::new();
    for asset in assets {
        let path = if flat {
            Path::new(
                asset
                    .relative_path
                    .file_name()
                    .expect("validated import path"),
            )
        } else {
            &asset.relative_path
        };
        match inspect_asset(&target, path, &mut |_, _, _| Ok(())) {
            Ok((existing, ())) => {
                require_content(&existing, asset.bytes, &asset.sha256)?;
                already_present.push(path.to_path_buf());
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => missing.push(asset),
            Err(error) => return Err(error),
        }
    }
    let plan = if missing.is_empty() {
        None
    } else {
        let scan = Scan {
            schema_version: SCHEMA_VERSION,
            root: source,
            assets: missing,
        };
        Some(build_plan(scan, destination, |asset| {
            let parent = asset.relative_path.parent().expect("validated import path");
            let directory = if flat || parent.as_os_str().is_empty() {
                Path::new(".")
            } else {
                parent
            };
            Ok((directory.to_path_buf(), Output::Filing(None)))
        })?)
    };
    Ok(Copies {
        already_present,
        plan,
    })
}

#[derive(Deserialize, Eq, PartialEq)]
struct BookSnapshot {
    book_id: String,
    title: String,
    author: String,
    audio_edition: String,
}

impl Edition {
    fn snapshot(&self) -> BookSnapshot {
        BookSnapshot {
            book_id: self.book_id.clone(),
            title: self.title.clone(),
            author: self.author.clone(),
            audio_edition: self
                .provenance
                .as_ref()
                .and_then(|p| p.edition.as_ref())
                .unwrap_or(&self.edition)
                .clone(),
        }
    }
}

fn book_snapshot(book: &Path) -> io::Result<BookSnapshot> {
    let root = checked_root(book)?;
    let (_, edition) = read_json::<Edition>(&root, Path::new("metadata.json"))?;
    Ok(edition.snapshot())
}

fn hash_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

#[derive(Deserialize)]
struct TextCatalog {
    schema_version: u32,
    book_id: String,
    book_snapshot: BookSnapshot,
    canonical_text_id: Option<String>,
    assets: Vec<TextPointer>,
}

#[derive(Deserialize)]
struct TextPointer {
    text_id: String,
    record: String,
    record_sha256: String,
    normalized_file: String,
    normalized_sha256: String,
    edition_relation: String,
}

#[derive(Deserialize)]
struct TextRecord {
    schema_version: u32,
    text_id: String,
    book_id: String,
    book_snapshot: BookSnapshot,
    asset_identity: Value,
    source: TextFile,
    normalized: TextFile,
}

#[derive(Deserialize)]
struct TextFile {
    path: String,
    bytes: u64,
    sha256: String,
}

fn read_text_catalog(root: &Path, book: &BookSnapshot) -> io::Result<(Asset, TextCatalog)> {
    let (asset, catalog) = read_json::<TextCatalog>(root, Path::new("catalog.json"))?;
    if catalog.schema_version != 1
        || catalog.book_id != book.book_id
        || catalog.book_snapshot != *book
    {
        return Err(invalid("Text catalog does not match the audiobook edition"));
    }
    let mut ids = BTreeSet::new();
    for pointer in &catalog.assets {
        if !hash_id(&pointer.text_id)
            || !ids.insert(&pointer.text_id)
            || !matches!(
                pointer.edition_relation.as_str(),
                "exact" | "alternate" | "unknown"
            )
        {
            return Err(invalid("Invalid or repeated canonical text asset identity"));
        }
    }
    if let Some(id) = &catalog.canonical_text_id
        && !catalog
            .assets
            .iter()
            .any(|asset| &asset.text_id == id && asset.edition_relation == "exact")
    {
        return Err(invalid("Canonical text is missing its exact-edition asset"));
    }
    Ok((asset, catalog))
}

fn read_text_asset(
    root: &Path,
    book: &BookSnapshot,
    pointer: &TextPointer,
) -> io::Result<([Asset; 3], String)> {
    let directory = format!("assets/{}", pointer.text_id);
    let record_path = format!("{directory}/asset.json");
    let normalized_path = format!("{directory}/normalized.txt");
    if pointer.record != record_path || pointer.normalized_file != normalized_path {
        return Err(invalid("Invalid catalog text asset paths"));
    }
    let (record_asset, record) = read_json::<TextRecord>(root, Path::new(&record_path))?;
    let identity = &record.asset_identity;
    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(identity)?));
    if record_asset.sha256 != pointer.record_sha256
        || digest != pointer.text_id
        || record.schema_version != 1
        || record.text_id != pointer.text_id
        || record.book_id != book.book_id
        || record.book_snapshot != *book
        || identity["book_id"] != book.book_id
        || identity["edition"]["relation_to_book"] != pointer.edition_relation
        || identity["source"]["sha256"] != record.source.sha256
        || identity["source"]["bytes"] != record.source.bytes
        || record.source.path != "raw/source.txt"
        || record.normalized.path != "normalized.txt"
        || record.normalized.sha256 != pointer.normalized_sha256
    {
        return Err(invalid(
            "Text asset identity or hash does not match the catalog",
        ));
    }
    let (raw, ()) = inspect_asset(
        root,
        Path::new(&directory).join("raw/source.txt").as_path(),
        &mut |_, _, _| Ok(()),
    )?;
    require_content(&raw, record.source.bytes, &record.source.sha256)?;
    let (normalized, text) = read_utf8(root, Path::new(&normalized_path))?;
    require_content(
        &normalized,
        record.normalized.bytes,
        &record.normalized.sha256,
    )?;
    Ok(([raw, normalized, record_asset], text))
}

pub fn text_import_plan(book: &Path, source: &Path, destination: &Path) -> io::Result<Value> {
    let book = book_snapshot(book)?;
    let source = checked_root(source)?;
    let (catalog_asset, catalog) = read_text_catalog(&source, &book)?;
    let mut assets = Vec::new();
    for pointer in &catalog.assets {
        assets.extend(read_text_asset(&source, &book, pointer)?.0);
    }
    let catalog_sha256 = catalog_asset.sha256.clone();
    assets.push(catalog_asset);
    let copies = plan_copies(source, destination, assets, false)?;
    Ok(
        json!({"schema_version": SCHEMA_VERSION, "book_id": book.book_id,
        "canonical_text_id": catalog.canonical_text_id, "catalog_sha256": catalog_sha256,
        "already_present": copies.already_present, "plan": copies.plan}),
    )
}

pub fn search_text(book: &Path, catalog: &Path, query: &str) -> io::Result<Value> {
    let query = query.trim().to_lowercase();
    if query.is_empty() || query.len() > 512 {
        return Err(invalid("Search expects a nonempty query up to 512 bytes"));
    }
    let book = book_snapshot(book)?;
    let root = checked_root(catalog)?;
    let (catalog_asset, catalog) = read_text_catalog(&root, &book)?;
    let pointer = catalog
        .assets
        .iter()
        .find(|p| Some(&p.text_id) == catalog.canonical_text_id.as_ref())
        .ok_or_else(|| invalid("This catalog has no selected canonical text"))?;
    let ([_, normalized, record], text) = read_text_asset(&root, &book, pointer)?;
    let hits: Vec<_> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| line.to_lowercase().contains(&query))
        .take(101)
        .map(|(index, line)| json!({"line": index + 1, "text": line}))
        .collect();
    Ok(
        json!({"schema_version": SCHEMA_VERSION, "kind": "canonical_text", "book_id": book.book_id,
        "text_id": pointer.text_id, "catalog_sha256": catalog_asset.sha256, "record_sha256": record.sha256,
        "path": normalized.relative_path, "sha256": normalized.sha256, "truncated": hits.len() > 100,
        "hits": &hits[..hits.len().min(100)]}),
    )
}

#[derive(Deserialize)]
struct TranscriptCatalog {
    schema_version: u32,
    book_id: String,
    book_snapshot: BookSnapshot,
    active_by_chapter: BTreeMap<String, String>,
    assets: Vec<TranscriptPointer>,
}

#[derive(Deserialize)]
struct TranscriptPointer {
    job_id: String,
    chapter: usize,
    record: String,
    record_sha256: String,
    transcript_file: String,
    transcript_sha256: String,
    segments_file: String,
    segments_sha256: String,
}

#[derive(Deserialize)]
struct TranscriptRecord {
    schema_version: u32,
    job_id: String,
    book_id: String,
    book_snapshot: BookSnapshot,
    asset_identity: Value,
    result: TranscriptResult,
}

#[derive(Deserialize)]
struct TranscriptResult {
    transcript_path: String,
    transcript_bytes: u64,
    transcript_sha256: String,
    segments_path: String,
    segments_bytes: u64,
    segments_sha256: String,
    segment_count: usize,
    detected_language: String,
    empty: bool,
    audio_duration_seconds: f64,
}

#[derive(Deserialize)]
struct TimedText {
    schema_version: u32,
    text: String,
    language: String,
    word_timestamps: bool,
    empty: bool,
    segments: Vec<TimedSegment>,
}

#[derive(Deserialize)]
struct TimedSegment {
    id: usize,
    start_ms: u64,
    end_ms: u64,
    text: String,
    words: Option<Vec<TimedWord>>,
}

#[derive(Deserialize)]
struct TimedWord {
    start_ms: u64,
    end_ms: u64,
    word: String,
    probability: Option<f64>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Correction {
    parent_job_id: String,
    parent_record_sha256: String,
    source_sha256: String,
    evidence: String,
    spans: Vec<CorrectionSpan>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CorrectionSpan {
    first: usize,
    last_exclusive: usize,
    source_start: usize,
    source_end: usize,
}

struct Transcript {
    record: TranscriptRecord,
    timing: TimedText,
    files: Vec<Asset>,
    correction: Option<Correction>,
}

fn read_utf8(root: &Path, path: &Path) -> io::Result<(Asset, String)> {
    use io::Read;
    inspect_asset(root, path, &mut |file, info, _| {
        if info.len() > 32 * 1024 * 1024 {
            return Err(invalid(format!("Text exceeds 32 MiB: {}", path.display())));
        }
        let mut text = String::new();
        file.read_to_string(&mut text)?;
        Ok(text)
    })
}

fn duration_ms(seconds: f64) -> u64 {
    (seconds * 1000.0).round_ties_even() as u64
}

impl TimedText {
    fn validate(&self, record: &TranscriptResult) -> io::Result<()> {
        let duration = duration_ms(record.audio_duration_seconds);
        if self.schema_version != 1
            || self.language != record.detected_language
            || self.empty != self.text.is_empty()
            || self.empty != record.empty
            || self.segments.len() != record.segment_count
            || record.audio_duration_seconds <= 0.0
            || !record.audio_duration_seconds.is_finite()
        {
            return Err(invalid("Transcript timing metadata is inconsistent"));
        }
        let mut previous_start = 0;
        for (index, item) in self.segments.iter().enumerate() {
            if item.id != index
                || item.start_ms < previous_start
                || item.start_ms > duration
                || item.end_ms < item.start_ms
                || item.end_ms > duration.saturating_add(2_000)
                || item.words.is_some() != self.word_timestamps
                || (self.empty && !item.text.is_empty())
            {
                return Err(invalid(format!("Invalid transcript segment {index}")));
            }
            let mut previous_end = item.start_ms;
            for word in item.words.iter().flatten() {
                if word.word.trim() != word.word
                    || word.start_ms < previous_end
                    || word.end_ms < word.start_ms
                    || word.end_ms > item.end_ms
                    || word.probability.is_some_and(|p| !(0.0..=1.0).contains(&p))
                {
                    return Err(invalid(format!("Invalid word timing in segment {index}")));
                }
                previous_end = word.end_ms;
            }
            previous_start = item.start_ms;
        }
        Ok(())
    }
}

fn read_transcript_asset(
    root: &Path,
    book: &BookSnapshot,
    pointer: &TranscriptPointer,
    chapter: &Chapter,
) -> io::Result<Transcript> {
    let directory = format!("assets/{}", pointer.job_id);
    let record_path = format!("{directory}/asset.json");
    let text_path = format!("{directory}/transcript.txt");
    let segments_path = format!("{directory}/segments.json");
    if pointer.record != record_path
        || pointer.transcript_file != text_path
        || pointer.segments_file != segments_path
    {
        return Err(invalid("Invalid transcript asset paths"));
    }
    let (record_asset, record) = read_json::<TranscriptRecord>(root, Path::new(&record_path))?;
    let identity = &record.asset_identity;
    let digest = format!("{:x}", Sha256::digest(serde_json::to_vec(identity)?));
    let source: Chapter = serde_json::from_value(identity["source"].clone())?;
    let result = &record.result;
    if record_asset.sha256 != pointer.record_sha256
        || digest != pointer.job_id
        || record.job_id != pointer.job_id
        || record.schema_version != 1
        || identity["schema_version"] != 1
        || record.book_id != book.book_id
        || identity["book_id"] != book.book_id
        || record.book_snapshot != *book
        || source != *chapter
        || result.transcript_path != "transcript.txt"
        || result.segments_path != "segments.json"
        || result.transcript_sha256 != pointer.transcript_sha256
        || result.segments_sha256 != pointer.segments_sha256
    {
        return Err(invalid(format!(
            "Transcript identity or chapter mismatch: {}",
            pointer.job_id
        )));
    }
    let (text_asset, text) = read_utf8(root, Path::new(&text_path))?;
    let (timing_asset, timing) = read_json::<TimedText>(root, Path::new(&segments_path))?;
    require_content(
        &text_asset,
        result.transcript_bytes,
        &result.transcript_sha256,
    )?;
    require_content(
        &timing_asset,
        result.segments_bytes,
        &result.segments_sha256,
    )?;
    timing.validate(result)?;
    if text.strip_suffix('\n') != Some(timing.text.as_str())
        || timing.text.contains('\r')
        || identity["recipe"]["word_timestamps"] != timing.word_timestamps
    {
        return Err(invalid("Transcript text and timings disagree"));
    }
    let correction = identity["recipe"]
        .get("reviewed_correction")
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?;
    Ok(Transcript {
        record,
        timing,
        files: vec![text_asset, timing_asset, record_asset],
        correction,
    })
}

fn verify_correction(
    root: &Path,
    child: &Transcript,
    correction: &Correction,
    parent: &Transcript,
) -> io::Result<Asset> {
    let mut child_recipe = child.record.asset_identity["recipe"].clone();
    child_recipe
        .as_object_mut()
        .expect("recipe has a correction")
        .remove("reviewed_correction");
    if parent.correction.is_some()
        || parent.timing.word_timestamps
        || child.timing.word_timestamps
        || correction.evidence.trim().is_empty()
        || correction.spans.is_empty()
        || parent.files[2].sha256 != correction.parent_record_sha256
        || child.record.asset_identity["source"] != parent.record.asset_identity["source"]
        || child_recipe != parent.record.asset_identity["recipe"]
        || child.timing.language != parent.timing.language
        || child.record.result.audio_duration_seconds != parent.record.result.audio_duration_seconds
    {
        return Err(invalid(
            "Correction parent identity or timing does not match",
        ));
    }
    let source_path = format!("assets/{}/correction-source.txt", child.record.job_id);
    let (source_asset, source) = read_utf8(root, Path::new(&source_path))?;
    if source_asset.sha256 != correction.source_sha256 {
        return Err(invalid(
            "Reviewed spoken source hash does not match the correction",
        ));
    }
    let characters: Vec<char> = source.chars().collect();
    let segments = &parent.timing.segments;
    let mut rebuilt = Vec::new();
    let mut cursor = 0;
    let mut source_cursor = 0;
    for span in &correction.spans {
        let CorrectionSpan {
            first,
            last_exclusive: last,
            source_start: start,
            source_end: end,
        } = *span;
        if first < cursor
            || first >= last
            || last > segments.len()
            || start < source_cursor
            || start >= end
            || end > characters.len()
        {
            return Err(invalid(
                "Reviewed correction spans overlap or exceed the source",
            ));
        }
        rebuilt.extend(
            segments[cursor..first]
                .iter()
                .map(|s| (s.start_ms, s.end_ms, s.text.clone())),
        );
        let replacement: String = characters[start..end].iter().collect();
        rebuilt.push((
            segments[first].start_ms,
            segments[last - 1].end_ms,
            replacement.trim().nfc().collect(),
        ));
        cursor = last;
        source_cursor = end;
    }
    rebuilt.extend(
        segments[cursor..]
            .iter()
            .map(|s| (s.start_ms, s.end_ms, s.text.clone())),
    );
    let rebuilt_text = rebuilt
        .iter()
        .map(|(_, _, text)| text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if rebuilt_text.trim().nfc().ne(child.timing.text.chars())
        || rebuilt.into_iter().ne(child
            .timing
            .segments
            .iter()
            .map(|s| (s.start_ms, s.end_ms, s.text.clone())))
    {
        return Err(invalid(
            "Reviewed correction does not reproduce the stored text and timings",
        ));
    }
    Ok(source_asset)
}

struct Transcripts {
    catalog_asset: Asset,
    catalog: TranscriptCatalog,
    transcripts: BTreeMap<String, Transcript>,
    audio: BTreeMap<usize, Asset>,
    corrections: BTreeMap<String, Asset>,
}

fn read_transcripts(book: &Path, root: &Path) -> io::Result<Transcripts> {
    let book = checked_root(book)?;
    let (_, edition) = read_json::<Edition>(&book, Path::new("metadata.json"))?;
    let book_snapshot = edition.snapshot();
    let (catalog_asset, catalog) = read_json::<TranscriptCatalog>(root, Path::new("catalog.json"))?;
    if catalog.schema_version != 1
        || catalog.book_id != book_snapshot.book_id
        || catalog.book_snapshot != book_snapshot
    {
        return Err(invalid(
            "Transcript catalog does not match the audiobook edition",
        ));
    }
    let chapters: BTreeMap<_, _> = edition
        .chapters
        .iter()
        .map(|chapter| (chapter.chapter, chapter))
        .collect();
    if chapters.len() != edition.chapters.len() {
        return Err(invalid("Book contains repeated chapter numbers"));
    }
    let mut transcripts = BTreeMap::new();
    let mut audio = BTreeMap::new();
    for pointer in &catalog.assets {
        if !hash_id(&pointer.job_id) || transcripts.contains_key(&pointer.job_id) {
            return Err(invalid("Invalid or repeated transcript asset identity"));
        }
        let chapter = chapters
            .get(&pointer.chapter)
            .filter(|c| c.chapter > 0)
            .ok_or_else(|| invalid("Transcript references a missing chapter"))?;
        if let std::collections::btree_map::Entry::Vacant(entry) = audio.entry(pointer.chapter) {
            let path = chapter.file.replace('\\', "/");
            let (asset, ()) = inspect_asset(&book, Path::new(&path), &mut |_, _, _| Ok(()))?;
            require_content(&asset, chapter.bytes, &chapter.sha256)?;
            entry.insert(asset);
        }
        transcripts.insert(
            pointer.job_id.clone(),
            read_transcript_asset(root, &book_snapshot, pointer, chapter)?,
        );
    }
    if audio.len() != catalog.active_by_chapter.len()
        || audio.keys().any(|chapter| {
            catalog
                .active_by_chapter
                .get(&chapter.to_string())
                .and_then(|id| transcripts.get(id))
                .is_none_or(|record| record.record.asset_identity["source"]["number"] != *chapter)
        })
    {
        return Err(invalid("Invalid active transcript selection"));
    }
    let mut corrections = BTreeMap::new();
    for (id, child) in &transcripts {
        if let Some(correction) = &child.correction {
            let parent = transcripts.get(&correction.parent_job_id).ok_or_else(|| {
                invalid(format!(
                    "Missing correction parent: {}",
                    correction.parent_job_id
                ))
            })?;
            corrections.insert(
                id.clone(),
                verify_correction(root, child, correction, parent)?,
            );
        }
    }
    Ok(Transcripts {
        catalog_asset,
        catalog,
        transcripts,
        audio,
        corrections,
    })
}

pub fn transcript_import_plan(book: &Path, source: &Path, destination: &Path) -> io::Result<Value> {
    let source = checked_root(source)?;
    let mut records = read_transcripts(book, &source)?;
    let mut assets = Vec::new();
    for record in records
        .transcripts
        .values_mut()
        .filter(|record| record.correction.is_none())
    {
        assets.append(&mut record.files);
    }
    for (id, record) in records
        .transcripts
        .iter_mut()
        .filter(|(_, r)| r.correction.is_some())
    {
        assets.extend(records.corrections.remove(id));
        assets.append(&mut record.files);
    }
    let catalog_sha256 = records.catalog_asset.sha256.clone();
    assets.push(records.catalog_asset);
    let copies = plan_copies(source, destination, assets, false)?;
    Ok(
        json!({"schema_version": SCHEMA_VERSION, "book_id": records.catalog.book_id,
        "catalog_sha256": catalog_sha256, "active_by_chapter": records.catalog.active_by_chapter,
        "already_present": copies.already_present, "plan": copies.plan}),
    )
}

pub fn search_transcripts(book: &Path, catalog: &Path, query: &str) -> io::Result<Value> {
    let query = query.trim().to_lowercase();
    if query.is_empty() || query.len() > 512 {
        return Err(invalid("Search expects a nonempty query up to 512 bytes"));
    }
    let root = checked_root(catalog)?;
    let records = read_transcripts(book, &root)?;
    let mut hits = Vec::new();
    for (chapter, audio) in &records.audio {
        let id = &records.catalog.active_by_chapter[&chapter.to_string()];
        let record = &records.transcripts[id];
        let duration = duration_ms(record.record.result.audio_duration_seconds);
        for segment in &record.timing.segments {
            if !segment.text.to_lowercase().contains(&query) {
                continue;
            }
            if hits.len() == 100 {
                return Ok(transcript_hits(records, hits, true));
            }
            hits.push(json!({"chapter": chapter, "job_id": id,
                "kind": if record.correction.is_some() {"reviewed_correction"} else {"machine_transcript"},
                "parent_job_id": record.correction.as_ref().map(|c| &c.parent_job_id),
                "audio_path": audio.relative_path, "audio_sha256": audio.sha256,
                "segments_path": record.files[1].relative_path, "segments_sha256": record.files[1].sha256,
                "segment": segment.id, "start_ms": segment.start_ms,
                "end_ms": segment.end_ms.min(duration), "stored_end_ms": segment.end_ms, "text": segment.text}));
        }
    }
    Ok(transcript_hits(records, hits, false))
}

fn transcript_hits(records: Transcripts, hits: Vec<Value>, truncated: bool) -> Value {
    json!({"schema_version": SCHEMA_VERSION, "book_id": records.catalog.book_id,
        "catalog_sha256": records.catalog_asset.sha256, "truncated": truncated, "hits": hits})
}
