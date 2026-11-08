use camino::Utf8PathBuf;
use dione_attention::{
    store::{AttentionStore, StoreError},
    store_access::{
        AttentionInitializationError, AttentionInitializationErrorKind, AttentionStoreAccess,
        AttentionStoreAccessErrorKind,
    },
};
use std::{
    io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

fn open() -> (tempfile::TempDir, Arc<AttentionStoreAccess>) {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("attention/records.json")).unwrap();
    let store = AttentionStore::open(path).unwrap();
    (
        dir,
        Arc::new(AttentionStoreAccess::from_initialized(Ok(store))),
    )
}

#[tokio::test]
async fn ordinary_transaction_persists_and_preserves_operation_causes() {
    let (_dir, access) = open();
    access.transact(|store| store.prune(0)).await.unwrap();
    let path = access
        .try_read(|store| Ok::<_, io::Error>(store.path().to_owned()))
        .unwrap();
    let value: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_eq!(value["schema_version"], 1);
    let error = access
        .transact(|_| Err::<(), _>(io::Error::other("operation fixture")))
        .await
        .unwrap_err();
    let AttentionStoreAccessErrorKind::Operation(source) = error.kind else {
        panic!("operation cause was lost")
    };
    assert_eq!(source.to_string(), "operation fixture");
}

#[tokio::test]
async fn failed_initialization_remains_sticky_without_running_operations() {
    let dir = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().join("attention/records.json")).unwrap();
    let held = AttentionStore::open(path.clone()).unwrap();
    let cause = Arc::new(AttentionInitializationError {
        kind: AttentionInitializationErrorKind::Store(
            AttentionStore::open(path)
                .err()
                .expect("second open must retain exclusive-lock failure"),
        ),
    });
    let access = Arc::new(AttentionStoreAccess::from_initialized(Err(cause.clone())));
    drop(held);
    let invoked = Arc::new(AtomicBool::new(false));
    let flag = invoked.clone();
    let error = access
        .transact(move |_| {
            flag.store(true, Ordering::SeqCst);
            Ok::<_, io::Error>(())
        })
        .await
        .unwrap_err();
    let AttentionStoreAccessErrorKind::Initialization(retained) = error.kind else {
        panic!("initialization cause was replaced")
    };
    assert!(Arc::ptr_eq(&retained, &cause));
    assert!(matches!(
        &retained.kind,
        AttentionInitializationErrorKind::Store(StoreError::Locked { .. })
    ));
    assert!(!invoked.load(Ordering::SeqCst));
    let error = access.try_read(|_| Ok::<_, io::Error>(())).unwrap_err();
    assert!(matches!(
        error.kind,
        AttentionStoreAccessErrorKind::Initialization(_)
    ));
}

#[tokio::test]
async fn worker_panic_poisoning_is_causal_and_fails_closed() {
    let (_dir, access) = open();
    let error = access
        .transact(|_| -> Result<(), io::Error> { panic!("worker fixture") })
        .await
        .unwrap_err();
    let AttentionStoreAccessErrorKind::Worker(source) = error.kind else {
        panic!("worker cause was lost")
    };
    assert!(source.is_panic());
    let error = access
        .transact(|_| Ok::<_, io::Error>(()))
        .await
        .unwrap_err();
    assert!(matches!(
        error.kind,
        AttentionStoreAccessErrorKind::LockPoisoned
    ));
    let error = access.try_read(|_| Ok::<_, io::Error>(())).unwrap_err();
    assert!(matches!(
        error.kind,
        AttentionStoreAccessErrorKind::LockPoisoned
    ));
}

#[tokio::test(flavor = "current_thread")]
async fn held_transaction_keeps_worker_responsive_and_read_nonblocking() {
    let (_dir, access) = open();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::sync_channel(0);
    let writer = access.clone();
    let task = tokio::spawn(async move {
        writer
            .transact(move |_| {
                entered.send(()).unwrap();
                released.recv().unwrap();
                Ok::<_, io::Error>(())
            })
            .await
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), ready)
        .await
        .unwrap()
        .unwrap();
    let reader = access.clone();
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let checker = std::thread::spawn(move || {
        sender
            .send(reader.try_read(|_| Ok::<_, io::Error>(())))
            .unwrap();
    });
    let observed = receiver.recv_timeout(std::time::Duration::from_secs(2));
    release.send(()).unwrap();
    task.await.unwrap().unwrap();
    checker.join().unwrap();
    assert!(
        matches!(observed, Ok(Err(error)) if matches!(error.kind, AttentionStoreAccessErrorKind::Busy))
    );
    access.try_read(|_| Ok::<_, io::Error>(())).unwrap();
}

#[tokio::test]
async fn cancelling_waiter_does_not_cancel_started_store_work() {
    let (_dir, access) = open();
    let (entered, ready) = tokio::sync::oneshot::channel();
    let (release, released) = std::sync::mpsc::sync_channel(0);
    let (completed, completion) = tokio::sync::oneshot::channel();
    let worker = access.clone();
    let waiting = tokio::spawn(async move {
        worker
            .transact(move |store| {
                entered.send(()).unwrap();
                released.recv().unwrap();
                store.prune(0)?;
                completed.send(()).unwrap();
                Ok::<_, StoreError>(())
            })
            .await
    });
    ready.await.unwrap();
    waiting.abort();
    assert!(waiting.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(5), completion)
        .await
        .unwrap()
        .unwrap();
    access.transact(|store| store.prune(0)).await.unwrap();
}
