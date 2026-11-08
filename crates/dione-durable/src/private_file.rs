//! Application-owned append files and atomic replacements.
//!
//! On Unix, parent traversal rejects symlinks and untrusted writable ancestors;
//! new directories use 0700 and regular single-link files use 0600/current UID.
//! Legacy 0644 is tightened through its open descriptor only beneath a verified
//! current-owner 0700 parent. Other unsafe existing objects are never repaired.
//! Non-Unix retains ordinary filesystem behavior; no Windows ACL guarantee is made.
//! Callers retain serialization, append flush policy and failure semantics.

use std::{
    fs::{self, File},
    io::{self, Read},
    path::Path,
};

/// Open an admitted file for append, privately creating missing parents/files on Unix.
pub fn open_append(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        unix::open(path, true, true)
    }
    #[cfg(not(unix))]
    {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::OpenOptions::new().create(true).append(true).open(path)
    }
}

/// Append with the same file admission while requiring its parent directory to exist.
/// This preserves owners whose missing-directory write failure drives retry policy.
pub fn open_append_existing_parent(path: &Path) -> io::Result<File> {
    #[cfg(unix)]
    {
        unix::open(path, true, false)
    }
    #[cfg(not(unix))]
    {
        fs::OpenOptions::new().create(true).append(true).open(path)
    }
}

/// Read an admitted file, without creating missing files or parents.
/// A missing file remains `NotFound` for the caller's existing empty-log policy.
pub fn read_to_string(path: &Path) -> io::Result<String> {
    #[cfg(unix)]
    let mut file = unix::open(path, false, false)?;
    #[cfg(not(unix))]
    let mut file = File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    Ok(contents)
}

/// File-sync and atomically replace an admitted file with a privately created sibling.
/// No directory-sync or crash-atomic append guarantee is added. Pre-rename failures
/// remove only the temporary created by this call. A verified legacy `jsonl.tmp`
/// sibling is removed after commit; unsafe aliases fail before replacement. Cleanup
/// errors after visibility explicitly report that the replacement committed. Unix
/// race protection assumes the trusted parent and the caller's serialized ownership.
pub fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    #[cfg(unix)]
    {
        unix::replace(path, bytes)
    }
    #[cfg(not(unix))]
    {
        use std::io::Write;
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let temporary = path.with_extension("jsonl.tmp");
        let mut file = File::create(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(temporary, path)
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use crate::{atomic_snapshot, store};
    use std::{
        ffi::CString,
        os::{
            fd::AsRawFd,
            unix::{
                ffi::OsStrExt,
                fs::{MetadataExt, PermissionsExt},
            },
        },
        sync::atomic::{AtomicU64, Ordering},
    };
    static NEXT_TEMPORARY: AtomicU64 = AtomicU64::new(0);

    fn parent(path: &Path, create: bool) -> io::Result<(File, CString)> {
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "private file requires a filename",
            )
        })?;
        let name = CString::new(name.as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid private filename"))?;
        let directory = store::open_private_directory(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new(".")),
            create,
        )?;
        Ok((directory, name))
    }

    fn admit(file: File, directory: &File) -> io::Result<File> {
        store::validate_startup_file(&file, store::private_directory(directory)?)?;
        if file.metadata()?.mode() & 0o7777 == 0o644 {
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        store::validate_private_file(&file)?;
        Ok(file)
    }

    pub(super) fn open(path: &Path, append: bool, create_parents: bool) -> io::Result<File> {
        let (directory, name) = parent(path, create_parents)?;
        let flags = if append {
            libc::O_WRONLY | libc::O_APPEND
        } else {
            libc::O_RDONLY
        };
        if append {
            match store::open_startup_at(
                &directory,
                &name,
                flags | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ) {
                Ok(file) => return admit(file, &directory),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error),
            }
        }
        admit(
            store::open_startup_at(&directory, &name, flags, 0)?,
            &directory,
        )
    }

    struct Temporary<'a> {
        directory: &'a File,
        name: CString,
        file: Option<File>,
        published: bool,
    }
    impl Drop for Temporary<'_> {
        fn drop(&mut self) {
            drop(self.file.take());
            if !self.published {
                // SAFETY: this call exclusively created this entry in the admitted directory.
                unsafe {
                    libc::unlinkat(self.directory.as_raw_fd(), self.name.as_ptr(), 0);
                }
            }
        }
    }

    #[cfg(test)]
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(super) enum TestFailure {
        BeforeRename,
        LegacyCleanup,
    }

    pub(super) fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
        replace_inner(
            path,
            bytes,
            #[cfg(test)]
            None,
        )
    }

    pub(super) fn replace_inner(
        path: &Path,
        bytes: &[u8],
        #[cfg(test)] failure: Option<TestFailure>,
    ) -> io::Result<()> {
        let (directory, destination) = parent(path, true)?;
        let existing = match store::open_startup_at(&directory, &destination, libc::O_RDONLY, 0) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        let legacy_name = path.with_extension("jsonl.tmp");
        let legacy_name =
            CString::new(legacy_name.file_name().unwrap().as_bytes()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid legacy staging filename",
                )
            })?;
        let legacy = match store::open_startup_at(&directory, &legacy_name, libc::O_RDONLY, 0) {
            Ok(file) => Some(file),
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => return Err(error),
        };
        // Validate the complete existing layout before tightening any legacy inode.
        let allow_legacy = store::private_directory(&directory)?;
        for file in existing.iter().chain(legacy.iter()) {
            store::validate_startup_file(file, allow_legacy)?;
        }
        for file in existing.iter().chain(legacy.iter()) {
            if file.metadata()?.mode() & 0o7777 == 0o644 {
                file.set_permissions(fs::Permissions::from_mode(0o600))?;
            }
            store::validate_private_file(file)?;
        }
        let mut temporary = None;
        for _ in 0..128 {
            let sequence = NEXT_TEMPORARY.fetch_add(1, Ordering::Relaxed);
            let name = CString::new(format!(
                ".dione-private-{}-{sequence}.tmp",
                std::process::id()
            ))
            .unwrap();
            match store::open_startup_at(
                &directory,
                &name,
                libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL,
                0o600,
            ) {
                Ok(file) => {
                    // Establish ownership before admission so an ordinary validation error cleans up.
                    temporary = Some(Temporary {
                        directory: &directory,
                        name,
                        file: Some(file),
                        published: false,
                    });
                    break;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }
        let mut temporary = temporary.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "private staging names exhausted",
            )
        })?;
        store::validate_private_file(temporary.file.as_ref().unwrap())?;
        let result = atomic_snapshot::replace(
            temporary.file.as_mut().unwrap(),
            bytes,
            |error| error,
            || {
                #[cfg(test)]
                if failure == Some(TestFailure::BeforeRename) {
                    return Err(io::Error::other("synthetic pre-rename failure"));
                }
                // SAFETY: both NUL-terminated entry names are relative to the admitted live descriptor.
                let result = unsafe {
                    libc::renameat(
                        directory.as_raw_fd(),
                        temporary.name.as_ptr(),
                        directory.as_raw_fd(),
                        destination.as_ptr(),
                    )
                };
                if result < 0 {
                    Err(io::Error::last_os_error())
                } else {
                    Ok(())
                }
            },
            || Ok(()),
        );
        match result {
            Ok(()) => {
                temporary.published = true;
                if let Some(witness) = legacy {
                    let cleanup = (|| {
                        #[cfg(test)]
                        if failure == Some(TestFailure::LegacyCleanup) {
                            return Err(io::Error::other("synthetic legacy cleanup failure"));
                        }
                        let current =
                            store::open_startup_at(&directory, &legacy_name, libc::O_RDONLY, 0)?;
                        store::validate_private_file(&current)?;
                        let before = witness.metadata()?;
                        let after = current.metadata()?;
                        if (before.dev(), before.ino()) != (after.dev(), after.ino()) {
                            return Err(io::Error::new(
                                io::ErrorKind::PermissionDenied,
                                "legacy staging identity changed",
                            ));
                        }
                        // SAFETY: the admitted exact sibling still matches its retained descriptor witness.
                        if unsafe { libc::unlinkat(directory.as_raw_fd(), legacy_name.as_ptr(), 0) }
                            < 0
                        {
                            return Err(io::Error::last_os_error());
                        }
                        Ok(())
                    })();
                    cleanup.map_err(|error: io::Error| {
                        io::Error::new(
                            error.kind(),
                            "private replacement committed; legacy staging cleanup failed",
                        )
                    })?;
                }
                Ok(())
            }
            Err(
                atomic_snapshot::ReplaceError::BeforeCommit(error)
                | atomic_snapshot::ReplaceError::VisibleDurabilityUncertain(error),
            ) => Err(error),
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::{
        io::Write,
        os::unix::fs::{PermissionsExt, symlink},
    };

    #[test]
    fn private_file_admission_rejects_aliases_and_exposed_legacy_without_mutation() {
        for kind in [
            "exposed",
            "symlink",
            "hardlink",
            "directory",
            "fifo",
            "legacy-symlink",
        ] {
            let root = tempfile::tempdir().unwrap();
            let destination = root.path().join("audit.jsonl");
            let victim = root.path().join("victim");
            fs::write(&victim, b"SYNTHETIC_PRIVATE_ORIGINAL").unwrap();
            fs::set_permissions(&victim, fs::Permissions::from_mode(0o600)).unwrap();
            if kind == "legacy-symlink" {
                fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
            }
            if kind == "exposed" {
                fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
            }
            match kind {
                "symlink" => symlink(&victim, &destination).unwrap(),
                "hardlink" => fs::hard_link(&victim, &destination).unwrap(),
                "directory" => fs::create_dir(&destination).unwrap(),
                "fifo" => {
                    use std::{ffi::CString, os::unix::ffi::OsStrExt};
                    let name = CString::new(destination.as_os_str().as_bytes()).unwrap();
                    // SAFETY: NUL terminated owned temporary fixture pathname, no unrelated objects.
                    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
                }
                _ => {
                    fs::write(&destination, b"SYNTHETIC_ORIGINAL").unwrap();
                    fs::set_permissions(&destination, fs::Permissions::from_mode(0o644)).unwrap();
                }
            }
            if kind == "legacy-symlink" {
                symlink(&victim, destination.with_extension("jsonl.tmp")).unwrap();
            }
            let before_mode = fs::symlink_metadata(&destination)
                .unwrap()
                .permissions()
                .mode();
            if kind != "legacy-symlink" {
                assert!(
                    open_append(&destination).is_err(),
                    "append must reject {kind}"
                );
                assert!(
                    read_to_string(&destination).is_err(),
                    "read must reject {kind}"
                );
            }
            assert!(
                replace(&destination, b"replacement").is_err(),
                "replacement must reject {kind}"
            );
            assert_eq!(fs::read(&victim).unwrap(), b"SYNTHETIC_PRIVATE_ORIGINAL");
            assert_eq!(
                fs::metadata(&victim).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::symlink_metadata(&destination)
                    .unwrap()
                    .permissions()
                    .mode(),
                before_mode,
                "unsafe layout must not trigger collateral permission changes"
            );
        }
    }

    #[test]
    fn replacement_failure_keeps_original_and_postcommit_error_keeps_published_bytes() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let destination = root.path().join("audit.jsonl");
        let legacy = destination.with_extension("jsonl.tmp");
        open_append(&destination)
            .unwrap()
            .write_all(b"original")
            .unwrap();
        open_append(&legacy).unwrap().write_all(b"legacy").unwrap();
        assert!(
            unix::replace_inner(&destination, b"new", Some(unix::TestFailure::BeforeRename))
                .is_err()
        );
        assert_eq!(read_to_string(&destination).unwrap(), "original");
        assert_eq!(read_to_string(&legacy).unwrap(), "legacy");
        let error =
            unix::replace_inner(&destination, b"new", Some(unix::TestFailure::LegacyCleanup))
                .unwrap_err();
        assert!(error.to_string().contains("replacement committed"));
        assert_eq!(read_to_string(&destination).unwrap(), "new");
        assert_eq!(read_to_string(&legacy).unwrap(), "legacy");
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".dione-private-"))
                .count(),
            0
        );
        replace(&destination, b"retry").unwrap();
        assert_eq!(read_to_string(&destination).unwrap(), "retry");
        assert!(!legacy.exists());
    }

    #[test]
    fn unsafe_ancestor_never_creates_descendants_or_follows_symlinks() {
        let root = tempfile::tempdir().unwrap();
        let real = root.path().join("real");
        fs::create_dir(&real).unwrap();
        symlink(&real, root.path().join("alias")).unwrap();
        assert!(open_append(&root.path().join("alias/held.jsonl")).is_err());
        assert!(!real.join("held.jsonl").exists());
        let unsafe_directory = root.path().join("writable");
        fs::create_dir(&unsafe_directory).unwrap();
        fs::set_permissions(&unsafe_directory, fs::Permissions::from_mode(0o777)).unwrap();
        assert!(open_append(&unsafe_directory.join("never/held.jsonl")).is_err());
        assert!(!unsafe_directory.join("never").exists());
        let missing = root.path().join("missing/held.jsonl");
        assert!(open_append_existing_parent(&missing).is_err());
        assert!(!root.path().join("missing").exists());
    }

    #[test]
    fn private_parent_legacy_migration_and_unique_staging_preserve_bytes() {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let destination = root.path().join("audit.jsonl");
        let legacy = destination.with_extension("jsonl.tmp");
        for (path, bytes) in [
            (&destination, b"old".as_slice()),
            (&legacy, b"stale".as_slice()),
        ] {
            fs::write(path, bytes).unwrap();
            fs::set_permissions(path, fs::Permissions::from_mode(0o644)).unwrap();
        }
        assert_eq!(read_to_string(&destination).unwrap(), "old");
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
        replace(&destination, b"new").unwrap();
        assert_eq!(read_to_string(&destination).unwrap(), "new");
        assert!(
            !legacy.exists(),
            "successful maintenance must consume admitted legacy staging"
        );
        assert_eq!(
            fs::metadata(&destination).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let nested = root.path().join("new/owned/diary.jsonl");
        open_append(&nested).unwrap().write_all(b"held").unwrap();
        for dir in [root.path().join("new"), root.path().join("new/owned")] {
            assert_eq!(
                fs::metadata(dir).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        assert_eq!(
            fs::metadata(nested).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            fs::read_dir(root.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|e| e
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".dione-private-"))
                .count(),
            0
        );
    }
}
