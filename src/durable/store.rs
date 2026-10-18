//! Synchronous persistence owned by the queue's blocking operation.

use super::{QueueCodec, QueueError, QueueErrorKind, QueueOptions, QueueSnapshot};
use camino::{Utf8Path, Utf8PathBuf};
use fs2::FileExt;
#[cfg(unix)]
use std::{
    ffi::{CStr, CString},
    io::Read,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{
            ffi::OsStrExt,
            fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
        },
    },
    path::Component,
};
use std::{
    fs::{self, File, OpenOptions},
    hash::Hash,
    io,
    sync::Arc,
};

#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PersistFailure {
    BeforeWrite,
    BeforeRename,
    AfterRename,
}

pub(super) struct DurableStore<P, K> {
    pub(super) state: QueueSnapshot<P, K>,
    pub(super) options: QueueOptions<K>,
    codec: Arc<dyn QueueCodec<Payload = P, Key = K>>,
    path: Utf8PathBuf,
    temporary_path: Utf8PathBuf,
    directory: Utf8PathBuf,
    directory_sync_pending: bool,
    _lock_file: File,
    #[cfg(test)]
    pub(super) persist_failure: Option<PersistFailure>,
    #[cfg(test)]
    pub(super) directory_sync_attempts: usize,
}

impl<P: Clone, K: Clone + Eq + Hash> DurableStore<P, K> {
    pub(super) fn load(
        directory: &Utf8Path,
        codec: Arc<dyn QueueCodec<Payload = P, Key = K>>,
        options: QueueOptions<K>,
    ) -> Result<Self, QueueError> {
        let valid_name = |name: &str| {
            !name.contains(['/', '\\', ':'])
                && Utf8Path::new(name).file_name() == Some(name)
                && Utf8Path::new(name).components().count() == 1
        };
        let temporary_name = format!("{}.tmp", options.file_name);
        if !valid_name(options.file_name)
            || !valid_name(options.lock_name)
            || options.file_name == options.lock_name
            || options.lock_name == temporary_name
        {
            return Err(QueueError::at(
                "validate durable storage layout",
                directory,
                QueueErrorKind::InvalidStorageLayout,
            ));
        }
        fs::create_dir_all(directory).map_err(|source| {
            QueueError::at(
                "create durable directory",
                directory,
                QueueErrorKind::Io(source),
            )
        })?;
        let lock_path = directory.join(options.lock_name);
        #[cfg(unix)]
        let directory_file = open_startup_directory(directory).map_err(|source| {
            QueueError::at(
                "open durable directory",
                directory,
                QueueErrorKind::Io(source),
            )
        })?;
        #[cfg(unix)]
        let lock_file = open_startup_entry(
            &directory_file,
            options.lock_name,
            libc::O_RDWR | libc::O_CREAT,
            0o600,
        )
        .and_then(|file| {
            validate_startup_file(&file, private_directory(&directory_file)?)?;
            Ok(file)
        })
        .map_err(|source| {
            QueueError::at("open durable lock", &lock_path, QueueErrorKind::Io(source))
        })?;
        #[cfg(not(unix))]
        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .map_err(|source| {
                QueueError::at("open durable lock", &lock_path, QueueErrorKind::Io(source))
            })?;
        FileExt::try_lock_exclusive(&lock_file).map_err(|source| {
            let kind = if source.raw_os_error() == fs2::lock_contended_error().raw_os_error() {
                QueueErrorKind::Locked(source)
            } else {
                QueueErrorKind::Io(source)
            };
            QueueError::at("acquire durable lock", &lock_path, kind)
        })?;
        let path = directory.join(options.file_name);
        let temporary_path = directory.join(&temporary_name);
        #[cfg(unix)]
        let snapshot_bytes = read_startup_snapshot(
            directory,
            &directory_file,
            &lock_file,
            options.lock_name,
            options.file_name,
            &temporary_name,
        );
        #[cfg(not(unix))]
        let snapshot_bytes = match fs::read(&path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(source) if source.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(source) => Err(source),
        };
        let (state, directory_sync_pending) = match snapshot_bytes {
            Ok(Some(bytes)) => (
                codec.decode(&bytes).map_err(|source| {
                    QueueError::at(
                        "decode durable snapshot",
                        &path,
                        QueueErrorKind::Codec(source),
                    )
                })?,
                true,
            ),
            Ok(None) => (QueueSnapshot::default(), false),
            Err(source) => {
                return Err(QueueError::at(
                    "read durable snapshot",
                    &path,
                    QueueErrorKind::Io(source),
                ));
            }
        };
        super::engine::validate_snapshot(&state, &options).map_err(|mut error| {
            error.operation = "validate durable snapshot";
            error.path = Some(path.clone());
            error
        })?;
        Ok(Self {
            state,
            options,
            codec,
            path,
            temporary_path,
            directory: directory.to_owned(),
            directory_sync_pending,
            _lock_file: lock_file,
            #[cfg(test)]
            persist_failure: None,
            #[cfg(test)]
            directory_sync_attempts: 0,
        })
    }

    pub(super) fn transaction<T>(
        &mut self,
        mutate: impl FnOnce(&mut QueueSnapshot<P, K>, &QueueOptions<K>) -> Result<T, QueueError>,
    ) -> Result<T, QueueError> {
        self.sync_prior()?;
        let previous = self.state.clone();
        let result = match mutate(&mut self.state, &self.options) {
            Ok(result) => result,
            Err(error) => {
                self.state = previous;
                return Err(error);
            }
        };
        match self.persist() {
            Ok(()) => Ok(result),
            Err(error) => {
                if !matches!(&error.kind, QueueErrorKind::DurabilityUncertain(_)) {
                    self.state = previous;
                }
                Err(error)
            }
        }
    }

    pub(super) fn sync_prior(&mut self) -> Result<(), QueueError> {
        self.sync_directory().map_err(|source| {
            QueueError::at(
                "sync prior durable commit",
                &self.path,
                QueueErrorKind::PriorSync(source),
            )
        })
    }

    fn persist(&mut self) -> Result<(), QueueError> {
        let bytes = self.codec.encode(&self.state).map_err(|source| {
            QueueError::at(
                "encode durable snapshot",
                &self.path,
                QueueErrorKind::Codec(source),
            )
        })?;
        #[cfg(test)]
        self.fail_persist_at(PersistFailure::BeforeWrite)
            .map_err(|source| self.temporary_error(source))?;
        // Do not truncate a stale inode before checking its private boundary.
        let mut options = OpenOptions::new();
        options.create(true).write(true).truncate(false);
        #[cfg(unix)]
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
        let mut temporary = options
            .open(&self.temporary_path)
            .map_err(|source| self.temporary_error(source))?;
        #[cfg(unix)]
        {
            validate_private_file(&temporary).map_err(|source| self.temporary_error(source))?;
        }
        temporary
            .set_len(0)
            .map_err(|source| self.temporary_error(source))?;
        let temporary_path = self.temporary_path.clone();
        let path = self.path.clone();
        #[cfg(test)]
        let before_rename = self.fail_persist_at(PersistFailure::BeforeRename);
        #[cfg(not(test))]
        let before_rename: io::Result<()> = Ok(());
        crate::atomic_snapshot::replace(
            &mut temporary,
            &bytes,
            |source| {
                QueueError::at(
                    "write durable temporary file",
                    &temporary_path,
                    QueueErrorKind::Io(source),
                )
            },
            || {
                before_rename
                    .and_then(|()| fs::rename(&temporary_path, &path))
                    .map_err(|source| {
                        QueueError::at(
                            "publish durable snapshot",
                            &path,
                            QueueErrorKind::Io(source),
                        )
                    })
            },
            || {
                self.directory_sync_pending = true;
                self.sync_directory().map_err(|source| {
                    QueueError::at(
                        "sync visible durable commit",
                        &path,
                        QueueErrorKind::DurabilityUncertain(source),
                    )
                })
            },
        )
        .map_err(|error| match error {
            crate::atomic_snapshot::ReplaceError::BeforeCommit(error)
            | crate::atomic_snapshot::ReplaceError::VisibleDurabilityUncertain(error) => error,
        })
    }

    fn temporary_error(&self, source: io::Error) -> QueueError {
        QueueError::at(
            "write durable temporary file",
            &self.temporary_path,
            QueueErrorKind::Io(source),
        )
    }

    fn sync_directory(&mut self) -> io::Result<()> {
        if !self.directory_sync_pending {
            return Ok(());
        }
        #[cfg(test)]
        {
            self.directory_sync_attempts += 1;
            self.fail_persist_at(PersistFailure::AfterRename)?;
        }
        File::open(&self.directory)?.sync_all()?;
        self.directory_sync_pending = false;
        Ok(())
    }

    #[cfg(test)]
    pub(super) fn test_state(&self) -> (QueueSnapshot<P, K>, bool, usize) {
        (
            self.state.clone(),
            self.directory_sync_pending,
            self.directory_sync_attempts,
        )
    }
    #[cfg(test)]
    fn fail_persist_at(&self, stage: PersistFailure) -> io::Result<()> {
        if self.persist_failure == Some(stage) {
            Err(io::Error::other("injected persistence failure"))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
#[path = "store_tests.rs"]
mod tests;

#[cfg(unix)]
fn validate_private_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no arguments or memory-safety preconditions.
    let owner = unsafe { libc::geteuid() };
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.nlink() != 1
        || metadata.permissions().mode() & 0o7777 != 0o600
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "durable file is not an owner-only regular file",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn unsafe_startup_layout() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "unsupported durable startup layout",
    )
}

#[cfg(unix)]
fn open_startup_entry(
    directory: &File,
    name: &str,
    flags: i32,
    mode: libc::mode_t,
) -> io::Result<File> {
    let name = CString::new(name).map_err(|_| unsafe_startup_layout())?;
    open_startup_at(directory, &name, flags, mode)
}

#[cfg(unix)]
fn open_startup_at(
    directory: &File,
    name: &CStr,
    flags: i32,
    mode: libc::mode_t,
) -> io::Result<File> {
    // SAFETY: directory is live, name is NUL terminated, and a creation mode is supplied.
    let descriptor = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC,
            mode,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a new owned descriptor, transferred exactly once.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

#[cfg(unix)]
fn open_startup_directory(path: &Utf8Path) -> io::Result<File> {
    let absolute = if path.is_absolute() {
        path.as_std_path().to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut directory = File::open("/")?;
    // SAFETY: geteuid has no memory-safety preconditions.
    let owner = unsafe { libc::geteuid() };
    for component in absolute.components() {
        match component {
            Component::RootDir | Component::CurDir => continue,
            Component::Normal(name) => {
                let name = CString::new(name.as_bytes()).map_err(|_| unsafe_startup_layout())?;
                directory =
                    open_startup_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                let metadata = directory.metadata()?;
                let trusted_owner = metadata.uid() == 0 || metadata.uid() == owner;
                let replaceable =
                    metadata.mode() & 0o022 != 0 && metadata.mode() & libc::S_ISVTX == 0;
                if !trusted_owner || replaceable {
                    return Err(unsafe_startup_layout());
                }
            }
            _ => return Err(unsafe_startup_layout()),
        }
    }
    Ok(directory)
}

#[cfg(unix)]
fn private_directory(directory: &File) -> io::Result<bool> {
    let metadata = directory.metadata()?;
    // SAFETY: geteuid has no memory-safety preconditions.
    Ok(metadata.is_dir()
        && metadata.uid() == unsafe { libc::geteuid() }
        && metadata.mode() & 0o7777 == 0o700)
}

#[cfg(unix)]
fn validate_startup_file(file: &File, allow_legacy: bool) -> io::Result<()> {
    let metadata = file.metadata()?;
    // SAFETY: geteuid has no memory-safety preconditions.
    let owner = unsafe { libc::geteuid() };
    let mode = metadata.mode() & 0o7777;
    if !metadata.is_file()
        || metadata.uid() != owner
        || metadata.nlink() != 1
        || !(mode == 0o600 || (allow_legacy && mode == 0o644))
    {
        return Err(unsafe_startup_layout());
    }
    Ok(())
}

#[cfg(unix)]
fn preflight_startup_entries(
    path: &Utf8Path,
    directory: &File,
    entries: &[(&str, &File)],
) -> io::Result<()> {
    let current_directory = open_startup_directory(path)?;
    let before = directory.metadata()?;
    let after = current_directory.metadata()?;
    if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
        return Err(unsafe_startup_layout());
    }
    let allow_legacy = private_directory(directory)?;
    for &(name, file) in entries {
        let current = open_startup_entry(directory, name, libc::O_RDONLY, 0)?;
        let opened = file.metadata()?;
        let entry = current.metadata()?;
        if (opened.dev(), opened.ino()) != (entry.dev(), entry.ino()) {
            return Err(unsafe_startup_layout());
        }
        validate_startup_file(file, allow_legacy)?;
        validate_startup_file(&current, allow_legacy)?;
    }
    Ok(())
}

#[cfg(unix)]
fn read_startup_snapshot(
    path: &Utf8Path,
    directory: &File,
    lock: &File,
    lock_name: &str,
    snapshot_name: &str,
    temporary_name: &str,
) -> io::Result<Option<Vec<u8>>> {
    // Caller already holds the same lock inode exclusively. No snapshot bytes
    // or permissions are touched until every existing entry passes preflight.
    let mut snapshot = match open_startup_entry(directory, snapshot_name, libc::O_RDONLY, 0) {
        Ok(file) => Some(file),
        Err(source) if source.kind() == io::ErrorKind::NotFound => None,
        Err(source) => return Err(source),
    };
    // A pre-rename failure from the legacy writer can leave a permissive
    // temporary inode. It is never a source of committed snapshot bytes.
    let temporary = match open_startup_entry(directory, temporary_name, libc::O_RDONLY, 0) {
        Ok(file) => Some(file),
        Err(source) if source.kind() == io::ErrorKind::NotFound => None,
        Err(source) => return Err(source),
    };
    let entries: Vec<_> = std::iter::once((lock_name, lock))
        .chain(snapshot.as_ref().map(|file| (snapshot_name, file)))
        .chain(temporary.as_ref().map(|file| (temporary_name, file)))
        .collect();
    preflight_startup_entries(path, directory, &entries)?;
    let mut upgraded = false;
    for &(_, file) in &entries {
        if file.metadata()?.mode() & 0o7777 == 0o644 {
            preflight_startup_entries(path, directory, &entries)?;
            if !private_directory(directory)? {
                return Err(unsafe_startup_layout());
            }
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
            file.sync_all()?;
            validate_private_file(file)?;
            upgraded = true;
        }
    }
    preflight_startup_entries(path, directory, &entries)?;
    if upgraded {
        tracing::info!("restricted legacy durable file permissions inside private state directory");
    }
    match snapshot.as_mut() {
        Some(file) => {
            validate_private_file(file)?;
            let mut bytes = Vec::new();
            file.read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        }
        None => Ok(None),
    }
}
