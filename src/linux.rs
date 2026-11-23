use crate::{FileId, invalid};
use rustix::{
    fs::{
        AtFlags, Dir, FileType, FlockOperation, Mode, OFlags, RenameFlags, flock, fstatfs, open,
        openat, renameat_with, statat,
    },
    io::Errno,
    process::{geteuid, getpgrp},
    termios::{QueueSelector, tcflush, tcgetpgrp},
};
use std::{
    collections::BTreeSet,
    env,
    ffi::OsStr,
    fs::File,
    io::{self, BufRead, BufReader, Read, Write},
    os::unix::fs::MetadataExt,
    path::{Component, Path},
};

const BTRFS: u32 = 0x9123_683e;

pub(crate) fn terminal_approval(review: &[u8]) -> io::Result<String> {
    let unavailable = || invalid("Adapter approval requires a foreground controlling terminal");
    let mut tty = File::from(
        open("/dev/tty", OFlags::RDWR | OFlags::CLOEXEC, Mode::empty())
            .map_err(|_| unavailable())?,
    );
    if tcgetpgrp(&tty).map_err(|_| unavailable())? != getpgrp() {
        return Err(unavailable());
    }
    flock(&tty, FlockOperation::NonBlockingLockExclusive)
        .map_err(|_| invalid("Another adapter approval is active"))?;
    tcflush(&tty, QueueSelector::IFlush)?;
    tty.write_all(review)?;
    tty.flush()?;
    let mut answer = String::new();
    BufReader::new(&tty)
        .take(16 * 1024)
        .read_line(&mut answer)?;
    Ok(answer)
}

const DIRECTORY: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);

pub(crate) struct PinnedDirectory {
    file: File,
    pub(crate) id: FileId,
}

impl PinnedDirectory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let relative = path
            .strip_prefix("/")
            .map_err(|_| invalid("Expected absolute Linux directory"))?;
        Self::descend(File::from(open("/", DIRECTORY, Mode::empty())?), relative)
    }

    pub(crate) fn open_beneath(&self, path: &Path) -> io::Result<Self> {
        Self::descend(self.file.try_clone()?, path)
    }

    fn descend(mut file: File, path: &Path) -> io::Result<Self> {
        require_native(&file)?;
        for component in path.components() {
            let Component::Normal(name) = component else {
                return Err(invalid("Expected normal Linux directory components"));
            };
            file = File::from(
                openat(&file, name, DIRECTORY, Mode::empty()).map_err(|error| match error {
                    Errno::LOOP | Errno::NOTDIR => invalid(format!(
                        "Expected a directory without links: {}",
                        name.to_string_lossy()
                    )),
                    _ => error.into(),
                })?,
            );
            require_native(&file)?;
        }
        Ok(Self {
            id: identity(&file)?,
            file,
        })
    }

    pub(crate) fn open_read(&self, name: &OsStr) -> io::Result<File> {
        self.open_regular(name, OFlags::RDONLY)
    }

    fn open_regular(&self, name: &OsStr, flags: OFlags) -> io::Result<File> {
        single_component(name)?;
        let metadata = statat(&self.file, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if !FileType::from_raw_mode(metadata.st_mode).is_file() {
            return Err(invalid("Expected a regular file without links"));
        }
        let file = File::from(
            openat(
                &self.file,
                name,
                flags | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
                Mode::empty(),
            )
            .map_err(|error| match error {
                Errno::LOOP => invalid("Expected a regular file without links"),
                _ => error.into(),
            })?,
        );
        require_native(&file)?;
        if !file.metadata()?.is_file() {
            return Err(invalid("Expected a regular file without links"));
        }
        Ok(file)
    }

    pub(crate) fn folded_names(&self) -> io::Result<BTreeSet<String>> {
        Dir::read_from(&self.file)?
            .map(|entry| Ok(entry?.file_name().to_string_lossy().to_lowercase()))
            .collect()
    }

    pub(crate) fn require_absent(&self, name: &OsStr) -> io::Result<()> {
        match statat(&self.file, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Err(invalid(format!(
                "Destination already exists: {}",
                name.to_string_lossy()
            ))),
            Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn open_execution(path: &Path) -> io::Result<Self> {
        let (root, mut directory) = execution_root()?;
        let relative = path
            .strip_prefix(root)
            .map_err(|_| invalid("Path is outside INTAKE_EXECUTION_ROOT"))?;
        for component in relative.components() {
            let Component::Normal(name) = component else {
                return Err(invalid("Expected a normal execution path component"));
            };
            directory = directory.open_beneath(Path::new(name))?;
            directory.require_private()?;
        }
        Ok(directory)
    }

    fn require_private(&self) -> io::Result<()> {
        if fstatfs(&self.file)?.f_type as u32 != BTRFS {
            return Err(invalid("Linux execution requires native btrfs directories"));
        }
        let metadata = self.file.metadata()?;
        if metadata.uid() != geteuid().as_raw() || metadata.mode() & 0o077 != 0 {
            return Err(invalid(
                "Linux execution directories must be private and owned by the operator",
            ));
        }
        Ok(())
    }

    pub(crate) fn present(&self, name: &OsStr) -> io::Result<bool> {
        single_component(name)?;
        match statat(&self.file, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Ok(true),
            Err(Errno::NOENT) => Ok(false),
            Err(error) => Err(error.into()),
        }
    }

    pub(crate) fn open_file(&self, name: &OsStr, access: Access) -> io::Result<File> {
        single_component(name)?;
        let file = match access {
            Access::Read | Access::Move => self.open_read(name)?,
            Access::StageExisting | Access::JournalExisting => {
                self.open_regular(name, OFlags::RDWR)?
            }
            Access::StageNew | Access::JournalNew => File::from(openat(
                &self.file,
                name,
                OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )?),
        };
        let metadata = file.metadata()?;
        if metadata.uid() != geteuid().as_raw()
            || metadata.mode() & 0o022 != 0
            || metadata.nlink() != 1
        {
            return Err(invalid(
                "Linux execution requires owner-controlled files without hard links",
            ));
        }
        Ok(file)
    }

    pub(crate) fn sync(&self) -> io::Result<()> {
        self.file.sync_all()
    }
}

fn single_component(name: &OsStr) -> io::Result<()> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(part)) if part == name)
        || components.next().is_some()
    {
        return Err(invalid("Expected a single Linux file component"));
    }
    Ok(())
}

fn execution_root() -> io::Result<(std::path::PathBuf, PinnedDirectory)> {
    let root = env::var_os("INTAKE_EXECUTION_ROOT")
        .map(std::path::PathBuf::from)
        .ok_or_else(|| {
            invalid("Set INTAKE_EXECUTION_ROOT to an existing private btrfs directory")
        })?;
    let root = crate::checked_root(&root)?;
    let pin = PinnedDirectory::open(&root)?;
    pin.require_private()?;
    Ok((root, pin))
}

pub(crate) struct RunGuard {
    _directory: PinnedDirectory,
}

impl RunGuard {
    pub(crate) fn acquire() -> io::Result<Self> {
        let (_, directory) = execution_root()?;
        flock(&directory.file, FlockOperation::NonBlockingLockExclusive)?;
        Ok(Self {
            _directory: directory,
        })
    }
}

pub(crate) enum Access {
    Read,
    Move,
    StageNew,
    StageExisting,
    JournalNew,
    JournalExisting,
}

pub(crate) fn publish(
    file: &File,
    source: &PinnedDirectory,
    name: &OsStr,
    destination: &PinnedDirectory,
    output: &OsStr,
) -> io::Result<()> {
    single_component(output)?;
    file.sync_all()?;
    if destination
        .folded_names()?
        .contains(&output.to_string_lossy().to_lowercase())
    {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "Destination already exists",
        ));
    }
    if identity(&source.open_file(name, Access::Read)?)? != identity(file)? {
        return Err(invalid("Publication source identity changed"));
    }
    renameat_with(
        &source.file,
        name,
        &destination.file,
        output,
        RenameFlags::NOREPLACE,
    )?;
    Ok(())
}

fn require_native(file: &File) -> io::Result<()> {
    // Some network filesystems resolve links on the server.
    const EXT: u32 = 0xef53;
    const XFS: u32 = 0x5846_5342;
    const TMPFS: u32 = 0x0102_1994;
    const OVERLAY: u32 = 0x794c_7630;
    if !matches!(
        fstatfs(file)?.f_type as u32,
        EXT | XFS | BTRFS | TMPFS | OVERLAY
    ) {
        return Err(invalid(
            "Unsupported Linux filesystem for safe path validation",
        ));
    }
    Ok(())
}

pub(crate) fn identity(file: &File) -> io::Result<FileId> {
    let metadata = file.metadata()?;
    let mut id = [0; 16];
    id[..8].copy_from_slice(&metadata.ino().to_be_bytes());
    Ok(FileId {
        volume: metadata.dev().to_be_bytes(),
        file: id,
    })
}
