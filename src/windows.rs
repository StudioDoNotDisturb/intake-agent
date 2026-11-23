use crate::{FileId, invalid};
use std::{
    ffi::OsStr,
    fs::{File, OpenOptions},
    io,
    mem::{offset_of, size_of},
    os::windows::{
        ffi::OsStrExt,
        fs::OpenOptionsExt,
        io::{AsRawHandle, FromRawHandle},
    },
    path::{Component, Path, Prefix},
    ptr::{null, null_mut},
};
use windows_sys::{
    Wdk::{
        Foundation::OBJECT_ATTRIBUTES,
        Storage::FileSystem::{
            FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
            FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
            FileRenameInformation, NtCreateFile, NtSetInformationFile,
        },
    },
    Win32::{
        Foundation::{
            CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, OBJ_CASE_INSENSITIVE,
            RtlNtStatusToDosError, UNICODE_STRING, WAIT_ABANDONED, WAIT_OBJECT_0, WAIT_TIMEOUT,
        },
        Storage::FileSystem::*,
        System::{
            Console::{
                AllocConsole, FreeConsole, GetStdHandle, ReadConsoleW, STD_ERROR_HANDLE,
                STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, SetConsoleTitleW, SetStdHandle, WriteConsoleW,
            },
            IO::IO_STATUS_BLOCK,
            RemoteDesktop::ProcessIdToSessionId,
            Threading::{CreateMutexW, ReleaseMutex, WaitForSingleObject},
            WindowsProgramming::{DRIVE_FIXED, DRIVE_REMOTE},
        },
    },
};

pub(crate) const APPROVAL_CONSOLE_TITLE: &str = "intake-agent Operator Approval";

pub(crate) fn desktop_approval(review: &str) -> io::Result<String> {
    const STREAMS: [u32; 3] = [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE];
    let mut session = 0;
    if unsafe { ProcessIdToSessionId(std::process::id(), &mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let handles = STREAMS.map(|stream| unsafe { GetStdHandle(stream) });
    if session == 0
        || handles[1..]
            .iter()
            .any(|handle| unsafe { GetFileType(*handle) } != FILE_TYPE_PIPE)
    {
        return Err(invalid(
            "Desktop approval requires an interactive Windows session and redirected RPC output",
        ));
    }

    // Detach when approval ends so the subprocess's RPC pipes stay separate from review.
    struct Desktop([HANDLE; 3]);
    impl Drop for Desktop {
        fn drop(&mut self) {
            unsafe { FreeConsole() };
            for (stream, handle) in STREAMS.into_iter().zip(self.0) {
                unsafe { SetStdHandle(stream, handle) };
            }
        }
    }
    if unsafe { FreeConsole() } == 0 {
        return Err(io::Error::last_os_error());
    }
    let _desktop = Desktop(handles);
    if unsafe { AllocConsole() } == 0 {
        return Err(io::Error::last_os_error());
    }
    for (stream, handle) in STREAMS.into_iter().zip(handles) {
        if unsafe { SetStdHandle(stream, handle) } == 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let title = wide(OsStr::new(APPROVAL_CONSOLE_TITLE));
    if unsafe { SetConsoleTitleW(title.as_ptr()) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let input = File::open("CONIN$")?;
    let output = OpenOptions::new().write(true).open("CONOUT$")?;
    let message: Vec<_> = review.replace('\n', "\r\n").encode_utf16().collect();
    for mut chunk in message.chunks(4096) {
        while !chunk.is_empty() {
            let mut count = 0;
            if unsafe {
                WriteConsoleW(
                    output.as_raw_handle(),
                    chunk.as_ptr(),
                    chunk.len() as u32,
                    &mut count,
                    null(),
                )
            } == 0
            {
                return Err(io::Error::last_os_error());
            }
            if count == 0 {
                return Err(io::ErrorKind::WriteZero.into());
            }
            chunk = &chunk[count as usize..];
        }
    }
    let mut answer = [0u16; 16 * 1024];
    let mut count = 0;
    if unsafe {
        ReadConsoleW(
            input.as_raw_handle(),
            answer.as_mut_ptr().cast(),
            answer.len() as u32,
            &mut count,
            null(),
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    String::from_utf16(&answer[..count as usize]).map_err(|_| invalid("Invalid approval text"))
}

pub(crate) struct RunGuard(HANDLE);

impl RunGuard {
    pub(crate) fn acquire() -> io::Result<Self> {
        let name = wide(OsStr::new(r"Global\IntakeAgentLocalExecution-v1"));
        // Each thread acquires and releases the mutex on the same thread.
        let handle = unsafe { CreateMutexW(null(), 0, name.as_ptr()) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        let error = match unsafe { WaitForSingleObject(handle, 0) } {
            WAIT_OBJECT_0 | WAIT_ABANDONED => return Ok(Self(handle)),
            WAIT_TIMEOUT => io::Error::new(
                io::ErrorKind::WouldBlock,
                "Another intake run holds the execution lock",
            ),
            _ => io::Error::last_os_error(),
        };
        unsafe { CloseHandle(handle) };
        Err(error)
    }
}

impl Drop for RunGuard {
    fn drop(&mut self) {
        unsafe {
            ReleaseMutex(self.0);
            CloseHandle(self.0);
        }
    }
}

fn wide(value: &OsStr) -> Vec<u16> {
    value.encode_wide().chain([0]).collect()
}

pub(crate) fn require_local_path(path: &Path) -> io::Result<()> {
    let remote = match path.components().next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::UNC(..) | Prefix::VerbatimUNC(..) => true,
            Prefix::Disk(drive) | Prefix::VerbatimDisk(drive) => {
                let root = [u16::from(drive), b':' as u16, b'\\' as u16, 0];
                unsafe { GetDriveTypeW(root.as_ptr()) == DRIVE_REMOTE }
            }
            _ => false,
        },
        _ => false,
    };
    if remote {
        // A server can resolve POSIX links without exposing a Windows reparse point.
        return Err(invalid(
            "Network roots are unsupported for safe path validation",
        ));
    }
    Ok(())
}

pub(crate) struct PinnedDirectory {
    ancestors: Vec<File>,
    pub(crate) id: FileId,
}

impl PinnedDirectory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(invalid("Expected absolute directory"));
        }
        let mut ancestors: Vec<_> = path.ancestors().filter(|p| p.has_root()).collect();
        ancestors.reverse();
        let mut handles = Vec::with_capacity(ancestors.len());
        for ancestor in ancestors {
            let rights = FILE_LIST_DIRECTORY | FILE_TRAVERSE | FILE_READ_ATTRIBUTES | SYNCHRONIZE;
            // Deny deleting or renaming directories while allowing a rename into them.
            let sharing = FILE_SHARE_READ | FILE_SHARE_WRITE;
            let directory = if let Some(parent) = handles.last() {
                let name = ancestor
                    .file_name()
                    .ok_or_else(|| invalid("Missing directory name"))?;
                open_relative(
                    parent,
                    name,
                    rights,
                    sharing,
                    FILE_OPEN,
                    FILE_DIRECTORY_FILE,
                )?
            } else {
                OpenOptions::new()
                    .access_mode(rights)
                    .share_mode(sharing)
                    .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
                    .open(ancestor)?
            };
            reject_reparse(&directory)?;
            handles.push(directory);
        }
        let id = identity(
            handles
                .last()
                .ok_or_else(|| invalid("No directory handle"))?,
        )?;
        Ok(Self {
            ancestors: handles,
            id,
        })
    }

    pub(crate) fn require_local_ntfs(&self, path: &Path) -> io::Result<()> {
        let volume = path
            .ancestors()
            .filter(|p| p.has_root())
            .last()
            .ok_or_else(|| invalid("Expected an absolute fixed-drive path"))?;
        if unsafe { GetDriveTypeW(wide(volume.as_os_str()).as_ptr()) } != DRIVE_FIXED {
            return Err(invalid("Execution requires a local fixed drive"));
        }
        let root = self
            .ancestors
            .first()
            .ok_or_else(|| invalid("No volume handle"))?;
        let mut name = [0u16; 32];
        if unsafe {
            GetVolumeInformationByHandleW(
                root.as_raw_handle(),
                null_mut(),
                0,
                null_mut(),
                null_mut(),
                null_mut(),
                name.as_mut_ptr(),
                name.len() as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        if name[..5] != [b'N' as u16, b'T' as u16, b'F' as u16, b'S' as u16, 0] {
            return Err(invalid("Execution requires NTFS"));
        }
        Ok(())
    }

    fn directory(&self) -> &File {
        self.ancestors.last().expect("constructed with a directory")
    }

    pub(crate) fn open_file(&self, name: &OsStr, access: Access) -> io::Result<File> {
        let (rights, disposition) = match access {
            Access::Read => (GENERIC_READ, FILE_OPEN),
            Access::Move => (GENERIC_READ | DELETE, FILE_OPEN),
            Access::StageNew => (GENERIC_READ | GENERIC_WRITE | DELETE, FILE_CREATE),
            Access::StageExisting => (GENERIC_READ | GENERIC_WRITE | DELETE, FILE_OPEN),
            Access::JournalNew => (GENERIC_READ | GENERIC_WRITE, FILE_CREATE),
            Access::JournalExisting => (GENERIC_READ | GENERIC_WRITE, FILE_OPEN),
        };
        let file = open_relative(
            self.directory(),
            name,
            rights | SYNCHRONIZE,
            FILE_SHARE_READ,
            disposition,
            FILE_NON_DIRECTORY_FILE,
        )?;
        reject_reparse(&file)?;
        Ok(file)
    }

    pub(crate) fn present(&self, name: &OsStr) -> io::Result<bool> {
        match open_relative(
            self.directory(),
            name,
            FILE_READ_ATTRIBUTES | SYNCHRONIZE,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            0,
        ) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }
}

fn single_component(name: &OsStr) -> io::Result<()> {
    let mut components = Path::new(name).components();
    if !matches!(components.next(), Some(Component::Normal(part)) if part == name)
        || components.next().is_some()
        || name.encode_wide().any(|c| matches!(c, 0 | 58))
    {
        return Err(invalid("Expected a single native path component"));
    }
    Ok(())
}

fn open_relative(
    parent: &File,
    name: &OsStr,
    access: u32,
    sharing: u32,
    disposition: u32,
    options: u32,
) -> io::Result<File> {
    single_component(name)?;
    let mut name = wide(name);
    let length = u16::try_from((name.len() - 1) * size_of::<u16>()).map_err(io::Error::other)?;
    let mut string = UNICODE_STRING {
        Length: length,
        MaximumLength: length,
        Buffer: name.as_mut_ptr(),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
        RootDirectory: parent.as_raw_handle(),
        ObjectName: &mut string,
        Attributes: OBJ_CASE_INSENSITIVE,
        ..OBJECT_ATTRIBUTES::default()
    };
    let mut result = null_mut();
    let mut status = IO_STATUS_BLOCK::default();
    // Open one name relative to a live directory handle and inspect the opened object.
    let code = unsafe {
        NtCreateFile(
            &mut result,
            access,
            &attributes,
            &mut status,
            null(),
            FILE_ATTRIBUTE_NORMAL,
            sharing,
            disposition,
            options | FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT,
            null(),
            0,
        )
    };
    if code < 0 {
        return Err(io::Error::from_raw_os_error(
            unsafe { RtlNtStatusToDosError(code) } as i32,
        ));
    }
    Ok(unsafe { File::from_raw_handle(result) })
}

fn reject_reparse(file: &File) -> io::Result<()> {
    let mut info = FILE_ATTRIBUTE_TAG_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileAttributeTagInfo,
            (&mut info as *mut FILE_ATTRIBUTE_TAG_INFO).cast(),
            size_of::<FILE_ATTRIBUTE_TAG_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(invalid("Link or reparse point refused"));
    }
    Ok(())
}

pub(crate) fn identity(file: &File) -> io::Result<FileId> {
    let mut id = FILE_ID_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileIdInfo,
            (&mut id as *mut FILE_ID_INFO).cast(),
            size_of::<FILE_ID_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(FileId {
        volume: id.VolumeSerialNumber.to_be_bytes(),
        file: id.FileId.Identifier,
    })
}

pub(crate) fn require_single_link(file: &File) -> io::Result<()> {
    let mut info = FILE_STANDARD_INFO::default();
    if unsafe {
        GetFileInformationByHandleEx(
            file.as_raw_handle(),
            FileStandardInfo,
            (&mut info as *mut FILE_STANDARD_INFO).cast(),
            size_of::<FILE_STANDARD_INFO>() as u32,
        )
    } == 0
    {
        return Err(io::Error::last_os_error());
    }
    if info.NumberOfLinks != 1 {
        return Err(invalid("Checkpoint must not have hard links"));
    }
    Ok(())
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
    parent: &PinnedDirectory,
    destination: &OsStr,
) -> io::Result<()> {
    single_component(destination)?;
    let name = wide(destination);
    let name_bytes = (name.len() - 1) * size_of::<u16>();
    let bytes = (offset_of!(FILE_RENAME_INFORMATION, FileName) + name.len() * size_of::<u16>())
        .max(size_of::<FILE_RENAME_INFORMATION>());
    // usize storage aligns the native struct. The zeroed union requests no replacement.
    let mut buffer = vec![0usize; bytes.div_ceil(size_of::<usize>())];
    let request = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
    let mut status = IO_STATUS_BLOCK::default();
    unsafe {
        (*request).FileNameLength = u32::try_from(name_bytes).map_err(io::Error::other)?;
        (*request).RootDirectory = parent.directory().as_raw_handle();
        let filename = request
            .cast::<u8>()
            .add(offset_of!(FILE_RENAME_INFORMATION, FileName))
            .cast();
        std::ptr::copy_nonoverlapping(name.as_ptr(), filename, name.len());
        let code = NtSetInformationFile(
            file.as_raw_handle(),
            &mut status,
            request.cast(),
            u32::try_from(bytes).map_err(io::Error::other)?,
            FileRenameInformation,
        );
        if code < 0 {
            return Err(io::Error::from_raw_os_error(
                RtlNtStatusToDosError(code) as i32
            ));
        }
    }
    Ok(())
}
