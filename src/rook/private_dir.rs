use std::{io, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
/// Private-storage failure classification, including whether replacement became visible.
pub enum StorageErrorKind {
    /// Snapshot replacement failed before rename; the previous snapshot remains authoritative.
    BeforeCommit,
    /// Replacement is visible, but directory synchronization must succeed before granting authority.
    VisibleDurabilityUncertain,
    /// Another owner holds the recipient's process lock.
    Locked,
    /// Ownership, permissions, links, or object types violate private-storage requirements.
    UnsafeLayout,
    /// This platform cannot provide the required descriptor-relative storage guarantees.
    Unsupported,
    /// The snapshot exceeds the caller's bounded read limit.
    TooLarge,
    /// An underlying filesystem operation failed outside snapshot replacement.
    Io,
}

#[derive(Debug, thiserror::Error)]
#[error("Rook private storage failed: {kind:?}")]
/// Storage refusal with a typed classification and optional underlying I/O cause.
pub struct StorageError {
    /// Classification used to distinguish refusal from visible durability uncertainty.
    pub kind: StorageErrorKind,
    #[source]
    source: Option<io::Error>,
}

impl StorageError {
    fn new(kind: StorageErrorKind) -> Self {
        Self { kind, source: None }
    }
    fn io(kind: StorageErrorKind, source: io::Error) -> Self {
        Self {
            kind,
            source: Some(source),
        }
    }
}

#[cfg(all(unix, test))]
pub(crate) use unix::Failure;
#[cfg(unix)]
pub(crate) use unix::PrivateStore;

#[cfg(not(unix))]
pub(crate) struct PrivateStore;

#[cfg(not(unix))]
impl PrivateStore {
    pub(crate) fn open(_root: &Path, _recipient: &str) -> Result<Self, StorageError> {
        Err(StorageError::new(StorageErrorKind::Unsupported))
    }
    pub(crate) fn read(&self, _max_bytes: usize) -> Result<Option<Vec<u8>>, StorageError> {
        Err(StorageError::new(StorageErrorKind::Unsupported))
    }
    pub(crate) fn write(&mut self, _bytes: &[u8]) -> Result<(), StorageError> {
        Err(StorageError::new(StorageErrorKind::Unsupported))
    }
    pub(crate) fn sync_pending(&mut self) -> Result<(), StorageError> {
        Err(StorageError::new(StorageErrorKind::Unsupported))
    }
}

#[cfg(unix)]
mod unix {
    use super::{Path, StorageError, StorageErrorKind};
    use crate::atomic_snapshot::{self, ReplaceError};
    use fs2::FileExt;
    use std::{
        ffi::{CStr, CString},
        fs::{File, Metadata},
        io::{self, Read},
        os::{
            fd::{AsRawFd, FromRawFd},
            unix::{ffi::OsStrExt, fs::MetadataExt},
        },
        path::Component,
    };

    const SNAPSHOT: &CStr = c"snapshot.json";
    const TEMPORARY: &CStr = c"snapshot.tmp";
    const LOCK: &CStr = c"owner.lock";

    pub(crate) struct PrivateStore {
        directory: File,
        _lock: File,
        pending_sync: bool,
        #[cfg(test)]
        failure: Option<Failure>,
    }

    #[cfg(test)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) enum Failure {
        BeforeRename,
        AfterRename,
    }

    impl PrivateStore {
        pub(crate) fn open(root: &Path, recipient: &str) -> Result<Self, StorageError> {
            if recipient.is_empty()
                || recipient.len() > 128
                || !recipient
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            {
                return Err(StorageError::new(StorageErrorKind::UnsafeLayout));
            }
            let root = open_root(root)?;
            let name = CString::new(recipient)
                .map_err(|_| StorageError::new(StorageErrorKind::UnsafeLayout))?;
            // SAFETY: root is a live directory descriptor and name is NUL terminated.
            let created = unsafe { libc::mkdirat(root.as_raw_fd(), name.as_ptr(), 0o700) };
            if created != 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::AlreadyExists {
                    return Err(storage_io(error));
                }
            }
            let directory = open_at(&root, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
            let metadata = directory.metadata().map_err(storage_io)?;
            if !metadata.is_dir()
                || metadata.uid() != effective_uid()
                || metadata.mode() & 0o7777 != 0o700
            {
                return Err(StorageError::new(StorageErrorKind::UnsafeLayout));
            }
            let lock = match open_at(
                &directory,
                LOCK,
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ) {
                Ok(file) => file,
                Err(error)
                    if error
                        .source
                        .as_ref()
                        .is_some_and(|source| source.kind() == io::ErrorKind::AlreadyExists) =>
                {
                    open_at(&directory, LOCK, libc::O_RDWR, 0)?
                }
                Err(error) => return Err(error),
            };
            validate_file(&lock.metadata().map_err(storage_io)?)?;
            lock.try_lock_exclusive().map_err(|error| {
                StorageError::io(
                    if error.kind() == io::ErrorKind::WouldBlock {
                        StorageErrorKind::Locked
                    } else {
                        StorageErrorKind::Io
                    },
                    error,
                )
            })?;
            if let Some(stale) = existing_file(&directory, TEMPORARY)? {
                drop(stale);
                unlink(&directory, TEMPORARY).map_err(storage_io)?;
            }
            existing_file(&directory, SNAPSHOT)?;
            root.sync_all().map_err(storage_io)?;
            let mut store = Self {
                directory,
                _lock: lock,
                pending_sync: true,
                #[cfg(test)]
                failure: None,
            };
            store.sync_pending()?;
            Ok(store)
        }

        pub(crate) fn read(&self, max_bytes: usize) -> Result<Option<Vec<u8>>, StorageError> {
            let Some(file) = existing_file(&self.directory, SNAPSHOT)? else {
                return Ok(None);
            };
            let limit = u64::try_from(max_bytes)
                .ok()
                .and_then(|limit| limit.checked_add(1))
                .ok_or_else(|| StorageError::new(StorageErrorKind::TooLarge))?;
            let mut bytes = Vec::new();
            file.take(limit)
                .read_to_end(&mut bytes)
                .map_err(storage_io)?;
            if bytes.len() > max_bytes {
                return Err(StorageError::new(StorageErrorKind::TooLarge));
            }
            Ok(Some(bytes))
        }

        pub(crate) fn write(&mut self, bytes: &[u8]) -> Result<(), StorageError> {
            self.sync_pending()?;
            existing_file(&self.directory, SNAPSHOT)?;
            let mut temporary = open_at(
                &self.directory,
                TEMPORARY,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )?;
            validate_file(&temporary.metadata().map_err(storage_io)?)?;
            let directory = &self.directory;
            let result = atomic_snapshot::replace(
                &mut temporary,
                bytes,
                |error| error,
                || {
                    #[cfg(test)]
                    if self.failure == Some(Failure::BeforeRename) {
                        return Err(io::Error::other("injected before rename"));
                    }
                    // SAFETY: both names are NUL terminated and directory remains open for this call.
                    if unsafe {
                        libc::renameat(
                            directory.as_raw_fd(),
                            TEMPORARY.as_ptr(),
                            directory.as_raw_fd(),
                            SNAPSHOT.as_ptr(),
                        )
                    } != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                },
                || {
                    #[cfg(test)]
                    if self.failure == Some(Failure::AfterRename) {
                        return Err(io::Error::other("injected after rename"));
                    }
                    directory.sync_all()
                },
            );
            match result {
                Ok(()) => Ok(()),
                Err(ReplaceError::BeforeCommit(error)) => {
                    unlink(directory, TEMPORARY).map_err(storage_io)?;
                    Err(StorageError::io(StorageErrorKind::BeforeCommit, error))
                }
                Err(ReplaceError::VisibleDurabilityUncertain(error)) => {
                    self.pending_sync = true;
                    Err(StorageError::io(
                        StorageErrorKind::VisibleDurabilityUncertain,
                        error,
                    ))
                }
            }
        }

        pub(crate) fn sync_pending(&mut self) -> Result<(), StorageError> {
            if self.pending_sync {
                #[cfg(test)]
                if self.failure == Some(Failure::AfterRename) {
                    return Err(StorageError::io(
                        StorageErrorKind::VisibleDurabilityUncertain,
                        io::Error::other("injected directory sync"),
                    ));
                }
                self.directory.sync_all().map_err(|error| {
                    StorageError::io(StorageErrorKind::VisibleDurabilityUncertain, error)
                })?;
                self.pending_sync = false;
            }
            Ok(())
        }

        #[cfg(test)]
        pub(crate) fn fail_at(&mut self, failure: Option<Failure>) {
            self.failure = failure;
        }
    }

    fn effective_uid() -> u32 {
        // SAFETY: geteuid has no arguments or memory preconditions.
        unsafe { libc::geteuid() }
    }

    fn storage_io(error: io::Error) -> StorageError {
        StorageError::io(StorageErrorKind::Io, error)
    }

    fn open_at(
        parent: &File,
        name: &CStr,
        flags: i32,
        mode: libc::mode_t,
    ) -> Result<File, StorageError> {
        // SAFETY: parent is live, name is NUL terminated, and mode is supplied for O_CREAT.
        let descriptor = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                name.as_ptr(),
                flags | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                mode,
            )
        };
        if descriptor < 0 {
            let error = io::Error::last_os_error();
            let kind = if matches!(error.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)) {
                StorageErrorKind::UnsafeLayout
            } else {
                StorageErrorKind::Io
            };
            return Err(StorageError::io(kind, error));
        }
        // SAFETY: openat returned a new owned descriptor, transferred exactly once.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }

    fn open_root(root: &Path) -> Result<File, StorageError> {
        let absolute = if root.is_absolute() {
            root.to_owned()
        } else {
            std::env::current_dir().map_err(storage_io)?.join(root)
        };
        let mut directory = File::open("/").map_err(storage_io)?;
        for component in absolute.components() {
            match component {
                Component::RootDir => continue,
                Component::Normal(name) => {
                    let name = CString::new(name.as_bytes())
                        .map_err(|_| StorageError::new(StorageErrorKind::UnsafeLayout))?;
                    directory = open_at(&directory, &name, libc::O_RDONLY | libc::O_DIRECTORY, 0)?;
                    let metadata = directory.metadata().map_err(storage_io)?;
                    let trusted_owner = metadata.uid() == 0 || metadata.uid() == effective_uid();
                    let replaceable =
                        metadata.mode() & 0o022 != 0 && metadata.mode() & libc::S_ISVTX == 0;
                    if !trusted_owner || replaceable {
                        return Err(StorageError::new(StorageErrorKind::UnsafeLayout));
                    }
                }
                _ => return Err(StorageError::new(StorageErrorKind::UnsafeLayout)),
            }
        }
        Ok(directory)
    }

    fn validate_file(metadata: &Metadata) -> Result<(), StorageError> {
        if !metadata.is_file()
            || metadata.uid() != effective_uid()
            || metadata.mode() & 0o7777 != 0o600
            || metadata.nlink() != 1
        {
            return Err(StorageError::new(StorageErrorKind::UnsafeLayout));
        }
        Ok(())
    }

    fn existing_file(directory: &File, name: &CStr) -> Result<Option<File>, StorageError> {
        match open_at(directory, name, libc::O_RDONLY, 0) {
            Ok(file) => {
                validate_file(&file.metadata().map_err(storage_io)?)?;
                Ok(Some(file))
            }
            Err(error)
                if error
                    .source
                    .as_ref()
                    .is_some_and(|source| source.kind() == io::ErrorKind::NotFound) =>
            {
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }

    fn unlink(directory: &File, name: &CStr) -> io::Result<()> {
        // SAFETY: directory remains live and name is NUL terminated.
        if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{Failure, PrivateStore, StorageErrorKind};
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    use tempfile::TempDir;

    fn error_kind<T>(result: Result<T, super::StorageError>) -> StorageErrorKind {
        match result {
            Ok(_) => panic!("expected storage refusal"),
            Err(error) => error.kind,
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn partial_write_failure_preserves_snapshot_and_recovers_after_reopen() {
        const CHILD: &str = "DIONE_ROOK_FSIZE_TEST_CHILD";
        if std::env::var_os(CHILD).is_none() {
            // Reaping also runs during assertion unwinding, so this test never
            // leaves its resource-limited child behind.
            struct ReapChild(std::process::Child);
            impl Drop for ReapChild {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let proof = TempDir::new().unwrap();
            let receipt = proof.path().join("completed");
            let mut child = ReapChild(std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "rook::private_dir::tests::partial_write_failure_preserves_snapshot_and_recovers_after_reopen",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD, "1")
                .env("DIONE_ROOK_FSIZE_TEST_RECEIPT", &receipt)
                // No captured pipes can fill while the parent waits. The receipt
                // proves the exact test ran rather than accepting zero selection.
                .stdout(std::process::Stdio::inherit())
                .stderr(std::process::Stdio::inherit())
                .spawn()
                .unwrap());
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            let status = loop {
                match child.0.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) if std::time::Instant::now() < deadline => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Ok(None) => {
                        drop(child);
                        panic!("isolated write-failure test exceeded 30 seconds");
                    }
                    Err(error) => {
                        drop(child);
                        panic!("could not wait for isolated write-failure test: {error}");
                    }
                }
            };
            drop(child);
            assert!(
                status.success(),
                "isolated write-failure test failed: {status}"
            );
            assert_eq!(fs::read(&receipt).unwrap(), b"passed");
            return;
        }

        let root = TempDir::new().unwrap();
        let mut store = PrivateStore::open(root.path(), "recipient").unwrap();
        store.write(b"old").unwrap();
        let probe_path = root.path().join("partial-probe");
        let mut probe = fs::File::create(&probe_path).unwrap();
        let old_path = root.path().join("probe-snapshot");
        fs::write(&old_path, b"old").unwrap();
        let mut original = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        // SAFETY: the pointer is writable for one initialized rlimit. This
        // dedicated subprocess isolates process-wide limits from other tests.
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut original) },
            0
        );
        assert!(original.rlim_max >= 256);
        // SAFETY: SIG_IGN is a supported signal disposition. Only this child
        // changes SIGXFSZ, letting write report EFBIG instead of killing it.
        let original_signal = unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_IGN) };
        assert_ne!(original_signal, libc::SIG_ERR);
        let limited = libc::rlimit {
            rlim_cur: 256,
            rlim_max: original.rlim_max,
        };
        // SAFETY: limited is a valid rlimit; the hard limit is unchanged.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limited) }, 0);

        let renamed = std::cell::Cell::new(false);
        let synced = std::cell::Cell::new(false);
        let bytes = [b'n'; 512];
        let result = crate::atomic_snapshot::replace(
            &mut probe,
            &bytes,
            |error| error,
            || {
                renamed.set(true);
                fs::rename(&probe_path, &old_path)
            },
            || {
                synced.set(true);
                fs::File::open(root.path())?.sync_all()
            },
        );
        let Err(crate::atomic_snapshot::ReplaceError::BeforeCommit(error)) = result else {
            panic!("expected partial regular-file write failure");
        };
        assert_eq!(error.raw_os_error(), Some(libc::EFBIG));
        assert_eq!(fs::read(&probe_path).unwrap(), bytes[..256]);
        assert!(!renamed.get());
        assert!(!synced.get());
        assert_eq!(fs::read(&old_path).unwrap(), b"old");

        let error = store.write(&bytes).unwrap_err();
        assert_eq!(error.kind, StorageErrorKind::BeforeCommit);
        assert_eq!(error.source.unwrap().raw_os_error(), Some(libc::EFBIG));
        assert!(!root.path().join("recipient/snapshot.tmp").exists());
        assert_eq!(store.read(512).unwrap(), Some(b"old".to_vec()));
        drop(store);
        let mut store = PrivateStore::open(root.path(), "recipient").unwrap();
        assert_eq!(store.read(512).unwrap(), Some(b"old".to_vec()));

        // SAFETY: restore the exact limit and signal disposition captured in
        // this child. A panic above exits the child without affecting its parent.
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &original) }, 0);
        // SAFETY: original_signal was returned by the successful signal call.
        assert_ne!(
            unsafe { libc::signal(libc::SIGXFSZ, original_signal) },
            libc::SIG_ERR
        );
        store.write(&bytes).unwrap();
        drop(store);
        let store = PrivateStore::open(root.path(), "recipient").unwrap();
        assert_eq!(store.read(512).unwrap(), Some(bytes.to_vec()));
        fs::write(
            std::env::var_os("DIONE_ROOK_FSIZE_TEST_RECEIPT").unwrap(),
            b"passed",
        )
        .unwrap();
    }

    #[test]
    fn snapshot_roundtrip_restart_and_bounded_read() {
        let root = TempDir::new().unwrap();
        let mut store = PrivateStore::open(root.path(), "recipient").unwrap();
        assert_eq!(store.read(4).unwrap(), None);
        store.write(b"data").unwrap();
        assert_eq!(store.read(4).unwrap(), Some(b"data".to_vec()));
        assert_eq!(error_kind(store.read(3)), StorageErrorKind::TooLarge);
        drop(store);
        let store = PrivateStore::open(root.path(), "recipient").unwrap();
        assert_eq!(store.read(4).unwrap(), Some(b"data".to_vec()));
    }

    #[test]
    fn recipient_lock_lasts_until_owner_drop() {
        let root = TempDir::new().unwrap();
        let store = PrivateStore::open(root.path(), "recipient").unwrap();
        assert_eq!(
            error_kind(PrivateStore::open(root.path(), "recipient")),
            StorageErrorKind::Locked
        );
        drop(store);
        PrivateStore::open(root.path(), "recipient").unwrap();
    }

    #[test]
    fn rejects_wrong_modes_symlinks_and_hardlinks() {
        let root = TempDir::new().unwrap();
        let mut store = PrivateStore::open(root.path(), "recipient").unwrap();
        store.write(b"original").unwrap();
        drop(store);
        let snapshot = root.path().join("recipient/snapshot.json");
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            error_kind(PrivateStore::open(root.path(), "recipient")),
            StorageErrorKind::UnsafeLayout
        );
        fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o600)).unwrap();
        fs::hard_link(&snapshot, root.path().join("alias")).unwrap();
        assert_eq!(
            error_kind(PrivateStore::open(root.path(), "recipient")),
            StorageErrorKind::UnsafeLayout
        );
        fs::remove_file(root.path().join("alias")).unwrap();
        fs::rename(&snapshot, root.path().join("original")).unwrap();
        symlink(root.path().join("original"), &snapshot).unwrap();
        assert_eq!(
            error_kind(PrivateStore::open(root.path(), "recipient")),
            StorageErrorKind::UnsafeLayout
        );
        fs::remove_file(&snapshot).unwrap();
        fs::set_permissions(
            root.path().join("recipient"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        assert_eq!(
            error_kind(PrivateStore::open(root.path(), "recipient")),
            StorageErrorKind::UnsafeLayout
        );
    }

    #[test]
    fn rejects_directory_symlinks_and_replaceable_ancestors() {
        let root = TempDir::new().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        symlink(&real, root.path().join("alias")).unwrap();
        assert_eq!(
            error_kind(PrivateStore::open(&root.path().join("alias"), "recipient")),
            StorageErrorKind::UnsafeLayout
        );
        fs::set_permissions(&real, fs::Permissions::from_mode(0o777)).unwrap();
        assert_eq!(
            error_kind(PrivateStore::open(&real, "recipient")),
            StorageErrorKind::UnsafeLayout
        );
    }

    #[test]
    fn pinned_directory_does_not_follow_replaced_path() {
        let root = TempDir::new().unwrap();
        let mut store = PrivateStore::open(root.path(), "recipient").unwrap();
        fs::rename(root.path().join("recipient"), root.path().join("moved")).unwrap();
        fs::create_dir(root.path().join("recipient")).unwrap();
        store.write(b"pinned").unwrap();
        assert_eq!(
            fs::read(root.path().join("moved/snapshot.json")).unwrap(),
            b"pinned"
        );
        assert!(!root.path().join("recipient/snapshot.json").exists());
    }

    #[test]
    fn failure_before_rename_retains_previous_snapshot() {
        let root = TempDir::new().unwrap();
        let mut store = PrivateStore::open(root.path(), "recipient").unwrap();
        store.write(b"old").unwrap();
        store.fail_at(Some(Failure::BeforeRename));
        assert_eq!(
            error_kind(store.write(b"new")),
            StorageErrorKind::BeforeCommit
        );
        assert_eq!(store.read(3).unwrap(), Some(b"old".to_vec()));
        drop(store);
        let store = PrivateStore::open(root.path(), "recipient").unwrap();
        assert_eq!(store.read(3).unwrap(), Some(b"old".to_vec()));
    }

    #[test]
    fn failure_after_rename_is_visible_and_blocks_later_writes() {
        let root = TempDir::new().unwrap();
        let mut store = PrivateStore::open(root.path(), "recipient").unwrap();
        store.write(b"old").unwrap();
        store.fail_at(Some(Failure::AfterRename));
        assert_eq!(
            error_kind(store.write(b"new")),
            StorageErrorKind::VisibleDurabilityUncertain
        );
        assert_eq!(store.read(3).unwrap(), Some(b"new".to_vec()));
        assert_eq!(
            error_kind(store.write(b"bad")),
            StorageErrorKind::VisibleDurabilityUncertain
        );
        drop(store);
        let mut store = PrivateStore::open(root.path(), "recipient").unwrap();
        assert_eq!(store.read(3).unwrap(), Some(b"new".to_vec()));
        store.write(b"next").unwrap();
    }

    #[test]
    fn stale_private_temporary_is_removed_after_lock() {
        let root = TempDir::new().unwrap();
        let store = PrivateStore::open(root.path(), "recipient").unwrap();
        drop(store);
        let temporary = root.path().join("recipient/snapshot.tmp");
        fs::write(&temporary, b"partial").unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).unwrap();
        let mut store = PrivateStore::open(root.path(), "recipient").unwrap();
        assert!(!temporary.exists());
        assert_eq!(store.read(20).unwrap(), None);
        store.write(b"complete").unwrap();
    }

    #[test]
    fn stale_temporary_symlink_is_refused_without_touching_target() {
        let root = TempDir::new().unwrap();
        drop(PrivateStore::open(root.path(), "recipient").unwrap());
        let target = root.path().join("outside");
        fs::write(&target, b"untouched").unwrap();
        symlink(&target, root.path().join("recipient/snapshot.tmp")).unwrap();
        assert_eq!(
            error_kind(PrivateStore::open(root.path(), "recipient")),
            StorageErrorKind::UnsafeLayout
        );
        assert_eq!(fs::read(target).unwrap(), b"untouched");
    }
}
