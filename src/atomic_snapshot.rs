use std::{
    fs::File,
    io::{self, Write},
};

pub(crate) enum ReplaceError<E> {
    BeforeCommit(E),
    VisibleDurabilityUncertain(E),
}

pub(crate) fn replace<E>(
    temporary: &mut File,
    bytes: &[u8],
    write_error: impl FnOnce(io::Error) -> E,
    rename: impl FnOnce() -> Result<(), E>,
    sync_directory: impl FnOnce() -> Result<(), E>,
) -> Result<(), ReplaceError<E>> {
    temporary
        .write_all(bytes)
        .and_then(|()| temporary.sync_all())
        .map_err(|error| ReplaceError::BeforeCommit(write_error(error)))?;
    rename().map_err(ReplaceError::BeforeCommit)?;
    sync_directory().map_err(ReplaceError::VisibleDurabilityUncertain)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{ReplaceError, replace};
    use std::{cell::Cell, fs, fs::OpenOptions};

    #[test]
    fn actual_file_sync_error_does_not_invoke_commit_closures() {
        let root = tempfile::TempDir::new().unwrap();
        let snapshot = root.path().join("snapshot");
        let replacement = root.path().join("replacement");
        fs::write(&snapshot, b"old").unwrap();
        fs::write(&replacement, b"new").unwrap();
        let renamed = Cell::new(false);
        let directory_synced = Cell::new(false);
        // This device accepts writes but rejects fsync. It tests the shared
        // helper's syscall boundary, not PrivateStore's regular-file admission.
        let mut device = OpenOptions::new().write(true).open("/dev/null").unwrap();
        let result = replace(
            &mut device,
            b"new",
            |error| error,
            || {
                renamed.set(true);
                fs::rename(&replacement, &snapshot)
            },
            || {
                directory_synced.set(true);
                fs::File::open(root.path())?.sync_all()
            },
        );
        let Err(ReplaceError::BeforeCommit(error)) = result else {
            panic!("expected file synchronization failure before commit");
        };
        assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
        assert!(!renamed.get());
        assert!(!directory_synced.get());
        assert_eq!(fs::read(&snapshot).unwrap(), b"old");
        assert_eq!(fs::read(&replacement).unwrap(), b"new");
    }
}
