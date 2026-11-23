//! Interactive approval and process-interruption recovery.

#[cfg(target_os = "linux")]
use crate::linux as platform;
#[cfg(windows)]
use crate::windows as platform;
use crate::{
    Action, Asset, FileId, Placement, Plan, SCHEMA_VERSION, checked_root, invalid, plan_digest,
    validate_relative, validate_root_pair,
};
use platform::{Access, PinnedDirectory, RunGuard};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    ffi::OsStr,
    fs::File,
    io::{self, BufRead, IsTerminal, Read, Seek, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const JOURNAL_VERSION: u32 = 1;
const RECORD_LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Run {
    schema_version: u32,
    tool_version: String,
    journal_id: FileId,
    plan: Plan,
    selected: Vec<u32>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum Method {
    Move,
    CopyRetain,
    Caption,
    ExtractAudio,
    RenderClip,
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct DerivedOutput {
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum Step {
    MovePrepared,
    CopyIntent {
        stage: String,
    },
    CopyCreated {
        stage: String,
        file_id: FileId,
    },
    CopyReady {
        stage: String,
        file_id: FileId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        derived: Option<DerivedOutput>,
    },
    Completed {
        method: Method,
        output_id: FileId,
        reconciled: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        derived: Option<DerivedOutput>,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
enum Event {
    Transition { id: u32, step: Step },
    Failure { id: u32, message: String },
}

#[derive(Default)]
struct Progress {
    step: Option<Step>,
    error: Option<String>,
}

struct Journal {
    _parent: PinnedDirectory,
    file: File,
    run: Run,
    progress: BTreeMap<u32, Progress>,
    valid_bytes: u64,
    torn_tail: bool,
    write_failed: bool,
}

struct Roots {
    _source: PinnedDirectory,
    _destination: PinnedDirectory,
}

impl Roots {
    fn bind(plan: &Plan, source: &Path, destination: &Path) -> io::Result<Self> {
        let source = checked_root(source)?;
        let destination = checked_root(destination)?;
        if source != plan.body.source_root || destination != plan.body.destination_root {
            return Err(invalid("Explicit permitted roots differ from the plan"));
        }
        let source_pin = execution_directory(&source)?;
        let destination_pin = execution_directory(&destination)?;
        #[cfg(target_os = "linux")]
        if source != destination && source_pin.id == destination_pin.id {
            return Err(invalid(
                "Distinct roots must not refer to the same directory",
            ));
        }
        expect_id(Some(source_pin.id), plan.body.source_root_id, "source root")?;
        expect_id(
            Some(destination_pin.id),
            plan.body.destination_root_id,
            "destination root",
        )?;
        Ok(Self {
            _source: source_pin,
            _destination: destination_pin,
        })
    }
}

struct Paths {
    source_parent: PinnedDirectory,
    destination_parent: PinnedDirectory,
    source: PathBuf,
    destination: PathBuf,
    method: Method,
}

impl Paths {
    fn bind(plan: &Plan, action: &Action) -> io::Result<Self> {
        let source = plan.body.source_root.join(&action.source.relative_path);
        let destination = plan.body.destination_root.join(&action.destination);
        #[cfg(windows)]
        let (source_parent, destination_parent) = (
            PinnedDirectory::open(parent(&source)?)?,
            PinnedDirectory::open(parent(&destination)?)?,
        );
        #[cfg(target_os = "linux")]
        let (source_parent, destination_parent) = (
            execution_directory(parent(&source)?)?,
            execution_directory(parent(&destination)?)?,
        );
        expect_id(
            Some(source_parent.id),
            action.source.parent_id,
            "source directory",
        )?;
        expect_id(
            Some(destination_parent.id),
            action.destination_parent_id,
            "destination directory",
        )?;
        let source_id = action
            .source
            .file_id
            .ok_or_else(|| invalid("Missing planned file identity"))?;
        let method = if action.placement == Placement::RenderClip {
            Method::RenderClip
        } else if action.placement == Placement::ExtractAudio {
            Method::ExtractAudio
        } else if action.placement == Placement::Caption {
            Method::Caption
        } else if action.placement == Placement::MoveIfSameFilesystemElseCopyRetain
            && source_id.volume == destination_parent.id.volume
        {
            Method::Move
        } else {
            Method::CopyRetain
        };
        Ok(Self {
            source_parent,
            destination_parent,
            source,
            destination,
            method,
        })
    }

    fn verified_source(&self, asset: &Asset) -> io::Result<File> {
        let access = if self.method == Method::Move {
            Access::Move
        } else {
            Access::Read
        };
        let mut source = self
            .source_parent
            .open_file(basename(&self.source)?, access)?;
        verify_file(&mut source, asset.file_id, asset)?;
        Ok(source)
    }

    fn require_absent(&self) -> io::Result<()> {
        let name = basename(&self.destination)?;
        #[cfg(windows)]
        let present = self.destination_parent.present(name)?;
        #[cfg(target_os = "linux")]
        let present = self
            .destination_parent
            .folded_names()?
            .contains(&name.to_string_lossy().to_lowercase());
        if present {
            return Err(invalid(format!(
                "Destination already exists: {}",
                self.destination.display()
            )));
        }
        Ok(())
    }

    fn publish(&self, file: &File, _from: &PinnedDirectory, _name: &OsStr) -> io::Result<()> {
        let destination = basename(&self.destination)?;
        #[cfg(windows)]
        let result = platform::publish(file, &self.destination_parent, destination);
        #[cfg(target_os = "linux")]
        let result = platform::publish(file, _from, _name, &self.destination_parent, destination);
        result.map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("Publish {}: {error}", self.destination.display()),
            )
        })
    }
}

fn execution_directory(path: &Path) -> io::Result<PinnedDirectory> {
    #[cfg(windows)]
    {
        let directory = PinnedDirectory::open(path)?;
        directory.require_local_ntfs(path)?;
        Ok(directory)
    }
    #[cfg(target_os = "linux")]
    PinnedDirectory::open_execution(path)
}

fn basename(path: &Path) -> io::Result<&OsStr> {
    path.file_name()
        .ok_or_else(|| invalid("Expected a file name"))
}

fn parent(path: &Path) -> io::Result<&Path> {
    path.parent()
        .ok_or_else(|| invalid("Expected file or directory parent"))
}

fn expect_id(actual: Option<FileId>, expected: Option<FileId>, label: &str) -> io::Result<()> {
    if expected.is_none() || actual != expected {
        return Err(invalid(format!("Stale or changed {label} identity")));
    }
    Ok(())
}

fn verify_file(file: &mut File, id: Option<FileId>, asset: &Asset) -> io::Result<()> {
    expect_id(Some(platform::identity(file)?), id, "file")?;
    file.rewind()?;
    let mut hash = Sha256::new();
    let bytes = io::copy(file, &mut hash)?;
    if bytes != asset.bytes || format!("{:x}", hash.finalize()) != asset.sha256 {
        return Err(invalid(format!(
            "File bytes changed: {}",
            asset.relative_path.display()
        )));
    }
    file.rewind()
}

pub(crate) fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn validate_plan(plan: &Plan, selection: &[u32]) -> io::Result<Vec<u32>> {
    if plan.body.schema_version != SCHEMA_VERSION || plan.sha256 != plan_digest(&plan.body)? {
        return Err(invalid(
            "Plan version or digest mismatch. Generate a fresh plan.",
        ));
    }
    let same_root = validate_root_pair(&plan.body.source_root, &plan.body.destination_root)?;
    let placement = if same_root {
        Placement::MoveIfSameFilesystemElseCopyRetain
    } else {
        Placement::CopyRetain
    };
    let mut sources = BTreeMap::new();
    let mut destinations = BTreeSet::new();
    for (index, action) in plan.body.actions.iter().enumerate() {
        let source = &action.source.relative_path;
        let destination = &action.destination;
        validate_relative(source)?;
        validate_relative(destination)?;
        let valid_output = match (&action.caption, &action.audio, &action.clip) {
            (Some(caption), None, None) => {
                same_root
                    && action.placement == Placement::Caption
                    && action.source.parent_id == action.destination_parent_id
                    && crate::caption_path(source, caption)? == *destination
            }
            (None, Some(recipe), None) => {
                action.placement == Placement::ExtractAudio
                    && crate::recording::valid_mp3_path(destination)
                    && recipe.valid()
            }
            (None, None, Some(recipe)) => {
                !same_root
                    && action.placement == Placement::RenderClip
                    && crate::recording::valid_mp4_path(destination)
                    && recipe.valid()
            }
            (None, None, None) => {
                action.placement == placement
                    && destination
                        .file_name()
                        .is_some_and(|name| crate::valid_destination_name(source, name))
            }
            _ => false,
        };
        let source_key = source
            .components()
            .collect::<PathBuf>()
            .to_string_lossy()
            .to_lowercase();
        let repeated_source = sources.insert(source_key, action);
        let valid_source = repeated_source.is_none_or(|previous: &Action| {
            previous.clip.is_some() && action.clip.is_some() && previous.source == action.source
        });
        if usize::try_from(action.id).ok() != Some(index + 1)
            || !valid_output
            || !valid_digest(&action.source.sha256)
            || !valid_source
            || !destinations.insert(
                destination
                    .components()
                    .collect::<PathBuf>()
                    .to_string_lossy()
                    .to_lowercase(),
            )
            || (same_root && source == destination)
        {
            return Err(invalid(format!(
                "Invalid or overlapping plan action {}",
                action.id
            )));
        }
    }
    let selected: BTreeSet<_> = selection.iter().copied().collect();
    if selected.is_empty()
        || selected.len() != selection.len()
        || selected.contains(&0)
        || selected
            .last()
            .is_some_and(|id| *id as usize > plan.body.actions.len())
    {
        return Err(invalid("Select distinct action IDs present in the plan"));
    }
    if same_root && sources.keys().any(|source| destinations.contains(source)) {
        return Err(invalid("A destination aliases a source path"));
    }
    Ok(selected.into_iter().collect())
}

fn action(plan: &Plan, id: u32) -> &Action {
    &plan.body.actions[(id - 1) as usize]
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> io::Result<T> {
    serde_json::from_slice(bytes).map_err(|error| invalid(format!("Invalid JSON record: {error}")))
}

fn read_bounded(file: &mut File) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(RECORD_LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECORD_LIMIT {
        return Err(invalid("Record exceeds 64 MiB"));
    }
    Ok(bytes)
}

fn load_plan(path: &Path) -> io::Result<Plan> {
    #[cfg(windows)]
    let mut file = File::open(path)?;
    #[cfg(target_os = "linux")]
    let mut file = execution_directory(parent(path)?)?.open_file(basename(path)?, Access::Read)?;
    decode(&read_bounded(&mut file)?)
}

fn preview(plan: &Plan, ids: &[u32]) -> Value {
    json!({
        "schema_version": SCHEMA_VERSION,
        "plan_sha256": plan.sha256,
        "source_root": plan.body.source_root,
        "destination_root": plan.body.destination_root,
        "selected_actions": ids.iter().map(|id| action(plan, *id)).collect::<Vec<_>>()
    })
}

fn check_selected(plan: &Plan, ids: &[u32]) -> io::Result<()> {
    for id in ids {
        let action = action(plan, *id);
        let paths = Paths::bind(plan, action)?;
        paths.require_absent()?;
        paths.verified_source(&action.source)?;
        if let Some(recipe) = &action.audio {
            recipe.engines()?;
        }
        if let Some(recipe) = &action.clip {
            recipe.engines()?;
        }
    }
    Ok(())
}

pub fn verify(plan: &Path, source: &Path, destination: &Path, ids: &[u32]) -> io::Result<Value> {
    let plan = load_plan(plan)?;
    let ids = validate_plan(&plan, ids)?;
    let _roots = Roots::bind(&plan, source, destination)?;
    check_selected(&plan, &ids)?;
    Ok(preview(&plan, &ids))
}

#[derive(Clone, Copy)]
pub enum Approval {
    Terminal,
    #[cfg(target_os = "linux")]
    ControllingTerminal,
    #[cfg(windows)]
    Desktop,
}

fn confirm(
    verb: &str,
    plan: &Plan,
    ids: &[u32],
    journal: &Path,
    approval: Approval,
) -> io::Result<()> {
    let selection = ids.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
    let expected = format!("{verb} {} {selection}", plan.sha256);
    let mut terminal = Vec::new();
    writeln!(terminal, "Operation: {verb}")?;
    writeln!(terminal, "Journal: {journal:?}")?;
    writeln!(terminal, "Source root: {:?}", plan.body.source_root)?;
    writeln!(
        terminal,
        "Destination root: {:?}",
        plan.body.destination_root
    )?;
    for id in ids {
        let action = action(plan, *id);
        let policy = match action.placement {
            Placement::CopyRetain => "copy and retain source",
            Placement::Caption => "write caption sidecar and retain source",
            Placement::ExtractAudio => "extract MP3 and retain source",
            Placement::RenderClip => "render selected MP4 range and retain source",
            Placement::MoveIfSameFilesystemElseCopyRetain => {
                "move on same filesystem, otherwise copy and retain"
            }
        };
        writeln!(
            terminal,
            "{id}: {:?} -> {:?} ({policy}, {} bytes, SHA-256 {})",
            action.source.relative_path,
            action.destination,
            action.source.bytes,
            action.source.sha256
        )?;
        if let Some(caption) = &action.caption {
            writeln!(terminal, "   Caption: {caption:?}")?;
        }
        if let Some(recipe) = &action.audio {
            writeln!(terminal, "   Audio: {}", serde_json::to_string(recipe)?)?;
        }
        if let Some(recipe) = &action.clip {
            writeln!(terminal, "   Clip: {}", serde_json::to_string(recipe)?)?;
        }
    }
    writeln!(
        terminal,
        "Review the roots and selected actions. Type this exact line to proceed:"
    )?;
    writeln!(terminal, "{expected}")?;
    let answer = match approval {
        Approval::Terminal => {
            if !io::stdin().is_terminal() || !io::stderr().is_terminal() {
                return Err(invalid(
                    "Interactive approval requires terminal stdin and stderr",
                ));
            }
            let mut output = io::stderr().lock();
            output.write_all(&terminal)?;
            output.flush()?;
            let mut answer = String::new();
            io::stdin().lock().take(16 * 1024).read_line(&mut answer)?;
            answer
        }
        #[cfg(target_os = "linux")]
        Approval::ControllingTerminal => platform::terminal_approval(&terminal)?,
        #[cfg(windows)]
        Approval::Desktop => {
            platform::desktop_approval(std::str::from_utf8(&terminal).map_err(io::Error::other)?)?
        }
    };
    if answer
        .strip_suffix("\r\n")
        .or_else(|| answer.strip_suffix('\n'))
        != Some(expected.as_str())
    {
        return Err(invalid("Approval denied. No operation was started."));
    }
    Ok(())
}

fn journal_path(path: &Path, plan: Option<&Plan>) -> io::Result<(PinnedDirectory, PathBuf)> {
    if let Some(plan) = plan {
        return record_path(path, &[&plan.body.source_root, &plan.body.destination_root]);
    }
    record_path(path, &[])
}

fn record_path(path: &Path, roots: &[&Path]) -> io::Result<(PinnedDirectory, PathBuf)> {
    let name = path
        .file_name()
        .ok_or_else(|| invalid("Expected an absolute journal file"))?;
    validate_relative(Path::new(name))?;
    let parent = checked_root(parent(path)?)?;
    for root in roots {
        if Path::new(&parent.to_string_lossy().to_lowercase())
            .starts_with(root.to_string_lossy().to_lowercase())
        {
            return Err(invalid("Store the journal outside the media roots"));
        }
    }
    let pin = execution_directory(&parent)?;
    Ok((pin, parent.join(name)))
}

pub(crate) fn open_checkpoint<T: serde::de::DeserializeOwned>(
    path: &Path,
    root: &Path,
    header: &[u8],
) -> io::Result<(File, Vec<T>)> {
    let (parent, path) = record_path(path, &[root])?;
    let name = basename(&path)?;
    let mut file = parent.open_file(
        name,
        if parent.present(name)? {
            Access::JournalExisting
        } else {
            Access::JournalNew
        },
    )?;
    #[cfg(windows)]
    platform::require_single_link(&file)?;
    file.try_lock().map_err(io::Error::from)?;
    let bytes = read_bounded(&mut file)?;
    if header.starts_with(&bytes) {
        file.rewind()?;
        file.write_all(header)?;
        file.sync_all()?;
        #[cfg(target_os = "linux")]
        parent.sync()?;
        return Ok((file, Vec::new()));
    }
    let records = bytes
        .strip_prefix(header)
        .ok_or_else(|| invalid("Invalid checkpoint header"))?;
    let complete = records
        .iter()
        .rposition(|&byte| byte == b'\n')
        .map_or(0, |index| index + 1);
    let records = records[..complete]
        .split_inclusive(|&byte| byte == b'\n')
        .map(decode)
        .collect::<io::Result<Vec<_>>>()?;
    let valid_bytes = (header.len() + complete) as u64;
    if valid_bytes < bytes.len() as u64 {
        file.set_len(valid_bytes)?;
        file.sync_all()?;
    }
    file.seek(io::SeekFrom::Start(valid_bytes))?;
    Ok((file, records))
}

pub(crate) fn append_checkpoint(file: &mut File, record: &impl Serialize) -> io::Result<()> {
    let mut bytes = serde_json::to_vec(record)?;
    bytes.push(b'\n');
    if file.stream_position()? + bytes.len() as u64 > RECORD_LIMIT {
        return Err(invalid("Checkpoint exceeds 64 MiB"));
    }
    file.write_all(&bytes)?;
    file.sync_all()
}

impl Journal {
    fn create(path: &Path, plan: Plan, selected: Vec<u32>) -> io::Result<Self> {
        let (parent, path) = journal_path(path, Some(&plan))?;
        let file = parent.open_file(basename(&path)?, Access::JournalNew)?;
        let run = Run {
            schema_version: JOURNAL_VERSION,
            tool_version: env!("CARGO_PKG_VERSION").into(),
            journal_id: platform::identity(&file)?,
            plan,
            selected,
        };
        let mut journal = Self::empty(parent, file, run);
        journal.append_line(serde_json::to_vec(&journal.run)?)?;
        #[cfg(target_os = "linux")]
        journal._parent.sync()?;
        Ok(journal)
    }

    fn empty(parent: PinnedDirectory, file: File, run: Run) -> Self {
        let progress = run
            .selected
            .iter()
            .map(|&id| (id, Progress::default()))
            .collect();
        Self {
            _parent: parent,
            file,
            run,
            progress,
            valid_bytes: 0,
            torn_tail: false,
            write_failed: false,
        }
    }

    fn open(path: &Path, write: bool) -> io::Result<Self> {
        let (parent, canonical) = journal_path(path, None)?;
        let access = if write {
            Access::JournalExisting
        } else {
            Access::Read
        };
        let mut file = parent.open_file(basename(&canonical)?, access)?;
        let bytes = read_bounded(&mut file)?;
        let mut lines = bytes.split_inclusive(|&byte| byte == b'\n');
        let header = lines
            .next()
            .filter(|line| line.ends_with(b"\n"))
            .ok_or_else(|| invalid("Journal has no complete approval header"))?;
        let run: Run = decode(header)?;
        if run.schema_version != JOURNAL_VERSION
            || run.journal_id != platform::identity(&file)?
            || run.selected != validate_plan(&run.plan, &run.selected)?
        {
            return Err(invalid("Journal identity, version or selection mismatch"));
        }
        journal_path(path, Some(&run.plan))?;
        let mut journal = Self::empty(parent, file, run);
        journal.valid_bytes = header.len() as u64;
        for line in lines {
            if !line.ends_with(b"\n") {
                journal.torn_tail = true;
                break;
            }
            journal.register(decode(line)?)?;
            journal.valid_bytes += line.len() as u64;
        }
        Ok(journal)
    }

    fn register(&mut self, event: Event) -> io::Result<()> {
        let id = match &event {
            Event::Transition { id, .. } | Event::Failure { id, .. } => *id,
        };
        let progress = self
            .progress
            .get_mut(&id)
            .ok_or_else(|| invalid("Unapproved journal action"))?;
        if matches!(progress.step, Some(Step::Completed { .. })) {
            return Err(invalid("Completed journal action has subsequent events"));
        }
        match event {
            Event::Transition { step, .. } => {
                let planned = action(&self.run.plan, id);
                let allowed = match (&progress.step, &step) {
                    (None, Step::MovePrepared) => {
                        planned.placement == Placement::MoveIfSameFilesystemElseCopyRetain
                    }
                    (None, Step::CopyIntent { stage }) => valid_stage(stage),
                    (Some(Step::CopyIntent { stage }), Step::CopyCreated { stage: next, .. }) => {
                        stage == next
                    }
                    (
                        Some(Step::CopyCreated { stage, file_id }),
                        Step::CopyReady {
                            stage: next,
                            file_id: next_id,
                            derived,
                        },
                    ) => {
                        stage == next
                            && file_id == next_id
                            && derived.is_some()
                                == (planned.audio.is_some() || planned.clip.is_some())
                            && derived
                                .as_ref()
                                .is_none_or(|info| info.bytes > 0 && valid_digest(&info.sha256))
                    }
                    (
                        Some(Step::CopyReady {
                            file_id, derived, ..
                        }),
                        Step::Completed {
                            method,
                            output_id,
                            derived: recorded,
                            ..
                        },
                    ) => {
                        file_id == output_id
                            && derived == recorded
                            && match planned.placement {
                                Placement::ExtractAudio => *method == Method::ExtractAudio,
                                Placement::RenderClip => *method == Method::RenderClip,
                                Placement::Caption => *method == Method::Caption,
                                _ => *method == Method::CopyRetain,
                            }
                    }
                    (
                        Some(Step::MovePrepared),
                        Step::Completed {
                            method: Method::Move,
                            output_id,
                            derived: None,
                            ..
                        },
                    ) => action(&self.run.plan, id).source.file_id == Some(*output_id),
                    _ => false,
                };
                if !allowed {
                    return Err(invalid("Invalid journal transition"));
                }
                progress.step = Some(step);
                progress.error = None;
            }
            Event::Failure { message, .. } => progress.error = Some(message),
        }
        Ok(())
    }

    fn append_line(&mut self, mut bytes: Vec<u8>) -> io::Result<()> {
        bytes.push(b'\n');
        if self.valid_bytes + bytes.len() as u64 > RECORD_LIMIT {
            return Err(invalid("Journal exceeds 64 MiB"));
        }
        if let Err(error) = self
            .file
            .write_all(&bytes)
            .and_then(|()| self.file.sync_all())
        {
            self.write_failed = true;
            return Err(io::Error::other(format!(
                "Journal update failed. Inspect the receipt before recovery: {error}"
            )));
        }
        self.valid_bytes += bytes.len() as u64;
        Ok(())
    }

    fn append(&mut self, event: Event) -> io::Result<()> {
        self.append_line(serde_json::to_vec(&event)?)?;
        self.register(event)
    }

    fn ready_to_append(&mut self) -> io::Result<()> {
        if self.torn_tail {
            self.file.set_len(self.valid_bytes)?;
            self.file.sync_all()?;
            self.torn_tail = false;
        }
        self.file.seek(io::SeekFrom::Start(self.valid_bytes))?;
        Ok(())
    }

    fn result(&self) -> (Value, bool) {
        let complete = !self.torn_tail
            && self
                .progress
                .values()
                .all(|p| matches!(p.step, Some(Step::Completed { .. })));
        let actions: Vec<_> = self
            .progress
            .iter()
            .map(|(&id, progress)| {
                let status = match (&progress.step, &progress.error) {
                    (Some(Step::Completed { .. }), _) => "completed",
                    (_, Some(_)) => "failed",
                    (Some(_), None) => "interrupted",
                    (None, None) => "pending",
                };
                json!({
                    "action": action(&self.run.plan, id),
                    "status": status,
                    "last_durable_state": progress.step,
                    "error": progress.error
                })
            })
            .collect();
        (
            json!({
                "schema_version": JOURNAL_VERSION,
                "tool_version": self.run.tool_version,
                "journal_id": self.run.journal_id,
                "plan_sha256": self.run.plan.sha256,
                "source_root": self.run.plan.body.source_root,
                "destination_root": self.run.plan.body.destination_root,
                "status": if complete { "completed" } else { "incomplete" },
                "torn_tail": self.torn_tail,
                "actions": actions
            }),
            complete,
        )
    }
}

fn valid_stage(stage: &str) -> bool {
    stage.strip_prefix(".intake-stage-").is_some_and(|rest| {
        !rest.is_empty()
            && rest.len() <= 64
            && rest.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    })
}

fn stage_name(id: u32) -> io::Result<String> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(io::Error::other)?
        .as_nanos();
    Ok(format!(".intake-stage-{}-{id}-{nanos}", std::process::id()))
}

// The observer is a no-op in the CLI. Tests stop a separate process at these boundaries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Boundary {
    JournalReady,
    StageCreated,
    BeforePublication,
    AfterPublication,
    BeforeReceipt,
    AfterReceipt,
}

fn write_step(journal: &mut Journal, id: u32, step: Step) -> io::Result<()> {
    journal.append(Event::Transition { id, step })
}

fn complete(
    journal: &mut Journal,
    id: u32,
    step: Step,
    observe: &mut impl FnMut(Boundary),
) -> io::Result<()> {
    observe(Boundary::BeforeReceipt);
    write_step(journal, id, step)?;
    observe(Boundary::AfterReceipt);
    Ok(())
}

fn finish_output(paths: &Paths, output_id: FileId, asset: &Asset) -> io::Result<File> {
    let mut output = paths
        .destination_parent
        .open_file(basename(&paths.destination)?, Access::Read)?;
    verify_file(&mut output, Some(output_id), asset)?;
    Ok(output)
}

fn placement(
    journal: &mut Journal,
    action: &Action,
    observe: &mut impl FnMut(Boundary),
) -> io::Result<()> {
    let id = action.id;
    let paths = Paths::bind(&journal.run.plan, action)?;
    let asset = &action.source;
    let mut state = journal.progress[&id].step.clone();
    let mut derived_result = None;
    let (output_id, reconciled, _source, _output) = match paths.method {
        Method::Move => {
            if matches!(state, Some(Step::MovePrepared))
                && !paths.source_parent.present(basename(&paths.source)?)?
            {
                let output_id = asset
                    .file_id
                    .ok_or_else(|| invalid("Missing planned file identity"))?;
                let output = finish_output(&paths, output_id, asset)?;
                (output_id, true, None, output)
            } else {
                paths.require_absent()?;
                let source = paths.verified_source(asset)?;
                let output_id = platform::identity(&source)?;
                match state {
                    None => write_step(journal, id, Step::MovePrepared)?,
                    Some(Step::MovePrepared) => {}
                    _ => {
                        return Err(invalid(
                            "Recorded execution method differs from approved move",
                        ));
                    }
                }
                observe(Boundary::BeforePublication);
                paths.publish(&source, &paths.source_parent, basename(&paths.source)?)?;
                observe(Boundary::AfterPublication);
                drop(source);
                let output = finish_output(&paths, output_id, asset)?;
                (output_id, false, None, output)
            }
        }
        Method::CopyRetain | Method::Caption | Method::ExtractAudio | Method::RenderClip => {
            let mut source = paths.verified_source(asset)?;
            let mut caption_bytes = action.caption.as_deref().map(str::as_bytes);
            let caption_asset = caption_bytes.map(|bytes| Asset {
                relative_path: action.destination.clone(),
                bytes: bytes.len() as u64,
                sha256: format!("{:x}", Sha256::digest(bytes)),
                file_id: None,
                parent_id: action.destination_parent_id,
            });
            if state.is_none() {
                paths.require_absent()?;
                let step = Step::CopyIntent {
                    stage: stage_name(id)?,
                };
                write_step(journal, id, step.clone())?;
                state = Some(step);
            }
            if let Some(Step::CopyIntent { stage }) = &state {
                let path = paths.destination.with_file_name(stage);
                let staged = paths
                    .destination_parent
                    .open_file(OsStr::new(stage), Access::StageNew)
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!(
                                "Cannot establish ownership of stage {}: {error}",
                                path.display()
                            ),
                        )
                    })?;
                let new = Step::CopyCreated {
                    stage: stage.clone(),
                    file_id: platform::identity(&staged)?,
                };
                #[cfg(target_os = "linux")]
                paths.destination_parent.sync()?;
                write_step(journal, id, new.clone())?;
                observe(Boundary::StageCreated);
                state = Some(new);
            }
            if let Some(Step::CopyCreated { stage, file_id }) = &state {
                paths.require_absent()?;
                let mut staged = paths
                    .destination_parent
                    .open_file(OsStr::new(stage), Access::StageExisting)?;
                expect_id(
                    Some(platform::identity(&staged)?),
                    Some(*file_id),
                    "staging file",
                )?;
                // FFmpeg inherits the staged handle. On Linux keep the lock on that
                // file description until both parent and child release it.
                #[cfg(target_os = "linux")]
                if action.audio.is_some() || action.clip.is_some() {
                    staged.try_lock().map_err(io::Error::from)?;
                }
                staged.set_len(0)?;
                let derived = if action.audio.is_some() || action.clip.is_some() {
                    if let Some(recipe) = &action.audio {
                        crate::recording::render_mp3(recipe, &source, &mut staged)?;
                    } else if let Some(recipe) = &action.clip {
                        crate::recording::render_mp4(recipe, &source, &mut staged)?;
                    }
                    verify_file(&mut source, asset.file_id, asset)?;
                    let mut hash = Sha256::new();
                    let bytes = io::copy(&mut staged, &mut hash)?;
                    Some(DerivedOutput {
                        bytes,
                        sha256: format!("{:x}", hash.finalize()),
                    })
                } else {
                    let input: &mut dyn Read = match &mut caption_bytes {
                        Some(bytes) => bytes,
                        None => &mut source,
                    };
                    io::copy(input, &mut staged)?;
                    verify_file(
                        &mut staged,
                        Some(*file_id),
                        caption_asset.as_ref().unwrap_or(asset),
                    )?;
                    None
                };
                staged.sync_all()?;
                let new = Step::CopyReady {
                    stage: stage.clone(),
                    file_id: *file_id,
                    derived,
                };
                write_step(journal, id, new.clone())?;
                state = Some(new);
            }
            let Some(Step::CopyReady {
                stage,
                file_id,
                derived,
            }) = state
            else {
                return Err(invalid(
                    "Recorded execution method differs from approved copy",
                ));
            };
            let derived_asset = derived.as_ref().map(|info| Asset {
                relative_path: action.destination.clone(),
                bytes: info.bytes,
                sha256: info.sha256.clone(),
                file_id: Some(file_id),
                parent_id: action.destination_parent_id,
            });
            let expected = derived_asset
                .as_ref()
                .or(caption_asset.as_ref())
                .unwrap_or(asset);
            let stage = OsStr::new(&stage);
            let reconciled = if paths.destination_parent.present(stage)? {
                paths.require_absent()?;
                let mut staged = paths
                    .destination_parent
                    .open_file(stage, Access::StageExisting)?;
                verify_file(&mut staged, Some(file_id), expected)?;
                observe(Boundary::BeforePublication);
                paths.publish(&staged, &paths.destination_parent, stage)?;
                observe(Boundary::AfterPublication);
                staged.sync_all()?;
                false
            } else {
                true
            };
            let output = finish_output(&paths, file_id, expected)?;
            derived_result = derived;
            (file_id, reconciled, Some(source), output)
        }
    };
    #[cfg(target_os = "linux")]
    {
        paths.destination_parent.sync()?;
        if paths.method == Method::Move && paths.source_parent.id != paths.destination_parent.id {
            paths.source_parent.sync()?;
        }
    }
    complete(
        journal,
        id,
        Step::Completed {
            method: paths.method,
            output_id,
            reconciled,
            derived: derived_result,
        },
        observe,
    )
}

fn execute(journal: &mut Journal, mut observe: impl FnMut(Boundary)) -> io::Result<(Value, bool)> {
    observe(Boundary::JournalReady);
    for id in journal.run.selected.clone() {
        if matches!(journal.progress[&id].step, Some(Step::Completed { .. })) {
            continue;
        }
        let action = action(&journal.run.plan, id).clone();
        if let Err(error) = placement(journal, &action, &mut observe) {
            if journal.write_failed {
                return Err(error);
            }
            journal.append(Event::Failure {
                id,
                message: error.to_string(),
            })?;
            break;
        }
    }
    Ok(journal.result())
}

pub fn apply(
    plan: &Path,
    source: &Path,
    destination: &Path,
    journal: &Path,
    ids: &[u32],
) -> io::Result<(Value, bool)> {
    apply_with_approval(plan, source, destination, journal, ids, Approval::Terminal)
}

pub fn apply_with_approval(
    plan: &Path,
    source: &Path,
    destination: &Path,
    journal: &Path,
    ids: &[u32],
    approval: Approval,
) -> io::Result<(Value, bool)> {
    let plan = load_plan(plan)?;
    let ids = validate_plan(&plan, ids)?;
    let (_journal_parent, reviewed_journal) = journal_path(journal, Some(&plan))?;
    {
        let _roots = Roots::bind(&plan, source, destination)?;
        check_selected(&plan, &ids)?;
    }
    confirm("APPROVE", &plan, &ids, &reviewed_journal, approval)?;
    start(plan, ids, source, destination, journal, |_| {})
}

fn start(
    plan: Plan,
    ids: Vec<u32>,
    source: &Path,
    destination: &Path,
    journal: &Path,
    observe: impl FnMut(Boundary),
) -> io::Result<(Value, bool)> {
    let _exclusive = RunGuard::acquire()?;
    let _roots = Roots::bind(&plan, source, destination)?;
    check_selected(&plan, &ids)?;
    let mut journal = Journal::create(journal, plan, ids)?;
    execute(&mut journal, observe)
}

pub fn recover(path: &Path, source: &Path, destination: &Path) -> io::Result<(Value, bool)> {
    recover_with_approval(path, source, destination, Approval::Terminal)
}

pub fn recover_with_approval(
    path: &Path,
    source: &Path,
    destination: &Path,
    approval: Approval,
) -> io::Result<(Value, bool)> {
    let review = Journal::open(path, false)?;
    {
        let _roots = Roots::bind(&review.run.plan, source, destination)?;
    }
    if review.result().1 {
        return Ok(review.result());
    }
    confirm(
        "RECOVER",
        &review.run.plan,
        &review.run.selected,
        path,
        approval,
    )?;
    let reviewed_run = review.run.clone();
    drop(review);
    resume(path, source, destination, &reviewed_run)
}

fn resume(
    path: &Path,
    source: &Path,
    destination: &Path,
    reviewed: &Run,
) -> io::Result<(Value, bool)> {
    let _exclusive = RunGuard::acquire()?;
    let mut journal = Journal::open(path, true)?;
    if journal.run.journal_id != reviewed.journal_id
        || journal.run.plan.sha256 != reviewed.plan.sha256
        || journal.run.selected != reviewed.selected
    {
        return Err(invalid("Journal changed after review"));
    }
    let _roots = Roots::bind(&journal.run.plan, source, destination)?;
    journal.ready_to_append()?;
    execute(&mut journal, |_| {})
}

pub fn receipt(path: &Path) -> io::Result<Value> {
    Ok(Journal::open(path, false)?.result().0)
}

pub fn verify_package(path: &Path, source: &Path, destination: &Path) -> io::Result<Value> {
    let journal = Journal::open(path, false)?;
    let plan = &journal.run.plan;
    let _roots = Roots::bind(plan, source, destination)?;
    let (mut receipt, complete) = journal.result();
    if journal.torn_tail || !complete {
        return Err(invalid("Recover the package journal before verification"));
    }
    for id in &journal.run.selected {
        let selected = action(plan, *id);
        let Some(Step::Completed {
            method: Method::RenderClip,
            output_id,
            derived: Some(derived),
            ..
        }) = &journal.progress[id].step
        else {
            return Err(invalid(format!(
                "Clip action {id} requires a completed receipt"
            )));
        };
        let paths = Paths::bind(plan, selected)?;
        let _original = paths.verified_source(&selected.source)?;
        let mut output = paths
            .destination_parent
            .open_file(basename(&paths.destination)?, Access::Read)?;
        verify_file(
            &mut output,
            Some(*output_id),
            &Asset {
                relative_path: selected.destination.clone(),
                sha256: derived.sha256.clone(),
                bytes: derived.bytes,
                file_id: Some(*output_id),
                parent_id: selected.destination_parent_id,
            },
        )?;
    }
    receipt["package_verified"] = Value::Bool(true);
    Ok(receipt)
}

pub fn undo_plan(path: &Path, root: &Path, ids: &[u32]) -> io::Result<Plan> {
    let journal = Journal::open(path, false)?;
    let original = &journal.run.plan;
    let ids = validate_plan(original, ids)?;
    if journal.torn_tail || original.body.source_root != original.body.destination_root {
        return Err(invalid("Undo requires a readable same-root move journal"));
    }
    let _roots = Roots::bind(original, root, root)?;
    let mut actions = Vec::with_capacity(ids.len());
    for (index, id) in ids.into_iter().enumerate() {
        let forward = action(original, id);
        let Some(Step::Completed {
            method: Method::Move,
            output_id,
            ..
        }) = journal
            .progress
            .get(&id)
            .and_then(|progress| progress.step.as_ref())
        else {
            return Err(invalid(format!(
                "Undo requires a completed move for action {id}"
            )));
        };
        expect_id(
            Some(*output_id),
            forward.source.file_id,
            "original move output",
        )?;
        actions.push(Action {
            id: u32::try_from(index + 1).map_err(io::Error::other)?,
            source: Asset {
                relative_path: forward.destination.clone(),
                parent_id: forward.destination_parent_id,
                ..forward.source.clone()
            },
            destination: forward.source.relative_path.clone(),
            destination_parent_id: forward.source.parent_id,
            placement: Placement::MoveIfSameFilesystemElseCopyRetain,
            caption: None,
            audio: None,
            clip: None,
        });
    }
    let body = crate::PlanBody {
        actions,
        ..original.body.clone()
    };
    let reverse = Plan {
        sha256: plan_digest(&body)?,
        body,
    };
    let selected: Vec<_> = reverse
        .body
        .actions
        .iter()
        .map(|action| action.id)
        .collect();
    check_selected(&reverse, &validate_plan(&reverse, &selected)?)?;
    Ok(reverse)
}

#[cfg(test)]
mod tests;
