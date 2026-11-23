use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, Metadata},
    io::{self, Seek},
    path::{Component, Path, PathBuf},
};

#[cfg(any(windows, target_os = "linux"))]
pub mod execution;
pub mod library;
#[cfg(target_os = "linux")]
mod linux;
pub mod organization;
pub mod recording;
pub mod speech;
pub mod video;
#[cfg(windows)]
mod windows;

pub const SCHEMA_VERSION: u32 = 2;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FileId {
    volume: [u8; 8],
    file: [u8; 16],
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Asset {
    pub relative_path: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub file_id: Option<FileId>,
    pub parent_id: Option<FileId>,
}

#[derive(Serialize)]
pub struct Scan {
    pub schema_version: u32,
    pub root: PathBuf,
    pub assets: Vec<Asset>,
}

#[derive(Clone, Copy, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Placement {
    MoveIfSameFilesystemElseCopyRetain,
    CopyRetain,
    Caption,
    ExtractAudio,
    RenderClip,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub id: u32,
    pub source: Asset,
    pub destination: PathBuf,
    pub destination_parent_id: Option<FileId>,
    pub placement: Placement,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caption: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<recording::AudioRecipe>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub clip: Option<recording::ClipRecipe>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlanBody {
    pub schema_version: u32,
    pub source_root: PathBuf,
    pub destination_root: PathBuf,
    pub source_root_id: Option<FileId>,
    pub destination_root_id: Option<FileId>,
    pub actions: Vec<Action>,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Plan {
    pub sha256: String,
    pub body: PlanBody,
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

fn portable_component(name: &str) -> bool {
    if name.is_empty()
        || name.ends_with(['.', ' '])
        || name.contains(['\\', '/', ':', '<', '>', '"', '|', '?', '*'])
        || name.chars().any(char::is_control)
    {
        return false;
    }
    let folded = name.to_ascii_lowercase();
    let stem = folded.split('.').next().unwrap_or_default().trim_end();
    let device = |prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    };
    !matches!(folded.as_str(), ".git" | "git~1")
        && !folded.starts_with(".intake-stage-")
        && !matches!(stem, "con" | "prn" | "aux" | "nul" | "conin$" | "conout$")
        && !device("com")
        && !device("lpt")
}

fn validate_relative(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty()
        || !path.components().all(|part| match part {
            Component::Normal(name) => name.to_str().is_some_and(portable_component),
            _ => false,
        })
    {
        return Err(invalid(format!("Unsafe relative path: {}", path.display())));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn plain_metadata(path: &Path) -> io::Result<Metadata> {
    let metadata = fs::symlink_metadata(path)
        .map_err(|error| io::Error::new(error.kind(), format!("{}: {error}", path.display())))?;
    #[cfg(windows)]
    let reparse = {
        use std::os::windows::fs::MetadataExt;
        metadata.file_attributes() & 0x400 != 0
    };
    #[cfg(not(windows))]
    let reparse = metadata.file_type().is_symlink();
    if reparse {
        return Err(invalid(format!(
            "Link or reparse point: {}",
            path.display()
        )));
    }
    Ok(metadata)
}

#[cfg(not(target_os = "linux"))]
fn checked_path(path: &Path) -> io::Result<Metadata> {
    // Inspect ancestors before descendants so a junction is never knowingly traversed.
    let ancestors: Vec<_> = path.ancestors().skip(1).collect();
    for ancestor in ancestors.into_iter().rev() {
        if !plain_metadata(ancestor)?.is_dir() {
            return Err(invalid(format!(
                "Expected directory: {}",
                ancestor.display()
            )));
        }
    }
    plain_metadata(path)
}

fn checked_root(path: &Path) -> io::Result<PathBuf> {
    #[cfg(windows)]
    use std::path::Prefix;
    #[cfg(windows)]
    windows::require_local_path(path)?;
    #[cfg(windows)]
    let supported_prefix = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_))
    );
    #[cfg(not(windows))]
    let supported_prefix = true;
    if !supported_prefix
        || !path.is_absolute()
        || !path.components().all(|part| match part {
            Component::Normal(name) => name.to_str().is_some_and(portable_component),
            Component::Prefix(_) | Component::RootDir => true,
            _ => false,
        })
    {
        return Err(invalid(format!(
            "Expected a normal absolute root: {}",
            path.display()
        )));
    }
    #[cfg(target_os = "linux")]
    {
        linux::PinnedDirectory::open(path)?;
        Ok(path.components().collect())
    }
    #[cfg(not(target_os = "linux"))]
    {
        if !checked_path(path)?.is_dir() {
            return Err(invalid(format!("Expected directory: {}", path.display())));
        }
        let canonical = path.canonicalize()?;
        #[cfg(windows)]
        windows::require_local_path(&canonical)?;
        Ok(canonical)
    }
}

fn inspect_asset<T>(
    root: &Path,
    relative_path: &Path,
    inspect: &mut impl FnMut(&mut fs::File, &Metadata, &str) -> io::Result<T>,
) -> io::Result<(Asset, T)> {
    validate_relative(relative_path)?;
    let path = root.join(relative_path);
    #[cfg(not(target_os = "linux"))]
    let checked = checked_path(&path)?;
    #[cfg(not(target_os = "linux"))]
    if !checked.is_file() {
        return Err(invalid(format!(
            "Expected regular file: {}",
            path.display()
        )));
    }
    #[cfg(not(target_os = "linux"))]
    let path = path.canonicalize()?;
    #[cfg(not(target_os = "linux"))]
    let relative_path = path
        .strip_prefix(root)
        .map_err(|_| invalid(format!("File escapes its root: {}", path.display())))?;
    #[cfg(not(target_os = "linux"))]
    validate_relative(relative_path)?;
    #[cfg(windows)]
    let (mut file, file_id, parent_id, _parent) = {
        let parent = windows::PinnedDirectory::open(path.parent().expect("file has a parent"))?;
        let file = parent.open_file(
            path.file_name().expect("validated file name"),
            windows::Access::Read,
        )?;
        let id = windows::identity(&file)?;
        (file, Some(id), Some(parent.id), parent)
    };
    #[cfg(not(any(windows, target_os = "linux")))]
    let (mut file, file_id, parent_id) = (fs::File::open(&path)?, None, None);
    #[cfg(target_os = "linux")]
    let (mut file, file_id, parent_id, checked) = {
        let (file, parent_id) = linux_source(root, relative_path)?;
        let checked = file.metadata()?;
        let id = linux::identity(&file)?;
        (file, Some(id), Some(parent_id), checked)
    };
    let before = file.metadata()?;
    if !before.is_file()
        || before.len() != checked.len()
        || before.modified()? != checked.modified()?
    {
        return Err(invalid(format!(
            "File changed before reading: {}",
            path.display()
        )));
    }
    let mut digest = Sha256::new();
    let bytes = io::copy(&mut file, &mut digest)?;
    let sha256 = format!("{:x}", digest.finalize());
    file.rewind()?;
    let details = inspect(&mut file, &before, &sha256)?;
    let after = file.metadata()?;
    #[cfg(not(target_os = "linux"))]
    let current = checked_path(&path)?;
    #[cfg(target_os = "linux")]
    let current = {
        let (current, current_parent) = linux_source(root, relative_path)?;
        if Some(current_parent) != parent_id || Some(linux::identity(&current)?) != file_id {
            return Err(invalid(format!(
                "File replaced while reading: {}",
                path.display()
            )));
        }
        current.metadata()?
    };
    if bytes != before.len()
        || bytes != after.len()
        || bytes != current.len()
        || before.modified()? != after.modified()?
        || before.modified()? != current.modified()?
        || !current.is_file()
    {
        return Err(invalid(format!(
            "File changed while reading: {}",
            path.display()
        )));
    }
    Ok((
        Asset {
            relative_path: relative_path.components().collect(),
            bytes,
            sha256,
            file_id,
            parent_id,
        },
        details,
    ))
}

#[cfg(target_os = "linux")]
fn linux_source(root: &Path, relative: &Path) -> io::Result<(fs::File, FileId)> {
    let root = linux::PinnedDirectory::open(root)?;
    let parent = root.open_beneath(relative.parent().expect("validated relative path"))?;
    let file = parent.open_read(relative.file_name().expect("validated file name"))?;
    Ok((file, parent.id))
}

pub fn scan(root: &Path, selected: &[PathBuf]) -> io::Result<Scan> {
    Ok(inspect_selection(root, selected, |_, _, _| Ok(()), |_, _| Ok(()))?.0)
}

fn inspect_selection<T>(
    root: &Path,
    selected: &[PathBuf],
    mut inspect: impl FnMut(&mut fs::File, &Metadata, &str) -> io::Result<T>,
    mut after: impl FnMut(&Asset, &T) -> io::Result<()>,
) -> io::Result<(Scan, Vec<T>)> {
    if selected.is_empty() {
        return Err(invalid("Select at least one regular file"));
    }
    let root = checked_root(root)?;
    let mut paths = BTreeSet::new();
    let mut assets = Vec::with_capacity(selected.len());
    let mut details = Vec::with_capacity(selected.len());
    for path in selected {
        let (asset, detail) = inspect_asset(&root, path, &mut inspect)?;
        if !paths.insert(asset.relative_path.to_string_lossy().to_lowercase()) {
            return Err(invalid(format!("Repeated selection: {}", path.display())));
        }
        after(&asset, &detail)?;
        assets.push(asset);
        details.push(detail);
    }
    Ok((
        Scan {
            schema_version: SCHEMA_VERSION,
            root,
            assets,
        },
        details,
    ))
}

pub fn plan(
    source_root: &Path,
    destination_root: &Path,
    destination_dir: &Path,
    selected: &[PathBuf],
) -> io::Result<Plan> {
    build_plan(scan(source_root, selected)?, destination_root, |_| {
        Ok((destination_dir.to_path_buf(), Output::Filing(None)))
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NarrationStageReceipt {
    version: u32,
    stage: PathBuf,
    files: BTreeMap<PathBuf, String>,
}

pub fn narration_delivery_plan(
    stage: &Path,
    destination: &Path,
    receipt: &Path,
) -> io::Result<Plan> {
    let root = receipt
        .parent()
        .ok_or_else(|| invalid("Expected an absolute staging receipt path"))?;
    let name = receipt
        .file_name()
        .ok_or_else(|| invalid("Expected a staging receipt file"))?;
    let root = checked_root(root)?;
    let (_, receipt): (_, NarrationStageReceipt) = library::read_json(&root, Path::new(name))?;
    if receipt.version != 1 || !receipt.files.contains_key(Path::new("job.json")) {
        return Err(invalid("Invalid narration staging receipt"));
    }
    let selected: Vec<_> = receipt.files.keys().cloned().collect();
    if selected.iter().any(|path| path.components().count() != 1) {
        return Err(invalid(
            "Narration staging receipt requires flat file names",
        ));
    }
    let source = scan(stage, &selected)?;
    if source.root != checked_root(&receipt.stage)? {
        return Err(invalid("Staging receipt describes a different stage"));
    }
    for asset in &source.assets {
        if receipt.files.get(&asset.relative_path) != Some(&asset.sha256) {
            return Err(invalid(format!(
                "File differs from the reviewed narration stage: {}",
                asset.relative_path.display()
            )));
        }
    }
    build_plan(source, destination, |_| {
        Ok((PathBuf::from("."), Output::Filing(None)))
    })
}

enum Output {
    Filing(Option<String>),
    Caption(String),
    Audio(String, recording::AudioRecipe),
    Clip(String, recording::ClipRecipe),
}

fn build_plan(
    source: Scan,
    destination_root: &Path,
    mut filing: impl FnMut(&Asset) -> io::Result<(PathBuf, Output)>,
) -> io::Result<Plan> {
    let destination_root = checked_root(destination_root)?;
    let same_root = validate_root_pair(&source.root, &destination_root)?;
    #[cfg(windows)]
    let (source_root_id, destination_root_id) = (
        Some(windows::PinnedDirectory::open(&source.root)?.id),
        Some(windows::PinnedDirectory::open(&destination_root)?.id),
    );
    #[cfg(target_os = "linux")]
    let destination_pin = linux::PinnedDirectory::open(&destination_root)?;
    #[cfg(target_os = "linux")]
    let (source_root_id, destination_root_id) = (
        Some(linux::PinnedDirectory::open(&source.root)?.id),
        Some(destination_pin.id),
    );
    #[cfg(target_os = "linux")]
    if !same_root && source_root_id == destination_root_id {
        return Err(invalid(
            "Distinct roots must not refer to the same directory",
        ));
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    let (source_root_id, destination_root_id) = (None, None);
    let mut directories = BTreeMap::new();
    let mut destinations = BTreeSet::new();
    let mut actions = Vec::with_capacity(source.assets.len());
    for asset in source.assets {
        let (dir, output) = filing(&asset)?;
        let dir: PathBuf = if dir == Path::new(".") {
            PathBuf::new()
        } else {
            validate_relative(&dir)?;
            dir.components().collect()
        };
        let (parent_id, names) = match directories.entry(dir.clone()) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                #[cfg(not(target_os = "linux"))]
                let target = destination_root.join(&dir);
                #[cfg(not(target_os = "linux"))]
                if !checked_path(&target)?.is_dir() {
                    return Err(invalid(format!(
                        "Expected destination directory: {}",
                        target.display()
                    )));
                }
                #[cfg(windows)]
                let parent_id = Some(windows::PinnedDirectory::open(&target)?.id);
                #[cfg(not(any(windows, target_os = "linux")))]
                let parent_id = None;
                #[cfg(not(target_os = "linux"))]
                let names = fs::read_dir(target)?
                    .map(|entry| Ok(entry?.file_name().to_string_lossy().to_lowercase()))
                    .collect::<io::Result<BTreeSet<_>>>()?;
                #[cfg(target_os = "linux")]
                let (parent_id, names) = {
                    let target = destination_pin.open_beneath(&dir)?;
                    (Some(target.id), target.folded_names()?)
                };
                entry.insert((parent_id, names))
            }
        };
        let source_name = asset
            .relative_path
            .file_name()
            .expect("validated relative path");
        let (destination, caption, audio, clip) = match output {
            Output::Filing(rename) => {
                let name = rename.as_deref().map_or(source_name, std::ffi::OsStr::new);
                if !valid_destination_name(&asset.relative_path, name) {
                    return Err(invalid(
                        "Destination must be a safe basename with the source extension",
                    ));
                }
                (dir.join(name), None, None, None)
            }
            Output::Caption(text) => (
                caption_path(&asset.relative_path, &text)?,
                Some(text),
                None,
                None,
            ),
            Output::Audio(name, recipe) => (dir.join(name), None, Some(recipe), None),
            Output::Clip(name, recipe) => (dir.join(name), None, None, Some(recipe)),
        };
        let name = destination.file_name().expect("validated output path");
        if names.contains(&name.to_string_lossy().to_lowercase())
            || !destinations.insert(destination.to_string_lossy().to_lowercase())
        {
            return Err(invalid(format!(
                "Destination name collision: {}",
                destination.display()
            )));
        }
        #[cfg(not(target_os = "linux"))]
        require_absent(&destination_root.join(&destination))?;
        #[cfg(target_os = "linux")]
        {
            let target = destination_pin
                .open_beneath(destination.parent().expect("validated output path"))?;
            if Some(target.id) != *parent_id {
                return Err(invalid("Destination directory changed during planning"));
            }
            target.require_absent(name)?;
        }
        actions.push(Action {
            id: u32::try_from(actions.len() + 1).map_err(io::Error::other)?,
            source: asset,
            destination,
            destination_parent_id: *parent_id,
            placement: if clip.is_some() {
                Placement::RenderClip
            } else if audio.is_some() {
                Placement::ExtractAudio
            } else if caption.is_some() {
                Placement::Caption
            } else if same_root {
                Placement::MoveIfSameFilesystemElseCopyRetain
            } else {
                Placement::CopyRetain
            },
            caption,
            audio,
            clip,
        });
    }
    let body = PlanBody {
        schema_version: SCHEMA_VERSION,
        source_root: source.root,
        destination_root,
        source_root_id,
        destination_root_id,
        actions,
    };
    let sha256 = plan_digest(&body)?;
    Ok(Plan { sha256, body })
}

fn caption_path(source: &Path, text: &str) -> io::Result<PathBuf> {
    if text.trim().is_empty()
        || text.len() > 4096
        || text
            .chars()
            .any(|c| c.is_control() && !matches!(c, '\n' | '\t'))
    {
        return Err(invalid(
            "Caption must be nonblank UTF-8 up to 4096 bytes with no control characters except tab and newline",
        ));
    }
    let name = source
        .file_name()
        .and_then(std::ffi::OsStr::to_str)
        .ok_or_else(|| invalid("Expected a valid captioned filename"))?;
    if name.to_ascii_lowercase().ends_with(".caption.txt") {
        return Err(invalid("A caption sidecar cannot be captioned"));
    }
    let destination = source.with_file_name(format!("{name}.caption.txt"));
    validate_relative(&destination)?;
    Ok(destination)
}

fn valid_destination_name(source: &Path, name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(portable_component)
        && match (source.extension(), Path::new(name).extension()) {
            (Some(original), Some(proposed)) => original.eq_ignore_ascii_case(proposed),
            (None, None) => true,
            _ => false,
        }
}

fn validate_root_pair(source: &Path, destination: &Path) -> io::Result<bool> {
    let same_root = source == destination;
    // Reject ambiguous overlaps across case variants as well.
    let source_folded = PathBuf::from(source.to_string_lossy().to_lowercase());
    let destination_folded = PathBuf::from(destination.to_string_lossy().to_lowercase());
    if !same_root
        && (source_folded.starts_with(&destination_folded)
            || destination_folded.starts_with(&source_folded))
    {
        return Err(invalid("Distinct roots must not overlap"));
    }
    Ok(same_root)
}

#[cfg(not(target_os = "linux"))]
fn require_absent(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(_) => Err(invalid(format!(
            "Destination already exists: {}",
            path.display()
        ))),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn plan_digest(body: &PlanBody) -> io::Result<String> {
    let payload = serde_json::to_value(body)?;
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&payload)?)
    ))
}
