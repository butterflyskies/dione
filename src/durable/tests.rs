#[path = "test_codec.rs"]
mod test_codec;

use super::{
    state::{ConsumerKind, ConsumerRegistration, Entry, Lease, ReplayReceipt},
    *,
};
use camino::Utf8Path;
use chrono::{DateTime, Utc};
use std::{
    fs,
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tempfile::TempDir;
use test_codec::{WorkItem, WorkKey, WorkshopCodec};

#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt};

type Workshop = DurableQueue<WorkItem, WorkKey>;
const LEASE: Duration = Duration::from_secs(60);

#[cfg(unix)]
#[tokio::test]
async fn private_snapshots_never_publish_a_permissive_stale_temporary_inode() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let temporary = root(&dir).join("workshop.toml.tmp");
    fs::write(&temporary, b"stale public inode").unwrap();
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o644)).unwrap();
    let result = queue.enqueue(admission(WorkKey::Batch(7))).await;
    if result.is_ok() {
        assert_eq!(
            fs::metadata(root(&dir).join("workshop.toml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600,
            "a successful commit must not expose private content through a stale permissive inode"
        );
    } else {
        assert!(!root(&dir).join("workshop.toml").exists());
        assert_eq!(fs::read(&temporary).unwrap(), b"stale public inode");
    }
}

#[cfg(unix)]
#[tokio::test]
async fn private_snapshots_remain_owner_only_across_replacement_in_a_traversable_parent() {
    let directory = TempDir::new().unwrap();
    let executable = std::env::current_exe().unwrap();
    let mut command = tokio::process::Command::new(&executable);
    command
        .args([
            "--exact",
            "durable::tests::private_snapshot_creation_child_022",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env("DIONE_PRIVATE_SNAPSHOT_CHILD", directory.path())
        .kill_on_drop(true);
    // SAFETY: the only pre-exec action is async-signal-safe umask in the child.
    // The parallel parent test process never changes its process-global mask.
    unsafe {
        command.pre_exec(|| {
            libc::umask(0o022);
            Ok(())
        });
    }
    let mut child = command.spawn().unwrap();
    let status = match tokio::time::timeout(Duration::from_secs(15), child.wait()).await {
        Ok(status) => status.unwrap(),
        Err(_) => {
            child.kill().await.unwrap();
            child.wait().await.unwrap();
            panic!("controlled022 private snapshot child exceeded its deadline");
        }
    };
    assert!(
        status.success(),
        "controlled022 private snapshot child must pass"
    );
    let receipt: serde_json::Value = serde_json::from_slice(
        &fs::read(directory.path().join("creation-child-receipt.json"))
            .expect("exact child fixture must produce its receipt"),
    )
    .unwrap();
    assert_eq!(receipt["fixture"], "private_snapshot_creation_child_022");
    assert_eq!(receipt["umask"], "022");
    assert_eq!(receipt["commits"], 2);
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "exact subprocess fixture; parent supplies owned directory and child-only022 mask"]
async fn private_snapshot_creation_child_022() {
    let path =
        std::env::var_os("DIONE_PRIVATE_SNAPSHOT_CHILD").expect("parent fixture marker required");
    let directory = Utf8Path::from_path(std::path::Path::new(&path)).unwrap();
    let metadata = fs::symlink_metadata(directory).unwrap();
    assert!(metadata.is_dir());
    // SAFETY: geteuid has no memory-safety preconditions.
    assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    assert_eq!(fs::read_dir(directory).unwrap().count(), 0);
    // This mutates only this explicitly isolated child, never a parallel parent.
    // SAFETY: umask has no memory-safety preconditions.
    assert_eq!(unsafe { libc::umask(0o022) }, 0o022);
    fs::set_permissions(directory, fs::Permissions::from_mode(0o755)).unwrap();
    let queue = Workshop::load(directory, Arc::new(WorkshopCodec), options()).unwrap();
    queue.test_set_now(now());
    for key in [1, 2] {
        queue.enqueue(admission(WorkKey::Batch(key))).await.unwrap();
        let metadata = fs::metadata(directory.join("workshop.toml")).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        // SAFETY: geteuid has no memory-safety preconditions.
        assert_eq!(metadata.uid(), unsafe { libc::geteuid() });
    }
    drop(queue);
    let before = fs::read(directory.join("workshop.toml")).unwrap();
    let reopened = Workshop::load(directory, Arc::new(WorkshopCodec), options());
    assert!(
        reopened.is_ok(),
        "private snapshot and lock must reopen under022"
    );
    assert_eq!(fs::read(directory.join("workshop.toml")).unwrap(), before);
    let mut receipt = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(directory.join("creation-child-receipt.json"))
        .unwrap();
    std::io::Write::write_all(
        &mut receipt,
        br#"{"fixture":"private_snapshot_creation_child_022","umask":"022","commits":2}"#,
    )
    .unwrap();
}

#[tokio::test]
async fn binding_controller_preserves_pull_primary_without_allocating_registration() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let controller = queue.binding_controller();
    let untouched = queue.test_snapshot().await;
    assert!(untouched.consumers.is_empty());
    assert_eq!(untouched.primary_consumer, None);
    assert_eq!(untouched.next_consumer_generation, 0);

    let first = EndpointId::new("unclaimed kiln outlet").unwrap();
    controller.bind(Some(first.clone())).await.unwrap();
    let bound = snapshot(&dir);
    assert_eq!(bound.endpoint, Some(first));
    assert!(bound.consumers.is_empty());
    assert_eq!(bound.primary_consumer, None);
    assert_eq!(bound.next_consumer_generation, 0);
    assert_eq!(bound.next_id, 0);
    assert_eq!(bound.next_lease_generation, 0);

    let consumer = worker(&queue).await;
    assert_eq!(consumer.as_str(), "worker0");
    let registered = snapshot(&dir);
    let next = EndpointId::new("rebound kiln outlet").unwrap();
    let (sender, mut receiver) = tokio::sync::watch::channel(registered.endpoint.clone());
    controller
        .bind_and_publish(Some(next.clone()), sender.clone())
        .await
        .unwrap();
    let rebound = snapshot(&dir);
    assert_eq!(rebound.endpoint, Some(next.clone()));
    assert!(receiver.has_changed().unwrap());
    assert_eq!(*receiver.borrow_and_update(), Some(next));
    assert_eq!(rebound.primary_consumer, Some(consumer.clone()));
    assert_eq!(rebound.consumers.len(), 1);
    let retained = &rebound.consumers[0];
    assert_eq!(retained.id, consumer);
    assert_eq!(retained.kind, ConsumerKind::Pull);
    assert_eq!(retained.label, "shift-a");
    assert_eq!(retained.expires_at, registered.consumers[0].expires_at);
    assert_eq!(retained.ttl_seconds, registered.consumers[0].ttl_seconds);
    assert_eq!(rebound.next_consumer_generation, 1);
    assert_eq!(rebound.next_id, 0);
    assert_eq!(rebound.next_lease_generation, 0);
    drop(controller);
    drop(queue);
    let reopened = open(&dir);
    assert_eq!(reopened.status().await.primary_consumer, Some(consumer));
    assert_eq!(reopened.status().await.consumers.len(), 1);
}

#[tokio::test]
async fn binding_watch_changes_only_for_retained_new_endpoint_inside_owned_operation() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let resident = queue.register_resident().await.unwrap();
    let old = EndpointId::new("kiln old outlet").unwrap();
    resident.bind(Some(old.clone())).await.unwrap();
    let (sender, mut receiver) = tokio::sync::watch::channel(Some(old.clone()));

    resident
        .bind_and_publish(Some(old.clone()), sender.clone())
        .await
        .unwrap();
    assert!(!receiver.has_changed().unwrap());
    assert_eq!(*receiver.borrow(), Some(old.clone()));

    let next = EndpointId::new("kiln next outlet").unwrap();
    let before = bytes(&dir);
    queue
        .test_set_persist_failure(Some(PersistFailure::BeforeRename))
        .await;
    assert!(matches!(
        resident
            .bind_and_publish(Some(next.clone()), sender.clone())
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::Io(_)
    ));
    assert_eq!(bytes(&dir), before);
    assert_eq!(queue.test_snapshot().await.endpoint, Some(old.clone()));
    assert_eq!(*receiver.borrow(), Some(old.clone()));
    assert!(!receiver.has_changed().unwrap());
    queue.test_set_persist_failure(None).await;

    let (entered, release) = queue.test_pause_binding_publish();
    let owned_resident = resident.clone();
    let owned_sender = sender.clone();
    let next_binding = next.clone();
    let caller = tokio::spawn(async move {
        owned_resident
            .bind_and_publish(Some(next_binding), owned_sender)
            .await
    });
    entered.await.unwrap();
    assert_eq!(snapshot(&dir).endpoint, Some(next.clone()));
    assert_eq!(*receiver.borrow(), Some(old));
    assert!(!receiver.has_changed().unwrap());
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    queue.drain_owned().await;
    assert!(receiver.has_changed().unwrap());
    assert_eq!(*receiver.borrow_and_update(), Some(next.clone()));
    assert!(!receiver.has_changed().unwrap());
    resident
        .bind_and_publish(Some(next.clone()), sender.clone())
        .await
        .unwrap();
    assert!(!receiver.has_changed().unwrap());

    let uncertain = EndpointId::new("kiln uncertain outlet").unwrap();
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(matches!(
        resident
            .bind_and_publish(Some(uncertain.clone()), sender.clone())
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::DurabilityUncertain(_)
    ));
    assert_eq!(snapshot(&dir).endpoint, Some(uncertain.clone()));
    assert_eq!(
        queue.test_snapshot().await.endpoint,
        Some(uncertain.clone())
    );
    assert!(receiver.has_changed().unwrap());
    assert_eq!(*receiver.borrow_and_update(), Some(uncertain.clone()));
    assert!(!receiver.has_changed().unwrap());
    queue.test_set_persist_failure(None).await;
    resident
        .bind_and_publish(Some(uncertain), sender.clone())
        .await
        .unwrap();
    queue.drain_owned().await;
    assert!(!receiver.has_changed().unwrap());
}

#[tokio::test]
async fn resident_deferral_uses_persisted_metadata_and_opaque_endpoint_fence() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let resident = queue.register_resident().await.unwrap();
    let endpoint = EndpointId::new("furnace station 2").unwrap();
    resident.bind(Some(endpoint.clone())).await.unwrap();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(77)))
        .await
        .unwrap();
    assert!(
        resident
            .next_event(
                &EndpointId::new("other furnace").unwrap(),
                Duration::ZERO,
                LEASE
            )
            .await
            .unwrap()
            .is_none()
    );
    let first = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert!(
        resident
            .defer(&first.delivery_token, Duration::from_secs(2))
            .await
            .unwrap()
    );
    assert!(
        resident
            .next_event(&endpoint, Duration::ZERO, LEASE)
            .await
            .unwrap()
            .is_none()
    );
    drop(resident);
    drop(queue);
    let queue = open(&dir);
    queue.test_set_now(now() + chrono::Duration::seconds(2));
    let resident = queue.register_resident().await.unwrap();
    let second = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(second.event_id, first.event_id);
    assert_ne!(second.delivery_token, first.delivery_token);
    assert!(resident.ack(&second.delivery_token).await.unwrap());
    let mut fixed = admission(WorkKey::Reservation(78));
    fixed.deferrable = false;
    queue.enqueue_resident(fixed).await.unwrap();
    let fixed = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    let before = bytes(&dir);
    assert!(matches!(
        resident
            .defer(&fixed.delivery_token, Duration::from_secs(2))
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::NotDeferrable
    ));
    assert_eq!(bytes(&dir), before);
    assert!(resident.ack(&fixed.delivery_token).await.unwrap());
}
fn now() -> DateTime<Utc> {
    DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc)
}
fn options() -> QueueOptions<WorkKey> {
    QueueOptions {
        file_name: "workshop.toml",
        lock_name: "workshop.lock",
        consumer_prefix: "worker",
        resident_label: "workshop-resident",
        replay_class: |key| match key {
            WorkKey::Batch(_) => ReplayClass::Rolling,
            WorkKey::Reservation(_) => ReplayClass::Timed,
        },
        rolling_limit: 10_000,
        timed_limit: 10_000,
        timed_retention: chrono::Duration::days(30),
    }
}
fn root(dir: &TempDir) -> &Utf8Path {
    Utf8Path::from_path(dir.path()).unwrap()
}
fn open(dir: &TempDir) -> Workshop {
    let queue = Workshop::load(root(dir), Arc::new(WorkshopCodec), options()).unwrap();
    queue.test_set_now(now());
    queue
}
fn item() -> WorkItem {
    WorkItem {
        station: "kiln east".into(),
        quantity: 17,
    }
}
fn admission(key: WorkKey) -> Admission<WorkItem, WorkKey> {
    Admission {
        payload: item(),
        replay_key: Some(key),
        deferrable: true,
    }
}
fn seed(dir: &TempDir, state: &QueueSnapshot<WorkItem, WorkKey>) {
    fs::write(
        root(dir).join("workshop.toml"),
        WorkshopCodec.encode(state).unwrap(),
    )
    .unwrap();
    #[cfg(unix)]
    fs::set_permissions(
        root(dir).join("workshop.toml"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
}
#[cfg(unix)]
#[test]
fn private_snapshots_reopen_rejects_exposed_or_aliased_snapshot_and_lock() {
    for name in ["workshop.toml", "workshop.lock"] {
        for boundary in ["exposed", "symlink", "hardlink", "directory"] {
            let dir = TempDir::new().unwrap();
            fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o755)).unwrap();
            let path = root(&dir).join(name);
            let original = b"private fixture remains untouched";
            match boundary {
                "directory" => fs::create_dir(&path).unwrap(),
                "symlink" | "hardlink" => {
                    let target = root(&dir).join("retained-target");
                    fs::write(&target, original).unwrap();
                    fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
                    if boundary == "symlink" {
                        std::os::unix::fs::symlink(&target, &path).unwrap();
                    } else {
                        fs::hard_link(&target, &path).unwrap();
                    }
                }
                _ => {
                    fs::write(&path, original).unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
                }
            }
            let result = Workshop::load(root(&dir), Arc::new(WorkshopCodec), options());
            let error = result
                .err()
                .expect("unsafe durable boundary must be refused");
            assert!(
                matches!(error.kind, QueueErrorKind::Io(_)),
                "{name}/{boundary}: {error:?}"
            );
            if boundary != "directory" {
                assert_eq!(fs::read(&path).unwrap(), original, "{name}/{boundary}");
            }
            if boundary == "exposed" {
                assert_eq!(
                    fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o644
                );
            }
        }
    }
}

#[cfg(unix)]
#[test]
fn legacy_private_permissions_preflight_preserves_lock_when_snapshot_is_unsafe() {
    for boundary in ["symlink", "hardlink", "directory", "unsupported_mode"] {
        let dir = TempDir::new().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let lock = root(&dir).join("workshop.lock");
        fs::write(&lock, b"retained lock").unwrap();
        fs::set_permissions(&lock, fs::Permissions::from_mode(0o644)).unwrap();
        let snapshot = root(&dir).join("workshop.toml");
        let target = root(&dir).join("retained-target");
        fs::write(&target, b"unchanged target").unwrap();
        fs::set_permissions(&target, fs::Permissions::from_mode(0o600)).unwrap();
        match boundary {
            "symlink" => std::os::unix::fs::symlink(&target, &snapshot).unwrap(),
            "hardlink" => fs::hard_link(&target, &snapshot).unwrap(),
            "directory" => fs::create_dir(&snapshot).unwrap(),
            _ => {
                fs::write(&snapshot, b"unsupported snapshot").unwrap();
                fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o640)).unwrap();
            }
        }
        assert!(Workshop::load(root(&dir), Arc::new(WorkshopCodec), options()).is_err());
        assert_eq!(fs::read(&lock).unwrap(), b"retained lock");
        assert_eq!(
            fs::metadata(&lock).unwrap().permissions().mode() & 0o777,
            0o644
        );
        assert_eq!(fs::read(&target).unwrap(), b"unchanged target");
        assert_eq!(
            fs::metadata(&target).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

fn bytes(dir: &TempDir) -> Vec<u8> {
    fs::read(root(dir).join("workshop.toml")).unwrap()
}
fn snapshot(dir: &TempDir) -> QueueSnapshot<WorkItem, WorkKey> {
    WorkshopCodec.decode(&bytes(dir)).unwrap()
}
async fn worker(queue: &Workshop) -> ConsumerId {
    queue
        .register_consumer("shift-a".into(), Duration::from_secs(3600), true, true)
        .await
        .unwrap()
        .consumer_id
}
fn entry(id: u64, keys: Vec<WorkKey>) -> Entry<WorkItem, WorkKey> {
    Entry {
        id: EventId::new(id),
        payload: item(),
        replay_keys: keys,
        consumer_id: None,
        endpoint: None,
        visibility: EntryVisibility::Public,
        deferrable: true,
        deferred_until: None,
        lease: None,
    }
}

#[tokio::test]
async fn distinct_disk_codec_survives_close_lease_ack_reopen_and_deduplicates() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    assert!(queue.enqueue(admission(WorkKey::Batch(41))).await.unwrap());
    queue.drain_owned().await;
    drop(queue);
    let disk = String::from_utf8(bytes(&dir)).unwrap();
    assert!(disk.starts_with("workshop_format = 7\n"));
    assert!(disk.contains("station = \"kiln east\""));
    let queue = open(&dir);
    let consumer = worker(&queue).await;
    let event = queue
        .next_event(&consumer, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.event_id.get(), 0);
    assert_eq!(
        event.payload,
        WorkItem {
            station: "kiln east".into(),
            quantity: 17
        }
    );
    assert!(queue.ack(&consumer, &event.delivery_token).await.unwrap());
    drop(queue);
    let queue = open(&dir);
    assert!(!queue.enqueue(admission(WorkKey::Batch(41))).await.unwrap());
    assert_eq!(queue.status().await.queued, 0);
}

#[tokio::test]
async fn resident_capability_and_opaque_destination_survive_disk_reopen() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let endpoint = EndpointId::new("kiln/east:shift Δ").unwrap();
    let resident = queue.register_resident().await.unwrap();
    resident.bind(Some(endpoint.clone())).await.unwrap();
    assert!(matches!(
        queue
            .enqueue_resident(admission(WorkKey::Reservation(9)))
            .await
            .unwrap(),
        AdmissionOutcome::Committed
    ));
    let copied = resident.consumer_id().clone();
    assert!(matches!(
        queue
            .next_event(&copied, Duration::ZERO, LEASE)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::UnknownConsumer
    ));
    assert!(matches!(
        queue
            .register_consumer("workshop-resident".into(), LEASE, false, false)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::Identity(_)
    ));
    let impersonator = queue
        .register_consumer("ordinary pull".into(), LEASE, false, false)
        .await
        .unwrap();
    assert!(
        queue
            .next_event(&impersonator.consumer_id, Duration::ZERO, LEASE)
            .await
            .unwrap()
            .is_none()
    );
    drop(resident);
    drop(queue);
    let queue = open(&dir);
    let resident = queue.register_resident().await.unwrap();
    let event = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.payload, item());
    assert!(matches!(
        queue
            .ack(resident.consumer_id(), &event.delivery_token)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::UnknownConsumer
    ));
    assert!(resident.ack(&event.delivery_token).await.unwrap());
    drop(resident);
    drop(queue);
    let queue = open(&dir);
    assert!(
        !queue
            .enqueue(admission(WorkKey::Reservation(9)))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn accepted_admission_outlives_cancel_and_drain_then_reopens() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let (entered, release) = queue.test_pause_owned_completion(false);
    let producer = queue.clone();
    let task = tokio::spawn(async move { producer.enqueue(admission(WorkKey::Batch(22))).await });
    entered.await.unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    queue.drain_owned().await;
    drop(queue);
    let queue = open(&dir);
    let consumer = worker(&queue).await;
    assert_eq!(
        queue
            .next_event(&consumer, Duration::ZERO, LEASE)
            .await
            .unwrap()
            .unwrap()
            .payload,
        item()
    );
    assert!(!queue.enqueue(admission(WorkKey::Batch(22))).await.unwrap());
}

#[tokio::test]
async fn exclusive_lock_retains_concrete_cause_and_reopens_after_last_handle() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let clone = queue.clone();
    drop(queue);
    let error = match Workshop::load(root(&dir), Arc::new(WorkshopCodec), options()) {
        Ok(_) => panic!("second owner acquired live lock"),
        Err(error) => error,
    };
    let QueueErrorKind::Locked(cause) = error.kind else {
        panic!("expected concrete lock error")
    };
    assert!(cause.raw_os_error().is_some());
    drop(clone);
    let reopened = open(&dir);
    assert!(
        reopened
            .enqueue(admission(WorkKey::Batch(1)))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn event_counter_adjacent_success_exhaustion_and_duplicate_do_not_reuse_identity() {
    let dir = TempDir::new().unwrap();
    seed(
        &dir,
        &QueueSnapshot {
            next_id: u64::MAX - 1,
            ..Default::default()
        },
    );
    let queue = open(&dir);
    assert!(queue.enqueue(admission(WorkKey::Batch(1))).await.unwrap());
    assert_eq!(
        snapshot(&dir).entries.front().unwrap().id.get(),
        u64::MAX - 1
    );
    let before = bytes(&dir);
    assert!(!queue.enqueue(admission(WorkKey::Batch(1))).await.unwrap());
    assert!(matches!(
        queue
            .enqueue(admission(WorkKey::Batch(2)))
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::AllocationExhausted {
            allocation: AllocationKind::Event
        }
    ));
    assert_eq!(bytes(&dir), before);
    assert_eq!(queue.status().await.next_event_id.get(), u64::MAX);
}

#[tokio::test]
async fn rolling_receipts_evict_fifo_at_real_limit_without_limiting_queued_keys() {
    let dir = TempDir::new().unwrap();
    let state = QueueSnapshot {
        acknowledged: (0..10_000)
            .map(|n| ReplayReceipt {
                key: WorkKey::Batch(n),
                acknowledged_at: now(),
            })
            .collect(),
        ..Default::default()
    };
    seed(&dir, &state);
    let queue = open(&dir);
    let consumer = worker(&queue).await;
    assert!(
        queue
            .enqueue(admission(WorkKey::Batch(10_000)))
            .await
            .unwrap()
    );
    assert!(!queue.enqueue(admission(WorkKey::Batch(0))).await.unwrap());
    let event = queue
        .next_event(&consumer, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert!(queue.ack(&consumer, &event.delivery_token).await.unwrap());
    assert_eq!(snapshot(&dir).acknowledged.len(), 10_000);
    assert!(!queue.enqueue(admission(WorkKey::Batch(1))).await.unwrap());
    assert!(queue.enqueue(admission(WorkKey::Batch(0))).await.unwrap());
    assert!(
        queue
            .enqueue(admission(WorkKey::Reservation(0)))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn timed_real_capacity_duplicates_then_exact_expiry_do_not_expire_queued_keys() {
    let dir = TempDir::new().unwrap();
    let state = QueueSnapshot {
        next_id: 1,
        entries: [entry(0, vec![WorkKey::Reservation(0)])].into(),
        acknowledged: (1..10_000)
            .map(|n| ReplayReceipt {
                key: WorkKey::Reservation(n),
                acknowledged_at: now() - chrono::Duration::days(30) + chrono::Duration::seconds(1),
            })
            .collect(),
        ..Default::default()
    };
    seed(&dir, &state);
    let queue = open(&dir);
    assert!(
        !queue
            .enqueue(admission(WorkKey::Reservation(1)))
            .await
            .unwrap()
    );
    let before = bytes(&dir);
    assert!(matches!(
        queue
            .enqueue(admission(WorkKey::Reservation(10_000)))
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::ReplayCapacity
    ));
    assert_eq!(bytes(&dir), before);
    assert!(
        queue
            .enqueue(admission(WorkKey::Batch(10_000)))
            .await
            .unwrap()
    );
    queue.test_set_now(now() + chrono::Duration::seconds(1));
    assert!(
        queue
            .enqueue(admission(WorkKey::Reservation(1)))
            .await
            .unwrap()
    );
    assert!(
        !queue
            .enqueue(admission(WorkKey::Reservation(0)))
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn failed_invalidation_rolls_back_two_key_families_and_lease_then_selects_once() {
    let dir = TempDir::new().unwrap();
    seed(
        &dir,
        &QueueSnapshot {
            next_id: 1,
            entries: [entry(0, vec![WorkKey::Batch(3), WorkKey::Reservation(4)])].into(),
            ..Default::default()
        },
    );
    let queue = open(&dir);
    let consumer = worker(&queue).await;
    let leased = queue
        .next_event(&consumer, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    let before = bytes(&dir);
    queue
        .test_set_persist_failure(Some(PersistFailure::BeforeRename))
        .await;
    assert!(queue.invalidate(|_| true).await.is_err());
    assert_eq!(bytes(&dir), before);
    queue.test_set_persist_failure(None).await;
    assert!(!queue.enqueue(admission(WorkKey::Batch(3))).await.unwrap());
    assert!(
        !queue
            .enqueue(admission(WorkKey::Reservation(4)))
            .await
            .unwrap()
    );
    assert!(queue.ack(&consumer, &leased.delivery_token).await.unwrap());
    assert!(!queue.enqueue(admission(WorkKey::Batch(3))).await.unwrap());
    assert!(
        !queue
            .enqueue(admission(WorkKey::Reservation(4)))
            .await
            .unwrap()
    );
    assert!(queue.enqueue(admission(WorkKey::Batch(5))).await.unwrap());
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let removed = queue
        .invalidate(move |payload| {
            observed.fetch_add(1, Ordering::SeqCst);
            payload.quantity == 17
        })
        .await
        .unwrap();
    assert_eq!(removed.removed, 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(queue.enqueue(admission(WorkKey::Batch(5))).await.unwrap());
}

#[tokio::test]
async fn lease_max_rejection_restores_expired_lease_and_registration_touch() {
    let dir = TempDir::new().unwrap();
    seed(
        &dir,
        &QueueSnapshot {
            next_lease_generation: u64::MAX - 1,
            ..Default::default()
        },
    );
    let queue = open(&dir);
    let consumer = worker(&queue).await;
    queue.enqueue(admission(WorkKey::Batch(1))).await.unwrap();
    let leased = queue
        .next_event(&consumer, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        leased.delivery_token.as_str(),
        "dione-0-18446744073709551614"
    );
    let before = bytes(&dir);
    queue.test_set_now(now() + chrono::Duration::seconds(61));
    assert!(matches!(
        queue
            .next_event(&consumer, Duration::ZERO, LEASE)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::AllocationExhausted {
            allocation: AllocationKind::Lease
        }
    ));
    assert_eq!(bytes(&dir), before);
    assert_eq!(
        queue
            .test_snapshot()
            .await
            .entries
            .front()
            .unwrap()
            .lease
            .as_ref()
            .unwrap()
            .token,
        leased.delivery_token
    );
}

#[tokio::test]
async fn pull_and_resident_consumer_max_and_nonallocating_resident_refresh() {
    let pull_dir = TempDir::new().unwrap();
    seed(
        &pull_dir,
        &QueueSnapshot {
            next_consumer_generation: u64::MAX - 1,
            ..Default::default()
        },
    );
    let queue = open(&pull_dir);
    let consumer = worker(&queue).await;
    assert_eq!(consumer.as_str(), "worker18446744073709551614");
    let before = bytes(&pull_dir);
    assert!(matches!(
        queue
            .register_consumer("second".into(), LEASE, false, false)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::AllocationExhausted {
            allocation: AllocationKind::Consumer
        }
    ));
    assert_eq!(bytes(&pull_dir), before);
    let resident_dir = TempDir::new().unwrap();
    seed(
        &resident_dir,
        &QueueSnapshot {
            next_consumer_generation: u64::MAX - 1,
            next_id: u64::MAX - 1,
            ..Default::default()
        },
    );
    let resident_queue = open(&resident_dir);
    let resident = resident_queue.register_resident().await.unwrap();
    assert_eq!(
        resident.consumer_id().as_str(),
        "worker18446744073709551614"
    );
    let refreshed = resident_queue.register_resident().await.unwrap();
    assert_eq!(refreshed.consumer_id(), resident.consumer_id());
    resident
        .bind(Some(EndpointId::new("kiln:west").unwrap()))
        .await
        .unwrap();
    assert!(matches!(
        resident_queue
            .enqueue_resident(admission(WorkKey::Reservation(1)))
            .await
            .unwrap(),
        AdmissionOutcome::Committed
    ));
    let before = bytes(&resident_dir);
    assert!(matches!(
        resident_queue
            .enqueue_resident(admission(WorkKey::Reservation(1)))
            .await
            .unwrap(),
        AdmissionOutcome::Duplicate
    ));
    assert!(matches!(
        resident_queue
            .enqueue_resident(admission(WorkKey::Reservation(2)))
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::AllocationExhausted {
            allocation: AllocationKind::Event
        }
    ));
    assert_eq!(bytes(&resident_dir), before);
    let exhausted_dir = TempDir::new().unwrap();
    seed(
        &exhausted_dir,
        &QueueSnapshot {
            next_consumer_generation: u64::MAX,
            ..Default::default()
        },
    );
    let exhausted = open(&exhausted_dir);
    let before = bytes(&exhausted_dir);
    let error = match exhausted.register_resident().await {
        Ok(_) => panic!("allocated exhausted resident"),
        Err(error) => error,
    };
    assert!(matches!(
        error.kind,
        QueueErrorKind::AllocationExhausted {
            allocation: AllocationKind::Consumer
        }
    ));
    assert_eq!(bytes(&exhausted_dir), before);
}

#[tokio::test]
async fn previsibility_failure_restores_counter_and_visible_uncertainty_retains_dedupe() {
    for fault in [PersistFailure::BeforeWrite, PersistFailure::BeforeRename] {
        let dir = TempDir::new().unwrap();
        seed(&dir, &QueueSnapshot::default());
        let queue = open(&dir);
        let before = bytes(&dir);
        queue.test_set_persist_failure(Some(fault)).await;
        assert!(queue.enqueue(admission(WorkKey::Batch(1))).await.is_err());
        assert_eq!(bytes(&dir), before);
        assert_eq!(queue.status().await.next_event_id.get(), 0);
        queue.test_set_persist_failure(None).await;
        assert!(queue.enqueue(admission(WorkKey::Batch(1))).await.unwrap());
        assert_eq!(snapshot(&dir).entries.front().unwrap().id.get(), 0);
    }
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(matches!(
        queue
            .enqueue(admission(WorkKey::Batch(2)))
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::DurabilityUncertain(_)
    ));
    assert_eq!(snapshot(&dir).entries.len(), 1);
    assert!(matches!(
        queue
            .enqueue(admission(WorkKey::Batch(2)))
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::PriorSync(_)
    ));
    queue.test_set_persist_failure(None).await;
    assert!(!queue.enqueue(admission(WorkKey::Batch(2))).await.unwrap());
    drop(queue);
    assert_eq!(open(&dir).status().await.queued, 1);
}

#[tokio::test]
async fn opaque_stored_token_acknowledges_exactly_and_invalid_nested_id_rejects_unchanged() {
    let dir = TempDir::new().unwrap();
    let consumer = ConsumerId::from_stored("legacy-worker").unwrap();
    let mut job = entry(0, vec![WorkKey::Batch(5)]);
    job.consumer_id = Some(consumer.clone());
    job.lease = Some(Lease {
        token: DeliveryToken::from_stored("historical token Δ").unwrap(),
        consumer_id: Some(consumer.clone()),
        expires_at: now() + chrono::Duration::minutes(1),
    });
    let state = QueueSnapshot {
        next_id: 1,
        primary_consumer: Some(consumer.clone()),
        consumers: vec![ConsumerRegistration {
            id: consumer.clone(),
            label: "old worker".into(),
            kind: ConsumerKind::Pull,
            expires_at: now() + chrono::Duration::hours(1),
            ttl_seconds: 3600,
        }],
        entries: [job].into(),
        ..Default::default()
    };
    seed(&dir, &state);
    let queue = open(&dir);
    assert!(
        queue
            .ack(
                &consumer,
                &DeliveryToken::parse("historical token Δ").unwrap()
            )
            .await
            .unwrap()
    );
    drop(queue);
    seed(&dir, &state);
    let malformed = String::from_utf8(bytes(&dir)).unwrap().replace(
        "token = \"historical token Δ\"",
        "token = \" historical token Δ\"",
    );
    fs::write(root(&dir).join("workshop.toml"), malformed.as_bytes()).unwrap();
    let error = match Workshop::load(root(&dir), Arc::new(WorkshopCodec), options()) {
        Ok(_) => panic!("normalized invalid stored token"),
        Err(error) => error,
    };
    assert!(matches!(error.kind, QueueErrorKind::Codec(_)));
    assert_eq!(bytes(&dir), malformed.as_bytes());
}

#[test]
fn stale_counters_and_duplicate_entries_reject_original_disk_bytes() {
    for state in [
        QueueSnapshot {
            entries: [entry(0, vec![])].into(),
            ..Default::default()
        },
        QueueSnapshot {
            next_id: 1,
            entries: [entry(0, vec![]), entry(0, vec![])].into(),
            ..Default::default()
        },
    ] {
        let dir = TempDir::new().unwrap();
        seed(&dir, &state);
        let before = bytes(&dir);
        let error = match Workshop::load(root(&dir), Arc::new(WorkshopCodec), options()) {
            Ok(_) => panic!("accepted conflicting snapshot"),
            Err(error) => error,
        };
        assert!(matches!(error.kind, QueueErrorKind::Integrity(_)));
        assert_eq!(bytes(&dir), before);
    }
}

#[tokio::test]
async fn ordinary_admission_retains_resident_privacy_after_registration_expires() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let resident = queue.register_resident().await.unwrap();
    let endpoint = EndpointId::new("furnace outlet").unwrap();
    resident.bind(Some(endpoint.clone())).await.unwrap();
    assert!(queue.enqueue(admission(WorkKey::Batch(99))).await.unwrap());
    assert!(matches!(
        queue
            .next_event(resident.consumer_id(), Duration::ZERO, LEASE)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::UnknownConsumer
    ));
    let leased = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leased.payload, item());
    queue.test_set_now(now() + chrono::Duration::days(2));
    let pull = worker(&queue).await;
    assert!(
        queue
            .next_event(&pull, Duration::ZERO, LEASE)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(
        snapshot(&dir).entries.front().unwrap().visibility,
        EntryVisibility::ResidentOnly
    );
}

#[tokio::test]
async fn subsecond_registration_and_lease_preserve_fractional_expiry() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let registration = queue
        .register_consumer("short shift".into(), Duration::from_millis(750), true, true)
        .await
        .unwrap();
    assert_eq!(
        registration.expires_at,
        now() + chrono::Duration::milliseconds(750)
    );
    queue.enqueue(admission(WorkKey::Batch(1))).await.unwrap();
    let leased = queue
        .next_event(
            &registration.consumer_id,
            Duration::ZERO,
            Duration::from_millis(250),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        leased.lease_expires_at,
        now() + chrono::Duration::milliseconds(250)
    );
    let second_dir = TempDir::new().unwrap();
    let second = open(&second_dir);
    let consumer = worker(&second).await;
    second.enqueue(admission(WorkKey::Batch(2))).await.unwrap();
    let zero = second
        .next_event(&consumer, Duration::ZERO, Duration::ZERO)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(zero.lease_expires_at, now());
}

#[tokio::test]
async fn stored_ttl_renewal_is_bounded_and_handoff_only_touches_destination() {
    let dir = TempDir::new().unwrap();
    let source = ConsumerId::from_stored("source").unwrap();
    let destination = ConsumerId::from_stored("destination").unwrap();
    let original_expiry = now() + chrono::Duration::hours(1);
    seed(
        &dir,
        &QueueSnapshot {
            primary_consumer: Some(source.clone()),
            consumers: [source.clone(), destination.clone()]
                .into_iter()
                .map(|id| ConsumerRegistration {
                    id,
                    label: "worker".into(),
                    kind: ConsumerKind::Pull,
                    expires_at: original_expiry,
                    ttl_seconds: u64::MAX,
                })
                .collect(),
            ..Default::default()
        },
    );
    let queue = open(&dir);
    queue.test_set_now(now() + chrono::Duration::minutes(1));
    queue.handoff(&source, &destination, true).await.unwrap();
    let state = snapshot(&dir);
    assert_eq!(
        state
            .consumers
            .iter()
            .find(|c| c.id == source)
            .unwrap()
            .expires_at,
        original_expiry
    );
    assert_eq!(
        state
            .consumers
            .iter()
            .find(|c| c.id == destination)
            .unwrap()
            .expires_at,
        now() + chrono::Duration::days(1) + chrono::Duration::minutes(1)
    );
    let resident_dir = TempDir::new().unwrap();
    seed(
        &resident_dir,
        &QueueSnapshot {
            primary_consumer: Some(source.clone()),
            consumers: vec![ConsumerRegistration {
                id: source.clone(),
                label: "workshop-resident".into(),
                kind: ConsumerKind::Resident,
                expires_at: original_expiry,
                ttl_seconds: u64::MAX,
            }],
            ..Default::default()
        },
    );
    let resident_queue = open(&resident_dir);
    let resident = resident_queue.register_resident().await.unwrap();
    assert_eq!(resident.consumer_id(), &source);
    assert_eq!(snapshot(&resident_dir).consumers[0].ttl_seconds, 86_400);
    assert_eq!(
        snapshot(&resident_dir).consumers[0].expires_at,
        now() + chrono::Duration::days(1)
    );
}

#[tokio::test]
async fn checked_resident_ack_rejects_wrong_endpoint_and_expiry_without_losing_work() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let endpoint = EndpointId::new("saved session a").unwrap();
    let other = EndpointId::new("saved session b").unwrap();
    let resident = queue
        .binding_controller()
        .rebind_resident(endpoint.clone())
        .await
        .unwrap();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(901)))
        .await
        .unwrap();
    let leased = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        resident
            .ack_checked(&other, &leased.delivery_token)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::UnknownDeliveryToken
    ));
    queue.test_set_now(leased.lease_expires_at);
    assert!(matches!(
        resident
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::UnknownDeliveryToken
    ));
    assert_eq!(queue.status().await.queued, 1);
    let fresh = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_ne!(fresh.delivery_token, leased.delivery_token);
    assert!(
        resident
            .ack_checked(&endpoint, &fresh.delivery_token)
            .await
            .unwrap()
    );
    assert_eq!(queue.status().await.queued, 0);
    assert!(matches!(
        queue
            .enqueue_resident(admission(WorkKey::Reservation(901)))
            .await
            .unwrap(),
        AdmissionOutcome::Duplicate
    ));
}

#[tokio::test]
async fn resident_rebind_revokes_selected_endpoint_tokens_and_preserves_recipient() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let controller = queue.binding_controller();
    let first = EndpointId::new("session first").unwrap();
    let second = EndpointId::new("session second").unwrap();
    let resident = controller.rebind_resident(first.clone()).await.unwrap();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(902)))
        .await
        .unwrap();
    let old = resident
        .next_event(&first, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    let resident = controller.rebind_resident(second.clone()).await.unwrap();
    assert!(
        resident
            .next_event(&second, Duration::ZERO, LEASE)
            .await
            .unwrap()
            .is_none()
    );
    assert_eq!(snapshot(&dir).entries[0].endpoint, Some(first.clone()));
    assert_eq!(
        snapshot(&dir).entries[0].lease.as_ref().unwrap().token,
        old.delivery_token
    );
    let resident = controller.rebind_resident(first.clone()).await.unwrap();
    assert!(matches!(
        resident
            .ack_checked(&first, &old.delivery_token)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::UnknownDeliveryToken
    ));
    let fresh = resident
        .next_event(&first, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    let resident = controller.rebind_resident(first.clone()).await.unwrap();
    assert!(matches!(
        resident
            .ack_checked(&first, &fresh.delivery_token)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::UnknownDeliveryToken
    ));
    let newest = resident
        .next_event(&first, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert!(
        resident
            .ack_checked(&first, &newest.delivery_token)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn resident_renewal_recovers_expired_registration_only_for_selected_private_endpoint() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let controller = queue.binding_controller();
    let first = EndpointId::new("recover first").unwrap();
    let second = EndpointId::new("recover second").unwrap();
    let resident = controller.rebind_resident(first.clone()).await.unwrap();
    let original = resident.consumer_id().clone();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(903)))
        .await
        .unwrap();
    controller.rebind_resident(second.clone()).await.unwrap();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(904)))
        .await
        .unwrap();
    queue.test_set_now(now() + chrono::Duration::days(2));
    let renewed = controller.renew_resident().await.unwrap();
    assert_eq!(renewed.consumer_id(), &original);
    assert_eq!(queue.status().await.queued, 2);
    drop(renewed);
    drop(resident);
    drop(controller);
    drop(queue);

    let mut expired = snapshot(&dir);
    expired.consumers.clear();
    expired.primary_consumer = None;
    let mut public = entry(2, vec![WorkKey::Reservation(905)]);
    public.endpoint = Some(first.clone());
    expired.entries.push_back(public);
    expired.next_id = 3;
    seed(&dir, &expired);
    let reopened = open(&dir);
    let controller = reopened.binding_controller();
    let recovered = controller.rebind_resident(first.clone()).await.unwrap();
    assert_ne!(recovered.consumer_id(), &original);
    let state = snapshot(&dir);
    assert_eq!(
        state.entries[0].consumer_id.as_ref(),
        Some(recovered.consumer_id())
    );
    assert_eq!(state.entries[1].consumer_id, Some(original));
    assert_eq!(state.entries[1].endpoint, Some(second.clone()));
    assert_eq!(state.entries[2].consumer_id, None);
    assert_eq!(state.entries[2].visibility, EntryVisibility::Public);
    let leased = recovered
        .next_event(&first, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert!(
        recovered
            .ack_checked(&first, &leased.delivery_token)
            .await
            .unwrap()
    );
    let recovered = controller.rebind_resident(second.clone()).await.unwrap();
    let leased = recovered
        .next_event(&second, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert!(
        recovered
            .ack_checked(&second, &leased.delivery_token)
            .await
            .unwrap()
    );
    assert_eq!(reopened.status().await.queued, 1);
}

#[tokio::test]
async fn resident_renewal_preserves_live_lease_and_legacy_late_ack() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let controller = queue.binding_controller();
    let endpoint = EndpointId::new("renew lease").unwrap();
    let resident = controller.rebind_resident(endpoint.clone()).await.unwrap();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(906)))
        .await
        .unwrap();
    let leased = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    let renewed = controller.renew_resident().await.unwrap();
    assert!(
        renewed
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
            .unwrap()
    );
    queue
        .enqueue_resident(admission(WorkKey::Reservation(907)))
        .await
        .unwrap();
    let leased = renewed
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    queue.test_set_now(leased.lease_expires_at);
    assert!(renewed.ack(&leased.delivery_token).await.unwrap());
    assert_eq!(queue.status().await.queued, 0);
}

#[tokio::test]
async fn resident_rebind_rollback_uncertainty_and_response_loss_keep_typed_outcomes() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let controller = queue.binding_controller();
    let first = EndpointId::new("durable first").unwrap();
    let next = EndpointId::new("durable next").unwrap();
    let resident = controller.rebind_resident(first.clone()).await.unwrap();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(908)))
        .await
        .unwrap();
    let leased = resident
        .next_event(&first, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    let before = bytes(&dir);
    queue
        .test_set_persist_failure(Some(PersistFailure::BeforeRename))
        .await;
    let error = controller
        .rebind_resident(next.clone())
        .await
        .err()
        .unwrap();
    assert!(matches!(error.kind, QueueErrorKind::Io(_)));
    assert_eq!(bytes(&dir), before);
    queue.test_set_persist_failure(None).await;
    assert!(
        resident
            .ack_checked(&first, &leased.delivery_token)
            .await
            .unwrap()
    );
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    let error = controller
        .rebind_resident(next.clone())
        .await
        .err()
        .unwrap();
    assert!(matches!(error.kind, QueueErrorKind::DurabilityUncertain(_)));
    assert_eq!(snapshot(&dir).endpoint, Some(next));
    queue.test_set_persist_failure(None).await;
    let (entered, release) = queue.test_pause_owned_completion(true);
    let owned = controller.clone();
    let target = first.clone();
    let caller = tokio::spawn(async move { owned.rebind_resident(target).await });
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();
    let error = caller.await.unwrap().err().unwrap();
    assert!(matches!(error.kind, QueueErrorKind::CompletionUnknown(_)));
    assert_eq!(snapshot(&dir).endpoint, Some(first));
}

#[tokio::test]
async fn cancelled_resident_rebind_finishes_accepted_transition() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let controller = queue.binding_controller();
    let endpoint = EndpointId::new("cancelled binding").unwrap();
    let (entered, release) = queue.test_pause_owned_completion(false);
    let target = endpoint.clone();
    let caller = tokio::spawn(async move { controller.rebind_resident(target).await });
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    caller.abort();
    assert!(caller.await.err().unwrap().is_cancelled());
    release.send(()).unwrap();
    queue.drain_owned().await;
    assert_eq!(snapshot(&dir).endpoint, Some(endpoint.clone()));
    let resident = queue.binding_controller().renew_resident().await.unwrap();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(909)))
        .await
        .unwrap();
    let leased = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert!(
        resident
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
            .unwrap()
    );
}

#[tokio::test]
async fn checked_resident_ack_preserves_storage_errors_and_cancelled_completion() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let endpoint = EndpointId::new("ack storage").unwrap();
    let resident = queue
        .binding_controller()
        .rebind_resident(endpoint.clone())
        .await
        .unwrap();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(910)))
        .await
        .unwrap();
    let leased = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    queue
        .test_set_persist_failure(Some(PersistFailure::BeforeRename))
        .await;
    assert!(matches!(
        resident
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::Io(_)
    ));
    assert_eq!(queue.status().await.queued, 1);
    queue.test_set_persist_failure(None).await;
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(matches!(
        resident
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::DurabilityUncertain(_)
    ));
    assert_eq!(queue.status().await.queued, 0);
    queue.test_set_persist_failure(None).await;
    assert!(matches!(
        queue
            .enqueue_resident(admission(WorkKey::Reservation(910)))
            .await
            .unwrap(),
        AdmissionOutcome::Duplicate
    ));
    queue
        .enqueue_resident(admission(WorkKey::Reservation(911)))
        .await
        .unwrap();
    let leased = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    let (entered, release) = queue.test_pause_owned_completion(false);
    let caller = tokio::spawn(async move {
        resident
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
    });
    tokio::time::timeout(Duration::from_secs(10), entered)
        .await
        .unwrap()
        .unwrap();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    queue.drain_owned().await;
    assert_eq!(queue.status().await.queued, 0);
    assert!(matches!(
        queue
            .enqueue_resident(admission(WorkKey::Reservation(911)))
            .await
            .unwrap(),
        AdmissionOutcome::Duplicate
    ));
}

#[tokio::test]
async fn resident_recovery_does_not_take_records_from_another_live_consumer() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let endpoint = EndpointId::new("owned elsewhere").unwrap();
    let other = queue
        .register_consumer("other owner".into(), LEASE, false, false)
        .await
        .unwrap();
    let mut state = queue.test_snapshot().await;
    let mut record = entry(0, vec![WorkKey::Reservation(912)]);
    record.consumer_id = Some(other.consumer_id.clone());
    record.endpoint = Some(endpoint.clone());
    record.visibility = EntryVisibility::ResidentOnly;
    state.entries.push_back(record);
    state.next_id = 1;
    drop(queue);
    seed(&dir, &state);
    let queue = open(&dir);
    let resident = queue
        .binding_controller()
        .rebind_resident(endpoint.clone())
        .await
        .unwrap();
    assert_eq!(
        snapshot(&dir).entries[0].consumer_id,
        Some(other.consumer_id)
    );
    assert!(
        resident
            .next_event(&endpoint, Duration::ZERO, LEASE)
            .await
            .unwrap()
            .is_none()
    );
    queue
        .enqueue_resident(admission(WorkKey::Reservation(913)))
        .await
        .unwrap();
    let leased = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leased.event_id.get(), 1);
    assert!(
        resident
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
            .unwrap()
    );
    assert_eq!(queue.status().await.queued, 1);
}

#[tokio::test]
async fn checked_resident_ack_requires_both_entry_and_lease_ownership() {
    let dir = TempDir::new().unwrap();
    let queue = open(&dir);
    let endpoint = EndpointId::new("ack exact ownership").unwrap();
    let resident = queue
        .binding_controller()
        .rebind_resident(endpoint.clone())
        .await
        .unwrap();
    let other = queue
        .register_consumer("other".into(), LEASE, false, false)
        .await
        .unwrap();
    queue
        .enqueue_resident(admission(WorkKey::Reservation(914)))
        .await
        .unwrap();
    let leased = resident
        .next_event(&endpoint, Duration::ZERO, LEASE)
        .await
        .unwrap()
        .unwrap();
    queue
        .test_mutate_snapshot(|state| {
            state.entries[0].consumer_id = Some(other.consumer_id.clone());
        })
        .await;
    assert!(matches!(
        resident
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::UnknownDeliveryToken
    ));
    queue
        .test_mutate_snapshot(|state| {
            state.entries[0].consumer_id = Some(resident.consumer_id().clone());
            state.entries[0].lease.as_mut().unwrap().consumer_id = Some(other.consumer_id);
        })
        .await;
    assert!(matches!(
        resident
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
            .unwrap_err()
            .kind,
        QueueErrorKind::UnknownDeliveryToken
    ));
    queue
        .test_mutate_snapshot(|state| {
            state.entries[0].lease.as_mut().unwrap().consumer_id =
                Some(resident.consumer_id().clone());
        })
        .await;
    assert!(
        resident
            .ack_checked(&endpoint, &leased.delivery_token)
            .await
            .unwrap()
    );
}
