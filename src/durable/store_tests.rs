use super::{DurableStore, PersistFailure};
use crate::durable::{
    CodecError, CodecErrorKind, QueueCodec, QueueError, QueueErrorKind, QueueOptions,
    QueueSnapshot, ReplayClass,
};
use camino::Utf8Path;
use chrono::Duration;
use std::{
    fs, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};
use tempfile::TempDir;

#[derive(Default)]
struct CounterCodec {
    reject_encoding: AtomicBool,
}

impl QueueCodec for CounterCodec {
    type Payload = ();
    type Key = u64;

    fn decode(&self, bytes: &[u8]) -> Result<QueueSnapshot<(), u64>, CodecError> {
        let counter = <[u8; 8]>::try_from(bytes).map_err(|source| CodecError {
            kind: CodecErrorKind::Decode(Box::new(source)),
        })?;
        Ok(QueueSnapshot {
            next_id: u64::from_le_bytes(counter),
            ..QueueSnapshot::default()
        })
    }

    fn encode(&self, state: &QueueSnapshot<(), u64>) -> Result<Vec<u8>, CodecError> {
        if self.reject_encoding.load(Ordering::SeqCst) {
            return Err(CodecError {
                kind: CodecErrorKind::Encode(Box::new(io::Error::other(
                    "counter encoding rejected",
                ))),
            });
        }
        Ok(state.next_id.to_le_bytes().to_vec())
    }
}

fn options() -> QueueOptions<u64> {
    QueueOptions {
        file_name: "counter.bin",
        lock_name: "counter.lock",
        consumer_prefix: "counter",
        resident_label: "counter resident",
        replay_class: |_| ReplayClass::Rolling,
        rolling_limit: 10,
        timed_limit: 10,
        timed_retention: Duration::days(30),
    }
}

fn load(directory: &TempDir, codec: Arc<CounterCodec>) -> DurableStore<(), u64> {
    DurableStore::load(
        Utf8Path::from_path(directory.path()).unwrap(),
        codec,
        options(),
    )
    .unwrap()
}

fn set_counter(store: &mut DurableStore<(), u64>, value: u64) -> Result<(), QueueError> {
    store.transaction(|snapshot, _| {
        snapshot.next_id = value;
        Ok(())
    })
}

#[test]
fn failures_before_visibility_restore_memory_and_disk() {
    for stage in [PersistFailure::BeforeWrite, PersistFailure::BeforeRename] {
        let directory = TempDir::new().unwrap();
        let codec = Arc::new(CounterCodec::default());
        let mut store = load(&directory, codec.clone());
        set_counter(&mut store, 1).unwrap();
        store.persist_failure = Some(stage);
        let error = set_counter(&mut store, 2).unwrap_err();
        assert!(matches!(error.kind, QueueErrorKind::Io(_)));
        assert_eq!(store.state.next_id, 1);
        assert_eq!(
            fs::read(directory.path().join("counter.bin")).unwrap(),
            1_u64.to_le_bytes()
        );
        assert_eq!(store.directory_sync_attempts, 1);
        drop(store);
        assert_eq!(load(&directory, codec).state.next_id, 1);
    }
}

#[test]
fn current_uncertainty_is_retained_and_prior_barrier_blocks_mutation() {
    let directory = TempDir::new().unwrap();
    let codec = Arc::new(CounterCodec::default());
    let mut store = load(&directory, codec.clone());
    set_counter(&mut store, 1).unwrap();
    store.persist_failure = Some(PersistFailure::AfterRename);
    let error = set_counter(&mut store, 2).unwrap_err();
    assert!(matches!(error.kind, QueueErrorKind::DurabilityUncertain(_)));
    assert_eq!(store.state.next_id, 2);
    assert_eq!(
        fs::read(directory.path().join("counter.bin")).unwrap(),
        2_u64.to_le_bytes()
    );
    assert_eq!(store.directory_sync_attempts, 2);
    let error = store
        .transaction::<()>(|_, _| panic!("prior barrier must run first"))
        .unwrap_err();
    assert!(matches!(error.kind, QueueErrorKind::PriorSync(_)));
    assert_eq!(store.state.next_id, 2);
    assert_eq!(store.directory_sync_attempts, 3);
    store.persist_failure = None;
    store.sync_prior().unwrap();
    assert_eq!(store.directory_sync_attempts, 4);
    store.sync_prior().unwrap();
    assert_eq!(store.directory_sync_attempts, 4);
    set_counter(&mut store, 3).unwrap();
    assert_eq!(store.directory_sync_attempts, 5);
    drop(store);
    let mut reopened = load(&directory, codec);
    assert_eq!(reopened.state.next_id, 3);
    reopened.sync_prior().unwrap();
    assert_eq!(reopened.directory_sync_attempts, 1);
}

#[test]
fn mutation_and_codec_rejection_restore_the_snapshot() {
    let directory = TempDir::new().unwrap();
    let codec = Arc::new(CounterCodec::default());
    let mut store = load(&directory, codec.clone());
    set_counter(&mut store, 7).unwrap();
    let error = store
        .transaction::<()>(|state, _| {
            state.next_id = 8;
            Err(QueueError::new(
                "reject counter",
                QueueErrorKind::ReplayCapacity,
            ))
        })
        .unwrap_err();
    assert!(matches!(error.kind, QueueErrorKind::ReplayCapacity));
    assert_eq!(store.state.next_id, 7);
    codec.reject_encoding.store(true, Ordering::SeqCst);
    let error = set_counter(&mut store, 9).unwrap_err();
    let QueueErrorKind::Codec(CodecError {
        kind: CodecErrorKind::Encode(source),
    }) = error.kind
    else {
        panic!("expected concrete encode failure");
    };
    assert_eq!(
        source.downcast_ref::<io::Error>().unwrap().to_string(),
        "counter encoding rejected"
    );
    assert_eq!(store.state.next_id, 7);
    assert_eq!(
        fs::read(directory.path().join("counter.bin")).unwrap(),
        7_u64.to_le_bytes()
    );
}

#[test]
fn lock_and_decode_failures_retain_concrete_causes_and_original_bytes() {
    let directory = TempDir::new().unwrap();
    let codec = Arc::new(CounterCodec::default());
    let store = load(&directory, codec.clone());
    let blocked = DurableStore::load(
        Utf8Path::from_path(directory.path()).unwrap(),
        codec.clone(),
        options(),
    );
    let error = match blocked {
        Ok(_) => panic!("second lock must fail"),
        Err(error) => error,
    };
    let QueueErrorKind::Locked(source) = error.kind else {
        panic!("expected lock contention");
    };
    assert_eq!(
        source.raw_os_error(),
        fs2::lock_contended_error().raw_os_error()
    );
    drop(store);
    let path = directory.path().join("counter.bin");
    fs::write(&path, b"broken counter").unwrap();
    // This case tests the codec cause after the private-file boundary succeeds.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    }
    let rejected = DurableStore::load(
        Utf8Path::from_path(directory.path()).unwrap(),
        codec,
        options(),
    );
    let error = match rejected {
        Ok(_) => panic!("malformed snapshot must fail"),
        Err(error) => error,
    };
    let QueueErrorKind::Codec(CodecError {
        kind: CodecErrorKind::Decode(source),
    }) = error.kind
    else {
        panic!("expected concrete decode failure");
    };
    assert!(
        source
            .downcast_ref::<std::array::TryFromSliceError>()
            .is_some()
    );
    assert_eq!(fs::read(path).unwrap(), b"broken counter");
}

#[test]
fn invalid_storage_names_and_lock_collisions_reject_before_filesystem_changes() {
    for (file_name, lock_name) in [
        ("", "counter.lock"),
        (".", "counter.lock"),
        ("..", "counter.lock"),
        ("../counter.bin", "counter.lock"),
        ("/counter.bin", "counter.lock"),
        ("C:\\counter.bin", "counter.lock"),
        ("nested/counter.bin", "counter.lock"),
        ("counter.bin", "../counter.lock"),
        ("counter.bin", "/counter.lock"),
        ("counter.bin", "counter.bin"),
        ("counter.bin", "counter.bin.tmp"),
    ] {
        let directory = TempDir::new().unwrap();
        let root = Utf8Path::from_path(directory.path()).unwrap();
        let untouched = root.join("must-not-be-created");
        let mut configuration = options();
        configuration.file_name = file_name;
        configuration.lock_name = lock_name;
        let codec = Arc::new(CounterCodec::default());
        let result = DurableStore::load(&untouched, codec.clone(), configuration.clone());
        let error = match result {
            Ok(_) => panic!("invalid layout must reject"),
            Err(error) => error,
        };
        assert!(matches!(error.kind, QueueErrorKind::InvalidStorageLayout));
        assert_eq!(error.path.as_deref(), Some(untouched.as_path()));
        assert!(!untouched.exists());
        fs::write(root.join("counter.bin"), b"original snapshot").unwrap();
        fs::write(root.join("counter.lock"), b"original lock").unwrap();
        assert!(DurableStore::load(root, codec, configuration).is_err());
        assert_eq!(
            fs::read(root.join("counter.bin")).unwrap(),
            b"original snapshot"
        );
        assert_eq!(
            fs::read(root.join("counter.lock")).unwrap(),
            b"original lock"
        );
        assert_eq!(fs::read_dir(root).unwrap().count(), 2);
    }
}

#[cfg(unix)]
fn startup_entry_witness(path: &std::path::Path) -> (u64, u64, u32, u32, u64, Vec<u8>) {
    use std::os::unix::fs::MetadataExt as _;
    let metadata = fs::symlink_metadata(path).unwrap();
    let bytes = if metadata.file_type().is_symlink() {
        use std::os::unix::ffi::OsStrExt as _;
        fs::read_link(path).unwrap().as_os_str().as_bytes().to_vec()
    } else if metadata.is_file() {
        fs::read(path).unwrap()
    } else {
        Vec::new()
    };
    (
        metadata.dev(),
        metadata.ino(),
        metadata.uid(),
        metadata.mode(),
        metadata.nlink(),
        bytes,
    )
}

#[cfg(unix)]
#[test]
fn private_legacy_temporary_is_restricted_without_becoming_authoritative() {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};
    for snapshot_present in [false, true] {
        let directory = tempfile::Builder::new()
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let metadata = fs::symlink_metadata(directory.path()).unwrap();
        assert!(metadata.file_type().is_dir());
        assert!(!metadata.file_type().is_symlink());
        // SAFETY: geteuid has no memory-safety preconditions.
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
        assert_eq!(metadata.mode() & 0o7777, 0o700);
        let directory_identity = (
            metadata.dev(),
            metadata.ino(),
            metadata.uid(),
            metadata.mode() & 0o7777,
        );
        let assert_directory_identity = || {
            let current = fs::symlink_metadata(directory.path()).unwrap();
            assert!(current.file_type().is_dir());
            assert!(!current.file_type().is_symlink());
            assert_eq!(
                (
                    current.dev(),
                    current.ino(),
                    current.uid(),
                    current.mode() & 0o7777,
                ),
                directory_identity
            );
        };
        let snapshot = directory.path().join("counter.bin");
        if snapshot_present {
            fs::write(&snapshot, 3_u64.to_le_bytes()).unwrap();
            fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o644)).unwrap();
        }
        let temporary = directory.path().join("counter.bin.tmp");
        fs::write(&temporary, b"not a committed counter snapshot").unwrap();
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644)).unwrap();
        let before = startup_entry_witness(&temporary);
        let codec = Arc::new(CounterCodec::default());
        let mut store = load(&directory, codec.clone());
        assert_directory_identity();
        let after = startup_entry_witness(&temporary);
        assert_eq!(
            (after.0, after.1, after.2, after.4, &after.5),
            (before.0, before.1, before.2, before.4, &before.5)
        );
        assert_eq!(after.3 & 0o7777, 0o600);
        assert_eq!(store.state.next_id, if snapshot_present { 3 } else { 0 });
        assert_eq!(snapshot.exists(), snapshot_present);
        set_counter(&mut store, 7).unwrap();
        let committed = fs::metadata(&snapshot).unwrap();
        assert_eq!((committed.dev(), committed.ino()), (before.0, before.1));
        assert_eq!(committed.mode() & 0o7777, 0o600);
        assert_eq!(fs::read(&snapshot).unwrap(), 7_u64.to_le_bytes());
        drop(store);
        assert_eq!(load(&directory, codec).state.next_id, 7);
        assert_directory_identity();
    }
}

#[cfg(unix)]
#[test]
fn unsafe_startup_temporary_refuses_before_any_layout_entry_is_restricted() {
    use std::os::unix::fs::{PermissionsExt as _, symlink};
    for kind in [
        "exposed",
        "symlink",
        "dangling",
        "hardlink",
        "directory",
        "mode",
    ] {
        let directory = TempDir::new().unwrap();
        let exposed = kind == "exposed";
        if exposed {
            fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        }
        let lock = directory.path().join("counter.lock");
        let snapshot = directory.path().join("counter.bin");
        let temporary = directory.path().join("counter.bin.tmp");
        let victim = directory.path().join("unrelated");
        fs::write(&lock, b"existing lock inode").unwrap();
        fs::write(&snapshot, 3_u64.to_le_bytes()).unwrap();
        fs::write(&victim, b"unrelated bytes").unwrap();
        for path in [&lock, &snapshot, &victim] {
            fs::set_permissions(
                path,
                fs::Permissions::from_mode(if exposed { 0o600 } else { 0o644 }),
            )
            .unwrap();
        }
        match kind {
            "symlink" => symlink(&victim, &temporary).unwrap(),
            "dangling" => symlink(directory.path().join("missing"), &temporary).unwrap(),
            "hardlink" => fs::hard_link(&victim, &temporary).unwrap(),
            "directory" => fs::create_dir(&temporary).unwrap(),
            _ => {
                fs::write(&temporary, b"stale temporary bytes").unwrap();
                fs::set_permissions(
                    &temporary,
                    fs::Permissions::from_mode(if kind == "mode" { 0o640 } else { 0o644 }),
                )
                .unwrap();
            }
        }
        let paths = [&lock, &snapshot, &temporary, &victim];
        let before: Vec<_> = paths
            .iter()
            .map(|path| startup_entry_witness(path))
            .collect();
        let result = DurableStore::load(
            Utf8Path::from_path(directory.path()).unwrap(),
            Arc::new(CounterCodec::default()),
            options(),
        );
        let error = match result {
            Ok(_) => panic!("unsafe temporary {kind} must refuse startup"),
            Err(error) => error,
        };
        let QueueErrorKind::Io(cause) = error.kind else {
            panic!("unsafe temporary must retain its filesystem refusal cause");
        };
        assert!(
            cause.kind() == io::ErrorKind::PermissionDenied
                || cause.raw_os_error() == Some(libc::ELOOP),
            "unexpected startup refusal for {kind}: {cause}"
        );
        for (path, original) in paths.iter().zip(before) {
            assert_eq!(
                startup_entry_witness(path),
                original,
                "entry changed for {kind}"
            );
        }
    }
}

#[cfg(unix)]
#[test]
fn absent_startup_temporary_stays_absent_and_private_temporary_in_755_remains_supported() {
    use std::os::unix::fs::PermissionsExt as _;
    for temporary_present in [false, true] {
        let directory = TempDir::new().unwrap();
        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
        let temporary = directory.path().join("counter.bin.tmp");
        if temporary_present {
            fs::write(&temporary, b"not authoritative").unwrap();
            fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600)).unwrap();
        }
        let mut store = load(&directory, Arc::new(CounterCodec::default()));
        assert_eq!(store.state.next_id, 0);
        assert_eq!(temporary.exists(), temporary_present);
        set_counter(&mut store, 9).unwrap();
        assert_eq!(
            fs::read(directory.path().join("counter.bin")).unwrap(),
            9_u64.to_le_bytes()
        );
    }
}

#[cfg(unix)]
#[test]
fn unsafe_temporary_introduced_after_startup_is_never_tightened_or_truncated() {
    use std::os::unix::fs::PermissionsExt as _;
    let directory = TempDir::new().unwrap();
    let mut store = load(&directory, Arc::new(CounterCodec::default()));
    set_counter(&mut store, 3).unwrap();
    let snapshot = directory.path().join("counter.bin");
    let temporary = directory.path().join("counter.bin.tmp");
    fs::write(&temporary, b"post-start public inode").unwrap();
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644)).unwrap();
    let before = startup_entry_witness(&temporary);
    let error = set_counter(&mut store, 4).unwrap_err();
    assert!(matches!(error.kind, QueueErrorKind::Io(_)));
    assert_eq!(startup_entry_witness(&temporary), before);
    assert_eq!(store.state.next_id, 3);
    assert_eq!(fs::read(snapshot).unwrap(), 3_u64.to_le_bytes());
}
