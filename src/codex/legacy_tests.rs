use super::{
    queue_codec::{
        LegacyPayload, LegacyQueueCodec, LegacyReplayIdentity, migrate_legacy_evidence_projection,
    },
    *,
};
use crate::{
    delivery::ProviderEventKey,
    durable::{PersistFailure, QueueCodec, QueueSnapshot},
};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::TimeDelta;
#[cfg(unix)]
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::sync::Arc;
use tempfile::TempDir;
use tokio::sync::Mutex;

const INBOX_FILE_NAME: &str = "codex-inbox.json";
const LIVE_CONSUMER_LABEL: &str = "dione-live-app-server";
const MAX_RETAINED_TEAMS_EVENT_KEYS: usize = 10_000;
const PROCESSED_TEAMS_RETENTION: Duration = Duration::from_secs(30 * 24 * 60 * 60);
fn cause_contains(error: &(dyn std::error::Error + 'static), expected: &str) -> bool {
    let mut cause = Some(error);
    while let Some(error) = cause {
        if error.to_string().contains(expected) {
            return true;
        }
        cause = error.source();
    }
    false
}
fn duration_delta(duration: Duration) -> TimeDelta {
    TimeDelta::from_std(duration).unwrap()
}
fn snapshot_value(
    queue: &CodexEventQueue,
    snapshot: &QueueSnapshot<LegacyPayload, LegacyReplayIdentity>,
) -> Value {
    serde_json::from_slice(
        &LegacyQueueCodec::new(queue.legacy_scope().clone())
            .encode(snapshot)
            .unwrap(),
    )
    .unwrap()
}
// Literal codec fixtures model an owner-only retained snapshot, not an exposed file.
fn write_private_fixture(
    path: impl AsRef<std::path::Path>,
    bytes: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).write(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    std::io::Write::write_all(&mut file, bytes.as_ref())
}

async fn write_fixture_snapshot(queue: &CodexEventQueue, path: &Utf8Path) {
    let snapshot = queue.test_snapshot().await;
    let bytes = LegacyQueueCodec::new(queue.legacy_scope().clone())
        .encode(&snapshot)
        .unwrap();
    write_private_fixture(path.join(INBOX_FILE_NAME), bytes).unwrap();
}

fn temp_path(dir: &TempDir) -> Utf8PathBuf {
    Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap()
}

fn message(id: &str, content: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": content,
            "meta": { "message_id": id, "chat_id": "123" }
        }
    })
}

fn teams_key(activity_id: &str) -> ProviderEventKey {
    ProviderEventKey::teams(
        "tenant-a".parse().unwrap(),
        "bot-a".parse().unwrap(),
        "conversation-a".parse().unwrap(),
        activity_id.parse().unwrap(),
    )
}

fn tool_result(response: Value) -> Value {
    let text = response["content"][0]["text"]
        .as_str()
        .expect("MCP tool result contains JSON text");
    serde_json::from_str(text).expect("MCP tool result text is JSON")
}

fn managed_message(id: &str, record_id: &str, delivery: &str, content: &str) -> Value {
    let mut notification = message(id, content);
    notification["params"]["meta"]["user_id"] = json!("456");
    notification["params"]["meta"]["attention_delivery"] = json!(delivery);
    notification["params"]["meta"]["attention_record"] = json!(record_id);
    notification
}

async fn primary_consumer(queue: &CodexEventQueue) -> ConsumerId {
    queue
        .register_consumer("test thread".to_owned(), DEFAULT_CONSUMER_TTL, true, true)
        .await
        .unwrap()
        .consumer_id
}

#[tokio::test]
async fn durable_payload_migrates_legacy_evidence_projection_to_citations() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = primary_consumer(&queue).await;
    let mut payload = message("1", "claim [🔍=v1:AAAAAAAAAAw]");
    payload["params"]["meta"]["evidence"] = json!([{
        "locator": "v1:AAAAAAAAAAw",
        "author_id": "300"
    }]);

    assert!(queue.enqueue(payload).await.unwrap());
    drop(queue);

    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let leased = reloaded
        .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("leased event");
    assert!(leased.event["params"]["meta"].get("evidence").is_none());
    assert_eq!(
        leased.event["params"]["meta"]["citation_locators"],
        json!([{
            "locator": "v1:AAAAAAAAAAw",
            "author_id": "300"
        }])
    );
    assert!(
        leased.event["params"]["meta"]
            .get("claim_locators")
            .is_none()
    );

    drop(reloaded);
    let persisted: Value =
        serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap()).unwrap();
    assert!(
        persisted["entries"][0]["payload"]["params"]["meta"]
            .get("evidence")
            .is_none()
    );
    assert_eq!(
        persisted["entries"][0]["payload"]["params"]["meta"]["citation_locators"],
        leased.event["params"]["meta"]["citation_locators"]
    );
}

#[test]
fn legacy_evidence_projection_merges_without_overwriting_v2_citations() {
    let mut payload = message("1", "mixed");
    payload["params"]["meta"]["citation_locators"] = json!([
        { "locator": "v2:citation:AAAAAAAAAAI", "author_id": "300" },
        { "locator": "v1:AAAAAAAAAAw", "author_id": "300" }
    ]);
    payload["params"]["meta"]["evidence"] = json!([
        { "locator": "v1:AAAAAAAAAAw", "author_id": "300" },
        { "locator": "v1:AAAAAAAAAAQ", "author_id": "300" }
    ]);

    assert!(migrate_legacy_evidence_projection(&mut payload));
    assert!(payload["params"]["meta"].get("evidence").is_none());
    assert_eq!(
        payload["params"]["meta"]["citation_locators"],
        json!([
            { "locator": "v2:citation:AAAAAAAAAAI", "author_id": "300" },
            { "locator": "v1:AAAAAAAAAAw", "author_id": "300" },
            { "locator": "v1:AAAAAAAAAAQ", "author_id": "300" }
        ])
    );
}

#[tokio::test]
async fn lease_ack_removes_event_durably() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = primary_consumer(&queue).await;
    queue.enqueue(message("1", "hello")).await.unwrap();
    let event = queue
        .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.event["params"]["content"], "hello");
    queue
        .acknowledge(&consumer, &event.delivery_token)
        .await
        .unwrap();
    assert_eq!(queue.status().await.queued, 0);
}

#[tokio::test]
async fn lease_and_ack_refresh_the_registered_consumer_ttl() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let registered_at = Utc::now();
    let ttl = Duration::from_secs(60);
    queue.test_set_now(registered_at);
    let consumer = queue
        .register_consumer("short lived".to_owned(), ttl, true, true)
        .await
        .unwrap()
        .consumer_id;
    queue.enqueue(message("1", "hello")).await.unwrap();

    let leased_at = registered_at + TimeDelta::seconds(30);
    queue.test_set_now(leased_at);
    let event = queue
        .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();
    let lease_refresh = queue
        .test_snapshot()
        .await
        .consumers
        .iter()
        .find(|registration| registration.id == consumer)
        .unwrap()
        .expires_at;
    assert_eq!(lease_refresh, leased_at + TimeDelta::seconds(60));

    let acknowledged_at = registered_at + TimeDelta::seconds(40);
    queue.test_set_now(acknowledged_at);
    queue
        .acknowledge(&consumer, &event.delivery_token)
        .await
        .unwrap();
    let ack_refresh = queue
        .test_snapshot()
        .await
        .consumers
        .iter()
        .find(|registration| registration.id == consumer)
        .unwrap()
        .expires_at;
    assert_eq!(ack_refresh, acknowledged_at + TimeDelta::seconds(60));
}

#[tokio::test]
async fn expired_lease_is_redelivered_with_new_token() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = primary_consumer(&queue).await;
    queue.enqueue(message("1", "hello")).await.unwrap();
    let first = queue
        .next_event(&consumer, Duration::ZERO, Duration::from_millis(1))
        .await
        .unwrap()
        .unwrap();
    tokio::time::sleep(Duration::from_millis(5)).await;
    let second = queue
        .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.event_id, second.event_id);
    assert_ne!(first.delivery_token, second.delivery_token);
}

#[tokio::test]
async fn duplicate_discord_message_is_not_enqueued_twice() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    primary_consumer(&queue).await;
    assert!(queue.enqueue(message("1", "first")).await.unwrap());
    assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
    assert_eq!(queue.status().await.queued, 1);
}

#[tokio::test]
async fn durable_lifecycle_events_are_not_deduplicated_as_duplicate_creates() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = primary_consumer(&queue).await;
    let mut edit = message("1", "edited");
    edit["params"]["meta"]["type"] = json!("message_edit");
    let mut delete = message("1", "deleted");
    delete["params"]["meta"]["type"] = json!("message_delete");

    assert!(queue.enqueue(message("1", "original")).await.unwrap());
    assert!(queue.enqueue(edit).await.unwrap());
    assert!(queue.enqueue(delete).await.unwrap());
    assert!(
        !queue
            .enqueue(message("1", "duplicate create"))
            .await
            .unwrap()
    );
    drop(queue);

    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let mut delivered_kinds = Vec::new();
    while let Some(event) = queue
        .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
    {
        delivered_kinds.push(
            event
                .event
                .pointer("/params/meta/type")
                .and_then(Value::as_str)
                .map(str::to_owned),
        );
        queue
            .acknowledge(&consumer, &event.delivery_token)
            .await
            .unwrap();
    }

    assert_eq!(
        delivered_kinds,
        [
            None,
            Some("message_edit".to_owned()),
            Some("message_delete".to_owned()),
        ]
    );
}

#[test]
fn second_process_owner_is_rejected() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let _first = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let second = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    );
    assert!(matches!(
        second,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::Locked(_),
                ..
            })
        })
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn legacy_private_codex_permissions_upgrade_preserves_inode_wire_and_consumption() {
    for (lock_mode, snapshot_mode) in [(0o644, 0o644), (0o600, 0o644), (0o644, 0o600)] {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let lock = path.join("codex-inbox.lock");
        let snapshot = path.join(INBOX_FILE_NAME);
        let original = serde_json::to_vec(&json!({
            "next_id": 2, "entries": [{ "id": 1, "payload": message("1", "legacy") }]
        }))
        .unwrap();
        std::fs::write(&lock, b"retained lock inode").unwrap();
        std::fs::write(&snapshot, &original).unwrap();
        std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(lock_mode)).unwrap();
        std::fs::set_permissions(&snapshot, std::fs::Permissions::from_mode(snapshot_mode))
            .unwrap();
        let before = std::fs::metadata(&lock).unwrap();
        let scope = LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        ));
        let queue = CodexEventQueue::load(&path, scope.clone()).unwrap();
        let after = std::fs::metadata(&lock).unwrap();
        assert_eq!((after.dev(), after.ino()), (before.dev(), before.ino()));
        assert_eq!(std::fs::read(&lock).unwrap(), b"retained lock inode");
        assert_eq!(std::fs::read(&snapshot).unwrap(), original);
        assert_eq!(after.permissions().mode() & 0o7777, 0o600);
        assert_eq!(
            std::fs::metadata(&snapshot).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        drop(queue);
        let reopened = CodexEventQueue::load(&path, scope.clone()).unwrap();
        assert_eq!(std::fs::read(&snapshot).unwrap(), original);
        let consumer = primary_consumer(&reopened).await;
        let delivery = reopened
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(delivery.event, message("1", "legacy"));
        reopened
            .acknowledge(&consumer, &delivery.delivery_token)
            .await
            .unwrap();
        drop(reopened);
        assert_eq!(
            CodexEventQueue::load(&path, scope)
                .unwrap()
                .status()
                .await
                .queued,
            0
        );
    }
}

#[cfg(unix)]
#[test]
fn legacy_private_codex_permissions_contention_does_not_tighten_or_replace_lock() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let lock = path.join("codex-inbox.lock");
    let snapshot = path.join(INBOX_FILE_NAME);
    std::fs::write(&lock, b"retained lock").unwrap();
    let original = br#"{"next_id":0,"entries":[]}"#;
    std::fs::write(&snapshot, original).unwrap();
    for file in [&lock, &snapshot] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let holder = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(&lock)
        .unwrap();
    fs2::FileExt::try_lock_exclusive(&holder).unwrap();
    let inode = holder.metadata().unwrap().ino();
    let scope = LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
        crate::core::ProviderId::new("discord").unwrap(),
        crate::core::ModuleSlotId::new("primary").unwrap(),
    ));
    assert!(matches!(
        CodexEventQueue::load(&path, scope.clone()),
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::Locked(_),
                ..
            })
        })
    ));
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), inode);
    assert_eq!(std::fs::read(&snapshot).unwrap(), original);
    for file in [&lock, &snapshot] {
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o7777,
            0o644
        );
    }
    drop(holder);
    assert!(CodexEventQueue::load(&path, scope).is_ok());
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), inode);
}

#[cfg(unix)]
#[test]
fn legacy_private_codex_permissions_refuses_exposed_parent_without_changes() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    let lock = path.join("codex-inbox.lock");
    let snapshot = path.join(INBOX_FILE_NAME);
    let original = br#"{"next_id":0,"entries":[]}"#;
    std::fs::write(&lock, b"exposed lock").unwrap();
    std::fs::write(&snapshot, original).unwrap();
    for file in [&lock, &snapshot] {
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    let inode = std::fs::metadata(&lock).unwrap().ino();
    let scope = LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
        crate::core::ProviderId::new("discord").unwrap(),
        crate::core::ModuleSlotId::new("primary").unwrap(),
    ));
    assert!(CodexEventQueue::load(&path, scope).is_err());
    assert_eq!(std::fs::metadata(&lock).unwrap().ino(), inode);
    assert_eq!(std::fs::read(&lock).unwrap(), b"exposed lock");
    assert_eq!(std::fs::read(&snapshot).unwrap(), original);
    for file in [&lock, &snapshot] {
        assert_eq!(
            std::fs::metadata(file).unwrap().permissions().mode() & 0o7777,
            0o644
        );
    }
}

#[tokio::test]
async fn existing_v1_inbox_is_backward_compatible() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    write_private_fixture(
        path.join(INBOX_FILE_NAME),
        serde_json::to_vec(&json!({
            "next_id": 2,
            "entries": [{ "id": 1, "payload": message("1", "legacy") }]
        }))
        .unwrap(),
    )
    .unwrap();
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let controller = queue.binding_controller();
    controller
        .bind_live_thread(Some(CodexThreadId::parse("legacy-thread").unwrap()))
        .await
        .unwrap();
    drop(controller);
    drop(queue);

    let persisted: Value =
        serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap()).unwrap();
    assert_eq!(persisted["entries"][0]["id"], 1);
    assert_eq!(persisted["entries"][0]["discord_message_id"], "1");
    assert_eq!(persisted["live_thread_id"], "legacy-thread");

    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let snapshot = reloaded.test_snapshot().await;
    assert_eq!(snapshot.entries[0].id, EventId::new(1));
    assert_eq!(
        snapshot.entries[0].replay_keys.clone(),
        vec![LegacyReplayIdentity::from_payload(&message("1", "")).unwrap()]
    );
    assert_eq!(
        snapshot.endpoint.as_ref().map(EndpointId::as_str),
        Some("legacy-thread")
    );
}

#[tokio::test]
async fn legacy_consumer_without_stored_ttl_uses_the_default() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let now = Utc::now();
    write_private_fixture(
        path.join(INBOX_FILE_NAME),
        serde_json::to_vec(&json!({
            "next_id": 1,
            "primary_consumer": "codex-consumer-0",
            "consumers": [{
                "id": "codex-consumer-0",
                "label": "legacy consumer",
                "expires_at": now + TimeDelta::hours(1)
            }],
            "entries": []
        }))
        .unwrap(),
    )
    .unwrap();

    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = ConsumerId::parse("codex-consumer-0").unwrap();
    let refreshed_at = now + TimeDelta::minutes(1);
    queue.test_set_now(refreshed_at);
    assert!(
        queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .is_none()
    );
    let snapshot = queue.test_snapshot().await;
    let registration = snapshot
        .consumers
        .iter()
        .find(|registration| registration.id == consumer)
        .unwrap();
    assert_eq!(registration.ttl_seconds, DEFAULT_CONSUMER_TTL.as_secs());
    assert_eq!(
        registration.expires_at,
        refreshed_at + duration_delta(DEFAULT_CONSUMER_TTL)
    );
}

#[tokio::test]
async fn registered_consumer_ttl_survives_restart() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let registered_at = Utc::now();
    let consumer = {
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        queue.test_set_now(registered_at);
        queue
            .register_consumer(
                "short lived".to_owned(),
                Duration::from_secs(60),
                true,
                true,
            )
            .await
            .unwrap()
            .consumer_id
    };

    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let refreshed_at = registered_at + TimeDelta::seconds(30);
    queue.test_set_now(refreshed_at);
    assert!(
        queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .is_none()
    );
    let snapshot = queue.test_snapshot().await;
    let registration = snapshot
        .consumers
        .iter()
        .find(|registration| registration.id == consumer)
        .unwrap();
    assert_eq!(registration.ttl_seconds, 60);
    assert_eq!(
        registration.expires_at,
        refreshed_at + TimeDelta::seconds(60)
    );
}

#[tokio::test]
async fn handoff_moves_future_and_pending_events_to_new_consumer() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let first = primary_consumer(&queue).await;
    queue.enqueue(message("1", "pending")).await.unwrap();
    let second = queue
        .register_consumer(
            "replacement thread".to_owned(),
            DEFAULT_CONSUMER_TTL,
            false,
            false,
        )
        .await
        .unwrap()
        .consumer_id;
    let handoff = queue.handoff(&first, &second, true).await.unwrap();
    assert_eq!(handoff.moved_pending, 1);
    assert!(
        queue
            .next_event(&first, Duration::ZERO, Duration::from_secs(60))
            .await
            .unwrap()
            .is_none()
    );
    let event = queue
        .next_event(&second, Duration::ZERO, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.event["params"]["content"], "pending");
    queue.enqueue(message("2", "future")).await.unwrap();
    queue
        .acknowledge(&second, &event.delivery_token)
        .await
        .unwrap();
    let future = queue
        .next_event(&second, Duration::ZERO, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(future.event["params"]["content"], "future");
}

#[tokio::test]
async fn acknowledged_discord_message_remains_deduplicated() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = primary_consumer(&queue).await;
    queue.enqueue(message("1", "first")).await.unwrap();
    let event = queue
        .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();
    queue
        .acknowledge(&consumer, &event.delivery_token)
        .await
        .unwrap();
    assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
}

#[tokio::test]
async fn registered_consumer_can_claim_after_primary_expires() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let first = queue
        .register_consumer(
            "short lived".to_owned(),
            Duration::from_secs(60),
            true,
            true,
        )
        .await
        .unwrap()
        .consumer_id;
    let second = queue
        .register_consumer(
            "waiting thread".to_owned(),
            Duration::from_secs(60 * 60),
            false,
            false,
        )
        .await
        .unwrap()
        .consumer_id;
    queue.enqueue(message("1", "orphaned")).await.unwrap();

    queue.test_set_now(Utc::now() + TimeDelta::minutes(2));
    let claimed = queue.claim_primary(&second, true).await.unwrap();
    assert_eq!(claimed, 1);
    assert_ne!(first, second);
}

#[tokio::test]
async fn live_consumer_is_reused_after_process_restart() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();

    let first = queue.register_live_consumer().await.unwrap();
    let first_id = first.consumer_id().clone();
    drop(first);
    drop(queue);
    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let resumed = reloaded.register_live_consumer().await.unwrap();

    assert_eq!(resumed.consumer_id(), &first_id);
    let status = reloaded.status().await;
    assert_eq!(status.primary_consumer.as_ref(), Some(&first_id));
    assert_eq!(status.consumers.len(), 1);
}

#[tokio::test]
async fn expired_live_consumer_keeps_routed_events_after_long_outage() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let first = queue.register_live_consumer().await.unwrap();
    let thread = CodexThreadId::parse("thread-outage").unwrap();
    first.bind_live_thread(Some(thread.clone())).await.unwrap();
    queue.enqueue(message("1", "still pending")).await.unwrap();
    {
        queue
            .test_mutate_snapshot(|snapshot| {
                snapshot
                    .consumers
                    .iter_mut()
                    .find(|consumer| &consumer.id == first.consumer_id())
                    .unwrap()
                    .expires_at = Utc::now() - TimeDelta::seconds(1);
            })
            .await;
        write_fixture_snapshot(&queue, &temp_path(&dir)).await;
    }
    let first_id = first.consumer_id().clone();
    drop(first);
    drop(queue);
    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let resumed = reloaded.register_live_consumer().await.unwrap();

    assert_eq!(resumed.consumer_id(), &first_id);
    assert!(
        resumed
            .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn live_rebind_does_not_replay_unleased_event_into_new_thread() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    consumer
        .bind_live_thread(Some(CodexThreadId::parse("thread-a").unwrap()))
        .await
        .unwrap();

    queue.enqueue(message("1", "for a")).await.unwrap();

    consumer
        .bind_live_thread(Some(CodexThreadId::parse("thread-b").unwrap()))
        .await
        .unwrap();
    queue.enqueue(message("2", "for b")).await.unwrap();

    let for_b = consumer
        .next_event(
            &CodexThreadId::parse("thread-b").unwrap(),
            Duration::ZERO,
            DEFAULT_LEASE,
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(for_b.event["params"]["content"], "for b");
    assert!(
        consumer
            .next_event(
                &CodexThreadId::parse("thread-a").unwrap(),
                Duration::ZERO,
                DEFAULT_LEASE,
            )
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn live_rebind_does_not_replay_leased_event_into_new_thread() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    consumer
        .bind_live_thread(Some(CodexThreadId::parse("thread-a").unwrap()))
        .await
        .unwrap();

    queue.enqueue(message("1", "leased for a")).await.unwrap();
    let _leased = consumer
        .next_event(
            &CodexThreadId::parse("thread-a").unwrap(),
            Duration::ZERO,
            DEFAULT_LEASE,
        )
        .await
        .unwrap()
        .unwrap();

    consumer
        .bind_live_thread(Some(CodexThreadId::parse("thread-b").unwrap()))
        .await
        .unwrap();

    assert!(
        consumer
            .next_event(
                &CodexThreadId::parse("thread-b").unwrap(),
                Duration::ZERO,
                DEFAULT_LEASE,
            )
            .await
            .unwrap()
            .is_none()
    );
}

async fn binding_server(
    path: &Utf8Path,
    queue: &CodexEventQueue,
) -> (
    crate::mcp::server::DioneServer,
    tokio::sync::watch::Receiver<Option<EndpointId>>,
) {
    let (notification_tx, _notification_rx) = tokio::sync::mpsc::channel(1);
    let (binding_tx, binding_rx) = tokio::sync::watch::channel(None);
    let server = crate::mcp::server::DioneServer::new(
        crate::state::new_state(),
        Arc::new(Mutex::new(crate::queue::AccessQueue::load(path))),
        Arc::new(serenity::http::Http::new("fake")),
        path.to_owned(),
        notification_tx,
        crate::tracing_channel::TraceLevelController::noop(),
        crate::harness::HarnessConnection::codex(queue.clone(), binding_tx),
        Arc::new(crate::no_rly::consent::ConsentGate::new(path)),
        Arc::new(crate::ingress_ledger::IngressLedger::new()),
        crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        ),
        None,
    )
    .await
    .expect("Discord reply registration");
    (server, binding_rx)
}

async fn dispatch_binding(
    server: &crate::mcp::server::DioneServer,
    thread_id: &str,
) -> Result<Value, String> {
    crate::mcp::dispatch::call_tool(
        server,
        "bind_codex_thread",
        json!({ "thread_id": thread_id }),
    )
    .await
}

#[tokio::test]
async fn dispatch_binding_publishes_retained_commit_on_failure() {
    for stage in [
        PersistFailure::BeforeWrite,
        PersistFailure::BeforeRename,
        PersistFailure::AfterRename,
    ] {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        let (server, binding_rx) = binding_server(&path, &queue).await;
        dispatch_binding(&server, "thread-a").await.unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        queue.test_set_persist_failure(Some(stage)).await;

        let error = dispatch_binding(&server, "thread-b").await.unwrap_err();
        let committed = stage == PersistFailure::AfterRename;
        assert_eq!(error.contains("unconfirmed crash durability"), committed);
        let expected =
            CodexThreadId::parse(if committed { "thread-b" } else { "thread-a" }).unwrap();
        assert_eq!(
            binding_rx.borrow().as_ref(),
            Some(&EndpointId::new(expected.as_str()).unwrap())
        );
        assert_eq!(
            queue.test_snapshot().await.endpoint.as_ref(),
            Some(&EndpointId::new(expected.as_str()).unwrap())
        );
        let disk = LegacyQueueCodec::new(queue.legacy_scope().clone())
            .decode(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap())
            .unwrap();
        assert_eq!(
            disk.endpoint.as_ref(),
            Some(&EndpointId::new(expected.as_str()).unwrap())
        );
        if committed {
            let error = dispatch_binding(&server, "thread-c").await.unwrap_err();
            assert!(error.contains("unconfirmed crash durability"));
            assert_eq!(
                binding_rx.borrow().as_ref(),
                Some(&EndpointId::new(expected.as_str()).unwrap())
            );
            assert_eq!(
                queue.test_snapshot().await.endpoint.as_ref(),
                Some(&EndpointId::new(expected.as_str()).unwrap())
            );
        }

        queue.test_set_persist_failure(None).await;
        queue
            .enqueue(message("1", "retained binding"))
            .await
            .unwrap();
        let watched = CodexThreadId::parse(binding_rx.borrow().as_ref().unwrap().as_str()).unwrap();
        let event = consumer
            .next_event(&watched, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("delivery without another bind or enqueue");
        assert_eq!(event.event["params"]["content"], "retained binding");
    }
}

#[tokio::test]
async fn dispatch_binding_recovery_barrier_publishes_actual_binding() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let (server, binding_rx) = binding_server(&path, &queue).await;
    dispatch_binding(&server, "thread-a").await.unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(queue.enqueue(message("1", "already queued")).await.is_err());
    let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();

    let error = dispatch_binding(&server, "thread-b").await.unwrap_err();
    assert!(error.contains("unconfirmed crash durability"));
    let expected = CodexThreadId::parse("thread-a").unwrap();
    assert_eq!(
        binding_rx.borrow().as_ref(),
        Some(&EndpointId::new(expected.as_str()).unwrap())
    );
    assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), before);
    queue.test_set_persist_failure(None).await;
    let watched = CodexThreadId::parse(binding_rx.borrow().as_ref().unwrap().as_str()).unwrap();
    let event = consumer
        .next_event(&watched, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("recovery delivers the existing event without another mutation");
    assert_eq!(event.event["params"]["content"], "already queued");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn dispatch_binding_concurrent_publication_matches_commit_order() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let (server, binding_rx) = binding_server(&path, &queue).await;
    dispatch_binding(&server, "thread-initial").await.unwrap();
    let server = Arc::new(server);
    let (reached_rx, release_tx) = queue.pause_binding_publish_for_test();

    let first_server = Arc::clone(&server);
    let first = tokio::spawn(async move { dispatch_binding(&first_server, "thread-first").await });
    tokio::time::timeout(Duration::from_secs(2), reached_rx)
        .await
        .expect("first bind reaches committed but unpublished state")
        .unwrap();
    let first_id = CodexThreadId::parse("thread-first").unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(20), queue.test_snapshot())
            .await
            .is_err()
    );
    assert_eq!(
        binding_rx.borrow().as_ref(),
        Some(&EndpointId::new("thread-initial").unwrap())
    );
    let committed = LegacyQueueCodec::new(queue.legacy_scope().clone())
        .decode(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap())
        .unwrap();
    assert_eq!(
        committed.endpoint.as_ref(),
        Some(&EndpointId::new(first_id.as_str()).unwrap())
    );

    let second_server = Arc::clone(&server);
    let mut second =
        tokio::spawn(async move { dispatch_binding(&second_server, "thread-second").await });
    assert!(
        tokio::time::timeout(Duration::from_millis(100), &mut second)
            .await
            .is_err(),
        "second bind must not complete before first watch publication"
    );
    release_tx.send(()).unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    let second_id = CodexThreadId::parse("thread-second").unwrap();
    assert_eq!(
        binding_rx.borrow().as_ref(),
        Some(&EndpointId::new(second_id.as_str()).unwrap())
    );
    let disk = LegacyQueueCodec::new(queue.legacy_scope().clone())
        .decode(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap())
        .unwrap();
    assert_eq!(
        disk.endpoint.as_ref(),
        Some(&EndpointId::new(second_id.as_str()).unwrap())
    );
}

#[tokio::test]
async fn post_rename_enqueue_keeps_memory_consistent_with_disk() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(queue.enqueue(message("1", "committed")).await.is_err());
    let persisted: Value =
        serde_json::from_slice(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap()).unwrap();
    assert_eq!(persisted["entries"].as_array().unwrap().len(), 1);
    assert_eq!(queue.status().await.queued, 1);
    queue.test_set_persist_failure(None).await;
    assert!(queue.enqueue(message("2", "later")).await.unwrap());
    drop(queue);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    assert_eq!(queue.status().await.queued, 2);
    assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
}

#[tokio::test]
async fn uncertain_lease_does_not_hide_first_event_behind_second() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = primary_consumer(&queue).await;
    queue.enqueue(message("1", "first")).await.unwrap();
    queue.enqueue(message("2", "second")).await.unwrap();

    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    let first_attempt = queue
        .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
        .await;
    queue.test_set_persist_failure(None).await;
    let first = match first_attempt {
        Ok(Some(event)) => event,
        Err(CodexQueueError {
            kind:
                CodexQueueErrorKind::Durable(QueueError {
                    kind: QueueErrorKind::DurabilityUncertain(_),
                    ..
                }),
        }) => queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .expect("first event after directory sync recovery"),
        other => panic!("unexpected first lease outcome: {other:?}"),
    };
    assert_eq!(
        first.event["params"]["content"], "first",
        "a retained uncertain lease must not let a later event pass first"
    );

    let visible = LegacyQueueCodec::new(queue.legacy_scope().clone())
        .decode(&std::fs::read(path.join(INBOX_FILE_NAME)).unwrap())
        .unwrap();
    let visible_first = visible.entries.front().unwrap();
    assert_eq!(
        visible_first.lease.as_ref().unwrap().token,
        first.delivery_token
    );
    drop(queue);
    let restarted = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    restarted
        .acknowledge(&consumer, &first.delivery_token)
        .await
        .unwrap();
    let second = restarted
        .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("second event after first is settled");
    assert_eq!(second.event["params"]["content"], "second");
}

#[tokio::test]
async fn pending_directory_sync_rejects_lease_before_mutation() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = primary_consumer(&queue).await;
    queue.enqueue(message("1", "first")).await.unwrap();
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(matches!(
        queue.enqueue(message("2", "second")).await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::DurabilityUncertain(_),
                ..
            })
        })
    ));
    let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
    let generation = queue.test_snapshot().await.next_lease_generation;
    assert!(matches!(
        queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::PriorSync(_),
                ..
            })
        })
    ));
    let snapshot = queue.test_snapshot().await;
    assert_eq!(snapshot.next_lease_generation, generation);
    assert!(snapshot.entries.iter().all(|event| event.lease.is_none()));
    drop(snapshot);
    assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), before);
    queue.test_set_persist_failure(None).await;
    let first = queue
        .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(first.event["params"]["content"], "first");
}

#[tokio::test]
async fn enqueue_persistence_failure_outcomes() {
    for stage in [
        PersistFailure::BeforeWrite,
        PersistFailure::BeforeRename,
        PersistFailure::AfterRename,
    ] {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        let consumer = primary_consumer(&queue).await;
        queue.enqueue(message("1", "existing")).await.unwrap();
        let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
        queue.test_set_persist_failure(Some(stage)).await;
        let result = queue.enqueue(message("2", "candidate")).await;
        let committed = stage == PersistFailure::AfterRename;
        if committed {
            assert!(matches!(
                result,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::DurabilityUncertain(_),
                        ..
                    })
                })
            ));
        } else {
            assert!(matches!(
                result,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::Io(_),
                        ..
                    })
                })
            ));
        }
        assert_eq!(queue.status().await.queued, if committed { 2 } else { 1 });
        let visible = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
        let snapshot = queue.test_snapshot().await;
        assert_eq!(
            snapshot_value(&queue, &snapshot),
            serde_json::from_slice::<Value>(&visible).unwrap()
        );
        assert_eq!(
            snapshot.entries.iter().any(|entry| entry
                .replay_keys
                .contains(&LegacyReplayIdentity::from_payload(&message("2", "")).unwrap())),
            committed
        );
        drop(snapshot);
        if committed {
            assert!(matches!(
                queue.enqueue(message("2", "duplicate")).await,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::PriorSync(_),
                        ..
                    })
                })
            ));
            assert!(matches!(
                queue.enqueue(message("3", "blocked")).await,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::PriorSync(_),
                        ..
                    })
                })
            ));
            assert!(matches!(
                queue
                    .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
                    .await,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::PriorSync(_),
                        ..
                    })
                })
            ));
            assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), visible);
            assert_eq!(queue.status().await.queued, 2);
        } else {
            assert_eq!(visible, before);
        }
        drop(queue);
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        assert_eq!(queue.status().await.queued, if committed { 2 } else { 1 });
        if committed {
            queue
                .test_set_persist_failure(Some(PersistFailure::AfterRename))
                .await;
            assert!(matches!(
                queue.enqueue(message("2", "duplicate after reopen")).await,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::PriorSync(_),
                        ..
                    })
                })
            ));
            queue.test_set_persist_failure(None).await;
        }
        assert_eq!(
            queue.enqueue(message("2", "retry")).await.unwrap(),
            !committed
        );
        assert!(queue.enqueue(message("3", "later")).await.unwrap());
        drop(queue);
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        assert_eq!(queue.status().await.queued, 3);
        for id in ["1", "2", "3"] {
            assert!(!queue.enqueue(message(id, "duplicate")).await.unwrap());
        }
    }
}

#[tokio::test]
async fn acknowledgement_persistence_failure_outcomes() {
    for stage in [
        PersistFailure::BeforeWrite,
        PersistFailure::BeforeRename,
        PersistFailure::AfterRename,
    ] {
        let dir = TempDir::new().unwrap();
        let path = temp_path(&dir);
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        let consumer = primary_consumer(&queue).await;
        queue.enqueue(message("1", "handled")).await.unwrap();
        let event = queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();
        let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
        queue.test_set_persist_failure(Some(stage)).await;
        let result = queue.acknowledge(&consumer, &event.delivery_token).await;
        let committed = stage == PersistFailure::AfterRename;
        if committed {
            assert!(matches!(
                result,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::DurabilityUncertain(_),
                        ..
                    })
                })
            ));
        } else {
            assert!(matches!(
                result,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::Io(_),
                        ..
                    })
                })
            ));
        }
        assert_eq!(queue.status().await.queued, if committed { 0 } else { 1 });
        let visible = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
        let snapshot = queue.test_snapshot().await;
        assert_eq!(
            snapshot_value(&queue, &snapshot),
            serde_json::from_slice::<Value>(&visible).unwrap()
        );
        assert_eq!(
            snapshot.acknowledged.iter().any(|receipt| receipt.key
                == LegacyReplayIdentity::from_payload(&message("1", "")).unwrap()),
            committed
        );
        assert_eq!(
            snapshot.entries.iter().any(|entry| entry
                .replay_keys
                .contains(&LegacyReplayIdentity::from_payload(&message("1", "")).unwrap())),
            !committed
        );
        drop(snapshot);
        if committed {
            assert!(matches!(
                queue.acknowledge(&consumer, &event.delivery_token).await,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::PriorSync(_),
                        ..
                    })
                })
            ));
            assert!(matches!(
                queue.enqueue(message("1", "duplicate")).await,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::PriorSync(_),
                        ..
                    })
                })
            ));
            assert!(matches!(
                queue.enqueue(message("2", "blocked")).await,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::PriorSync(_),
                        ..
                    })
                })
            ));
            assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), visible);
        } else {
            assert_eq!(visible, before);
        }
        drop(queue);
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        assert_eq!(queue.status().await.queued, if committed { 0 } else { 1 });
        let retry = queue.acknowledge(&consumer, &event.delivery_token).await;
        if committed {
            assert!(matches!(
                retry,
                Err(CodexQueueError {
                    kind: CodexQueueErrorKind::Durable(QueueError {
                        kind: QueueErrorKind::UnknownDeliveryToken,
                        ..
                    })
                })
            ));
        } else {
            retry.unwrap();
        }
        assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
        assert!(queue.enqueue(message("2", "later")).await.unwrap());
        drop(queue);
        let queue = CodexEventQueue::load(
            &path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        assert_eq!(queue.status().await.queued, 1);
        assert!(!queue.enqueue(message("1", "duplicate")).await.unwrap());
    }
}

#[tokio::test]
async fn live_enqueue_requires_exact_resident_and_preserves_failed_route_state() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    assert!(matches!(
        queue
            .enqueue_live(message("1", "no resident"), teams_key("1"))
            .await,
        Err(LiveQueueError {
            kind: LiveQueueErrorKind::ResidentUnavailable(_)
        })
    ));
    let consumer = queue.register_live_consumer().await.unwrap();
    assert!(matches!(
        queue
            .enqueue_live(message("2", "no thread"), teams_key("2"))
            .await,
        Err(LiveQueueError {
            kind: LiveQueueErrorKind::ResidentUnavailable(_)
        })
    ));
    assert_eq!(queue.status().await.queued, 0);

    let thread = CodexThreadId::parse("thread-a").unwrap();
    consumer
        .bind_live_thread(Some(thread.clone()))
        .await
        .unwrap();
    assert!(matches!(
        queue
            .enqueue_live(message("3", "routed"), teams_key("3"))
            .await,
        Ok(LiveAdmissionOutcome::Committed)
    ));
    let event = consumer
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("exact resident event");
    assert_eq!(event.event["params"]["content"], "routed");

    {
        queue
            .test_mutate_snapshot(|snapshot| {
                snapshot.consumers[0].expires_at = Utc::now() - TimeDelta::seconds(1);
            })
            .await;
    }
    assert!(matches!(
        queue
            .enqueue_live(message("4", "expired route"), teams_key("4"))
            .await,
        Err(LiveQueueError {
            kind: LiveQueueErrorKind::ResidentUnavailable(_)
        })
    ));
    let snapshot = queue.test_snapshot().await;
    assert_eq!(
        snapshot.primary_consumer.as_ref(),
        Some(consumer.consumer_id())
    );
    assert_eq!(snapshot.consumers.len(), 1);
}

#[tokio::test]
async fn dispatch_forged_live_label_cannot_admit_or_receive_teams_event() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let (server, binding_rx) = binding_server(&path, &queue).await;
    dispatch_binding(&server, "thread-a").await.unwrap();
    let forged = crate::mcp::dispatch::call_tool(
        &server,
        "register_event_consumer",
        json!({
            "label": LIVE_CONSUMER_LABEL,
            "make_primary": true,
            "claim_unassigned": true
        }),
    )
    .await
    .unwrap_err();
    assert_eq!(forged, "invalid consumer id");
    assert!(queue.status().await.primary_consumer.is_none());

    let resident = queue.register_live_consumer().await.unwrap();
    assert!(matches!(
        queue
            .enqueue_live(message("1", "for resident"), teams_key("activity-1"))
            .await,
        Ok(LiveAdmissionOutcome::Committed)
    ));
    let thread = CodexThreadId::parse(binding_rx.borrow().as_ref().unwrap().as_str()).unwrap();
    let event = resident
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("only dedicated live consumer receives Teams event");
    assert_eq!(event.event["params"]["content"], "for resident");
}

#[tokio::test]
async fn dispatch_public_poll_cannot_lease_status_disclosed_live_consumer() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let (server, binding_rx) = binding_server(&path, &queue).await;
    dispatch_binding(&server, "thread-a").await.unwrap();
    let resident = queue.register_live_consumer().await.unwrap();
    queue
        .enqueue_live(
            message("1", "private Teams activity"),
            teams_key("activity-1"),
        )
        .await
        .unwrap();

    let status = crate::mcp::dispatch::call_tool(&server, "event_queue_status", json!({}))
        .await
        .unwrap();
    let status = tool_result(status);
    let disclosed = status["primary_consumer"].as_str().unwrap();
    assert_eq!(disclosed, resident.consumer_id().as_str());
    let public_poll = crate::mcp::dispatch::call_tool(
        &server,
        "next_event",
        json!({ "consumer_id": disclosed, "wait_seconds": 0 }),
    )
    .await;
    assert!(
        public_poll.is_err(),
        "public poll leased live event: {public_poll:?}"
    );

    let thread = CodexThreadId::parse(binding_rx.borrow().as_ref().unwrap().as_str()).unwrap();
    let event = resident
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("resident still receives private activity");
    assert_eq!(event.event["params"]["content"], "private Teams activity");
}

#[tokio::test]
async fn dispatch_public_ack_cannot_remove_resident_lease() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let (server, binding_rx) = binding_server(&path, &queue).await;
    dispatch_binding(&server, "thread-a").await.unwrap();
    let resident = queue.register_live_consumer().await.unwrap();
    queue
        .enqueue_live(
            message("1", "private Teams activity"),
            teams_key("activity-1"),
        )
        .await
        .unwrap();
    let thread = CodexThreadId::parse(binding_rx.borrow().as_ref().unwrap().as_str()).unwrap();
    let leased = resident
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();

    let public_ack = crate::mcp::dispatch::call_tool(
        &server,
        "ack_event",
        json!({
            "consumer_id": resident.consumer_id().as_str(),
            "delivery_token": leased.delivery_token.as_str()
        }),
    )
    .await;
    assert!(
        public_ack.is_err(),
        "public ack removed resident lease: {public_ack:?}"
    );
    assert_eq!(queue.status().await.queued, 1);
    assert_eq!(queue.status().await.leased, 1);
}

#[tokio::test]
async fn dispatch_public_handoff_cannot_move_live_primary_or_pending_activity() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let (server, binding_rx) = binding_server(&path, &queue).await;
    dispatch_binding(&server, "thread-a").await.unwrap();
    let resident = queue.register_live_consumer().await.unwrap();
    queue
        .enqueue_live(
            message("1", "private Teams activity"),
            teams_key("activity-1"),
        )
        .await
        .unwrap();
    let pull = crate::mcp::dispatch::call_tool(
        &server,
        "register_event_consumer",
        json!({ "label": "forged pull" }),
    )
    .await
    .unwrap();
    let pull = tool_result(pull);
    let pull_id = pull["consumer_id"].as_str().unwrap();

    let handoff = crate::mcp::dispatch::call_tool(
        &server,
        "handoff_event_consumer",
        json!({
            "from_consumer_id": resident.consumer_id().as_str(),
            "to_consumer_id": pull_id,
            "move_pending": true
        }),
    )
    .await;
    assert!(
        handoff.is_err(),
        "public handoff moved live route: {handoff:?}"
    );
    assert_eq!(
        queue.status().await.primary_consumer.as_ref(),
        Some(resident.consumer_id())
    );

    let thread = CodexThreadId::parse(binding_rx.borrow().as_ref().unwrap().as_str()).unwrap();
    let event = resident
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("live event remains with resident");
    assert_eq!(event.event["params"]["content"], "private Teams activity");
}

#[tokio::test]
async fn dispatch_public_claim_cannot_take_expired_live_activity_after_reload() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let (server, _binding_rx) = binding_server(&path, &queue).await;
    dispatch_binding(&server, "thread-a").await.unwrap();
    queue.register_live_consumer().await.unwrap();
    queue
        .enqueue_live(
            message("1", "private Teams activity"),
            teams_key("activity-1"),
        )
        .await
        .unwrap();
    {
        queue
            .test_mutate_snapshot(|snapshot| {
                snapshot.consumers[0].expires_at = Utc::now() - TimeDelta::seconds(1);
            })
            .await;
        write_fixture_snapshot(&queue, &temp_path(&dir)).await;
    }
    drop(server);
    drop(queue);

    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let (server, _) = binding_server(&path, &reloaded).await;
    let pull = crate::mcp::dispatch::call_tool(
        &server,
        "register_event_consumer",
        json!({ "label": "ordinary pull" }),
    )
    .await
    .unwrap();
    let pull = tool_result(pull);
    let pull_id = pull["consumer_id"].as_str().unwrap();
    let claimed = crate::mcp::dispatch::call_tool(
        &server,
        "claim_event_consumer",
        json!({ "consumer_id": pull_id, "claim_orphaned": true }),
    )
    .await
    .unwrap();
    let claimed = tool_result(claimed);
    assert_eq!(claimed["claimed_events"], 0);
    let poll = crate::mcp::dispatch::call_tool(
        &server,
        "next_event",
        json!({ "consumer_id": pull_id, "wait_seconds": 0 }),
    )
    .await
    .unwrap();
    let poll = tool_result(poll);
    assert!(poll["event"].is_null());
    assert_eq!(reloaded.status().await.queued, 1);
}

#[tokio::test]
async fn dispatch_public_registration_cannot_claim_unassigned_live_activity() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let (server, _binding_rx) = binding_server(&path, &queue).await;
    dispatch_binding(&server, "thread-a").await.unwrap();
    queue.register_live_consumer().await.unwrap();
    queue
        .enqueue_live(
            message("1", "private Teams activity"),
            teams_key("activity-1"),
        )
        .await
        .unwrap();
    {
        queue
            .test_mutate_snapshot(|snapshot| {
                snapshot.primary_consumer = None;
                snapshot.consumers.clear();
                snapshot.entries[0].consumer_id = None;
            })
            .await;
        write_fixture_snapshot(&queue, &temp_path(&dir)).await;
    }

    let pull = crate::mcp::dispatch::call_tool(
        &server,
        "register_event_consumer",
        json!({
            "label": "ordinary pull",
            "make_primary": true,
            "claim_unassigned": true
        }),
    )
    .await
    .unwrap();
    let pull = tool_result(pull);
    let pull_id = pull["consumer_id"].as_str().unwrap();
    let poll = crate::mcp::dispatch::call_tool(
        &server,
        "next_event",
        json!({ "consumer_id": pull_id, "wait_seconds": 0 }),
    )
    .await
    .unwrap();
    let poll = tool_result(poll);
    assert!(poll["event"].is_null());
    assert_eq!(queue.status().await.queued, 1);
    assert_eq!(queue.status().await.unassigned, 1);
}

#[tokio::test]
async fn public_live_consumer_guard_waits_for_prior_sync_barrier() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let resident = queue.register_live_consumer().await.unwrap();
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(matches!(
        queue.enqueue(message("1", "committed before guard")).await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::DurabilityUncertain(_),
                ..
            })
        })
    ));
    assert!(matches!(
        queue
            .next_event(resident.consumer_id(), Duration::ZERO, DEFAULT_LEASE)
            .await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::PriorSync(_),
                ..
            })
        })
    ));
    queue.test_set_persist_failure(None).await;
    assert!(matches!(
        queue
            .next_event(resident.consumer_id(), Duration::ZERO, DEFAULT_LEASE)
            .await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::UnknownConsumer,
                ..
            })
        })
    ));
    assert_eq!(queue.status().await.queued, 1);
}

#[tokio::test]
async fn legacy_same_label_pull_record_never_becomes_live_after_reload_or_expiry() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let thread = CodexThreadId::parse("thread-a").unwrap();
    let controller = queue.binding_controller();
    controller
        .bind_live_thread(Some(thread.clone()))
        .await
        .unwrap();
    drop(controller);
    drop(queue);

    // A pre-kind inbox could persist a public pull registration with the
    // live label. Missing kind must not upgrade that record to a resident.
    let inbox_path = path.join(INBOX_FILE_NAME);
    let mut disk: Value = serde_json::from_slice(&std::fs::read(&inbox_path).unwrap()).unwrap();
    disk["next_consumer_generation"] = json!(1);
    disk["primary_consumer"] = json!("codex-consumer-0");
    disk["consumers"] = json!([{
        "id": "codex-consumer-0",
        "label": LIVE_CONSUMER_LABEL,
        "expires_at": Utc::now() + TimeDelta::hours(1),
        "ttl_seconds": 3600
    }]);
    write_private_fixture(&inbox_path, serde_json::to_vec_pretty(&disk).unwrap()).unwrap();

    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    assert!(matches!(
        reloaded
            .enqueue_live(message("1", "must reject"), teams_key("activity-1"))
            .await,
        Err(LiveQueueError {
            kind: LiveQueueErrorKind::ResidentUnavailable(_)
        })
    ));
    assert!(matches!(
        reloaded.register_live_consumer().await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::PrimaryConsumerExists,
                ..
            })
        })
    ));
    {
        reloaded
            .test_mutate_snapshot(|snapshot| {
                snapshot.consumers[0].expires_at = Utc::now() - TimeDelta::seconds(1);
            })
            .await;
        write_fixture_snapshot(&reloaded, &temp_path(&dir)).await;
    }
    drop(reloaded);

    let recovered = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let resident = recovered.register_live_consumer().await.unwrap();
    assert_ne!(
        resident.consumer_id(),
        &ConsumerId::parse("codex-consumer-0").unwrap()
    );
    assert!(matches!(
        recovered
            .enqueue_live(message("2", "for real resident"), teams_key("activity-2"))
            .await,
        Ok(LiveAdmissionOutcome::Committed)
    ));
    let event = resident
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("recovered resident receives only fresh event");
    assert_eq!(event.event["params"]["content"], "for real resident");
}

#[tokio::test]
async fn reserved_live_label_rejection_waits_for_prior_sync_barrier() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    queue.register_live_consumer().await.unwrap();
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(matches!(
        queue.enqueue(message("1", "already committed")).await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::DurabilityUncertain(_),
                ..
            })
        })
    ));
    assert!(matches!(
        queue
            .register_consumer(
                LIVE_CONSUMER_LABEL.to_owned(),
                DEFAULT_CONSUMER_TTL,
                true,
                true,
            )
            .await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::PriorSync(_),
                ..
            })
        })
    ));
    queue.test_set_persist_failure(None).await;
    assert!(matches!(
        queue
            .register_consumer(
                format!(" {LIVE_CONSUMER_LABEL} "),
                DEFAULT_CONSUMER_TTL,
                true,
                true,
            )
            .await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::Identity(IdentityError {
                    kind: IdentityErrorKind::Consumer
                }),
                ..
            })
        })
    ));
    assert_eq!(queue.status().await.consumers.len(), 1);
}

#[tokio::test]
async fn live_enqueue_distinguishes_precommit_from_visible_uncertainty_and_wakes() {
    for stage in [
        PersistFailure::BeforeWrite,
        PersistFailure::BeforeRename,
        PersistFailure::AfterRename,
    ] {
        let dir = TempDir::new().unwrap();
        let queue = CodexEventQueue::load(
            &temp_path(&dir),
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .unwrap();
        let consumer = queue.register_live_consumer().await.unwrap();
        let thread = CodexThreadId::parse("thread-a").unwrap();
        consumer
            .bind_live_thread(Some(thread.clone()))
            .await
            .unwrap();
        queue.test_set_persist_failure(Some(stage)).await;
        let changed = queue.test_changed();
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let result = queue
            .enqueue_live(message("1", "for resident"), teams_key("1"))
            .await;
        let committed = stage == PersistFailure::AfterRename;
        if committed {
            assert!(matches!(
                result,
                Ok(LiveAdmissionOutcome::CommittedDurabilityUncertain(
                    CodexQueueError {
                        kind: CodexQueueErrorKind::Durable(QueueError {
                            kind: QueueErrorKind::DurabilityUncertain(_),
                            ..
                        })
                    }
                ))
            ));
        } else {
            assert!(matches!(
                result,
                Err(LiveQueueError {
                    kind: LiveQueueErrorKind::Queue(CodexQueueError {
                        kind: CodexQueueErrorKind::Durable(QueueError {
                            kind: QueueErrorKind::Io(_),
                            ..
                        })
                    })
                })
            ));
        }
        assert_eq!(queue.status().await.queued, usize::from(committed));
        if committed {
            tokio::time::timeout(Duration::from_millis(100), notified)
                .await
                .expect("committed event wakes resident");
            assert!(matches!(
                queue
                    .enqueue_live(message("1", "replay while unsynced"), teams_key("1"))
                    .await,
                Err(LiveQueueError {
                    kind: LiveQueueErrorKind::Queue(CodexQueueError {
                        kind: CodexQueueErrorKind::Durable(QueueError {
                            kind: QueueErrorKind::PriorSync(_),
                            ..
                        })
                    })
                })
            ));
        } else {
            assert!(
                tokio::time::timeout(Duration::from_millis(20), notified)
                    .await
                    .is_err()
            );
        }
        queue.test_set_persist_failure(None).await;
        if committed {
            assert!(matches!(
                queue
                    .enqueue_live(message("1", "replay after recovery"), teams_key("1"))
                    .await,
                Ok(LiveAdmissionOutcome::Duplicate)
            ));
        } else {
            assert!(matches!(
                queue
                    .enqueue_live(message("1", "retry after rollback"), teams_key("1"))
                    .await,
                Ok(LiveAdmissionOutcome::Committed)
            ));
        }
        let event = consumer
            .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap();
        assert!(event.is_some());
    }
}

#[tokio::test]
async fn live_enqueue_prior_sync_barrier_rejects_this_event_before_mutation() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    let thread = CodexThreadId::parse("thread-a").unwrap();
    consumer
        .bind_live_thread(Some(thread.clone()))
        .await
        .unwrap();
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(matches!(
        queue.enqueue(message("1", "prior event")).await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::DurabilityUncertain(_),
                ..
            })
        })
    ));
    let before = std::fs::read(temp_path(&dir).join(INBOX_FILE_NAME)).unwrap();
    // Even an unavailable route must not bypass an older sync barrier.
    queue
        .test_mutate_snapshot(|state| state.endpoint = None)
        .await;
    assert!(matches!(
        queue
            .enqueue_live(message("2", "must not insert"), teams_key("2"))
            .await,
        Err(LiveQueueError {
            kind: LiveQueueErrorKind::Queue(CodexQueueError {
                kind: CodexQueueErrorKind::Durable(QueueError {
                    kind: QueueErrorKind::PriorSync(_),
                    ..
                })
            })
        })
    ));
    assert_eq!(
        std::fs::read(temp_path(&dir).join(INBOX_FILE_NAME)).unwrap(),
        before
    );
    queue
        .test_mutate_snapshot(|state| {
            state.endpoint = Some(EndpointId::new(thread.as_str()).unwrap())
        })
        .await;
    queue.test_set_persist_failure(None).await;
    let event = consumer
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("prior event remains deliverable after recovery");
    assert_eq!(event.event["params"]["content"], "prior event");
    let status = queue.status().await;
    assert_eq!(status.queued, 1);
    assert_eq!(status.leased, 1);
}

#[tokio::test]
async fn live_enqueue_reloaded_inbox_clears_barrier_before_admission() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    let thread = CodexThreadId::parse("thread-a").unwrap();
    consumer
        .bind_live_thread(Some(thread.clone()))
        .await
        .unwrap();
    drop(consumer);
    drop(queue);

    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    reloaded
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    let before = std::fs::read(path.join(INBOX_FILE_NAME)).unwrap();
    assert!(matches!(
        reloaded
            .enqueue_live(message("1", "not inserted"), teams_key("1"))
            .await,
        Err(LiveQueueError {
            kind: LiveQueueErrorKind::Queue(CodexQueueError {
                kind: CodexQueueErrorKind::Durable(QueueError {
                    kind: QueueErrorKind::PriorSync(_),
                    ..
                })
            })
        })
    ));
    assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), before);
    assert_eq!(reloaded.status().await.queued, 0);

    reloaded.test_set_persist_failure(None).await;
    assert!(matches!(
        reloaded
            .enqueue_live(message("2", "after recovery"), teams_key("2"))
            .await,
        Ok(LiveAdmissionOutcome::Committed)
    ));
    let consumer = reloaded.register_live_consumer().await.unwrap();
    let event = consumer
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .expect("recovered exact route");
    assert_eq!(event.event["params"]["content"], "after recovery");
}

#[tokio::test]
async fn live_enqueue_suppresses_queued_and_acknowledged_replays_across_restart() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    let thread = CodexThreadId::parse("thread-a").unwrap();
    consumer
        .bind_live_thread(Some(thread.clone()))
        .await
        .unwrap();
    let key = teams_key("activity/a");
    assert!(matches!(
        queue
            .enqueue_live(message("1", "original"), key.clone())
            .await,
        Ok(LiveAdmissionOutcome::Committed)
    ));
    {
        let changed = queue.test_changed();
        let notified = changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        assert!(matches!(
            queue
                .enqueue_live(message("1", "replayed with new handle"), key.clone())
                .await,
            Ok(LiveAdmissionOutcome::Duplicate)
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(20), notified)
                .await
                .is_err()
        );
    }
    assert_eq!(queue.status().await.queued, 1);
    drop(consumer);
    drop(queue);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    assert!(matches!(
        queue
            .enqueue_live(message("1", "replayed pending after restart"), key.clone())
            .await,
        Ok(LiveAdmissionOutcome::Duplicate)
    ));
    let leased = consumer
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leased.event["params"]["content"], "original");
    consumer.acknowledge(&leased.delivery_token).await.unwrap();
    assert!(matches!(
        queue
            .enqueue_live(message("1", "after ack"), key.clone())
            .await,
        Ok(LiveAdmissionOutcome::Duplicate)
    ));
    drop(consumer);
    drop(queue);

    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    assert!(matches!(
        reloaded
            .enqueue_live(message("1", "after restart"), key.clone())
            .await,
        Ok(LiveAdmissionOutcome::Duplicate)
    ));
    assert_eq!(reloaded.status().await.queued, 0);
    for (tenant, bot, conversation, activity) in [
        ("tenant-b", "bot-a", "conversation-a", "activity/a"),
        ("tenant-a", "bot-b", "conversation-a", "activity/a"),
        ("tenant-a", "bot-a", "conversation-b", "activity/a"),
        ("tenant-a", "bot-a", "conversation-a", "activity/b"),
    ] {
        let scoped = ProviderEventKey::teams(
            tenant.parse().unwrap(),
            bot.parse().unwrap(),
            conversation.parse().unwrap(),
            activity.parse().unwrap(),
        );
        assert!(matches!(
            reloaded
                .enqueue_live(message("1", "other scope"), scoped.clone())
                .await,
            Ok(LiveAdmissionOutcome::Committed)
        ));
        assert!(matches!(
            reloaded
                .enqueue_live(message("1", "same scope"), scoped)
                .await,
            Ok(LiveAdmissionOutcome::Duplicate)
        ));
    }
    assert_eq!(reloaded.status().await.queued, 4);
}

#[tokio::test]
async fn literal_teams_inbox_preserves_scoped_keys_and_prior_wire_shape() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let bytes = include_bytes!("../../tests/fixtures/teams/codex-inbox-scoped-identities.json");
    write_private_fixture(path.join(INBOX_FILE_NAME), bytes).unwrap();
    let queue = CodexEventQueue::load(
        &path,
        LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let snapshot = queue.test_snapshot().await;
    assert_eq!(
        snapshot_value(&queue, &snapshot),
        serde_json::from_slice::<Value>(bytes).unwrap()
    );
    let pending = ProviderEventKey::teams(
        " Tenant/A ".parse().unwrap(),
        "Bot:A".parse().unwrap(),
        "Room/A".parse().unwrap(),
        "Activity/A".parse().unwrap(),
    );
    let processed = ProviderEventKey::teams(
        " Tenant/A ".parse().unwrap(),
        "Bot:A".parse().unwrap(),
        "Room/A".parse().unwrap(),
        "Already/A".parse().unwrap(),
    );
    assert!(snapshot.entries.iter().any(|entry| {
        entry
            .replay_keys
            .contains(&LegacyReplayIdentity::Provider(pending.clone()))
    }));
    assert!(
        snapshot
            .acknowledged
            .iter()
            .any(|receipt| receipt.key == LegacyReplayIdentity::Provider(processed.clone()))
    );
    assert_eq!(std::fs::read(path.join(INBOX_FILE_NAME)).unwrap(), bytes);
}

#[tokio::test]
async fn inbox_reload_rejects_invalid_provider_event_key_without_rewriting_state() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let resident = queue.register_live_consumer().await.unwrap();
    resident
        .bind_live_thread(Some(CodexThreadId::parse("thread-a").unwrap()))
        .await
        .unwrap();
    queue
        .enqueue_live(message("1", "original"), teams_key("activity-a"))
        .await
        .unwrap();
    drop(resident);
    drop(queue);

    let inbox_path = path.join(INBOX_FILE_NAME);
    let mut disk: Value = serde_json::from_slice(&std::fs::read(&inbox_path).unwrap()).unwrap();
    disk["entries"][0]["provider_event_key"]["id"]["tenant_id"] = json!("");
    let bytes = serde_json::to_vec_pretty(&disk).unwrap();
    write_private_fixture(&inbox_path, &bytes).unwrap();
    assert!(matches!(
        CodexEventQueue::load(&path, crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(crate::core::ProviderId::new("discord").unwrap(), crate::core::ModuleSlotId::new("primary").unwrap()))),
        Err(CodexQueueError {kind: CodexQueueErrorKind::Durable(QueueError {kind: QueueErrorKind::Codec(source), ..})}) if cause_contains(&source, "invalid Teams")
    ));
    assert_eq!(std::fs::read(&inbox_path).unwrap(), bytes);
}

#[tokio::test]
async fn live_enqueue_replay_key_validates_identity_and_expires_after_ack_window() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    let thread = CodexThreadId::parse("thread-a").unwrap();
    consumer
        .bind_live_thread(Some(thread.clone()))
        .await
        .unwrap();
    let key = teams_key("activity/a");
    queue
        .enqueue_live(message("1", "original"), key.clone())
        .await
        .unwrap();
    let leased = consumer
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();
    consumer.acknowledge(&leased.delivery_token).await.unwrap();
    {
        queue
            .test_mutate_snapshot(|snapshot| {
                let expired =
                    Utc::now() - duration_delta(PROCESSED_TEAMS_RETENTION) - TimeDelta::seconds(1);
                snapshot.acknowledged[0].acknowledged_at = expired;
            })
            .await;
        write_fixture_snapshot(&queue, &temp_path(&dir)).await;
    }
    drop(consumer);
    drop(queue);

    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    assert!(matches!(
        reloaded
            .enqueue_live(message("1", "fresh after expiry"), key.clone())
            .await,
        Ok(LiveAdmissionOutcome::Committed)
    ));
    let snapshot = reloaded.test_snapshot().await;
    assert!(snapshot.acknowledged.is_empty());
    assert_eq!(
        snapshot
            .entries
            .iter()
            .flat_map(|entry| &entry.replay_keys)
            .filter(|key| matches!(key, LegacyReplayIdentity::Provider(_)))
            .count(),
        1
    );
}

#[tokio::test]
async fn live_enqueue_acknowledgement_uncertainty_retains_processed_replay_key() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    let thread = CodexThreadId::parse("thread-a").unwrap();
    consumer
        .bind_live_thread(Some(thread.clone()))
        .await
        .unwrap();
    let key = teams_key("activity/a");
    queue
        .enqueue_live(message("1", "original"), key.clone())
        .await
        .unwrap();
    let leased = consumer
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();
    queue
        .test_set_persist_failure(Some(PersistFailure::AfterRename))
        .await;
    assert!(matches!(
        consumer.acknowledge(&leased.delivery_token).await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::DurabilityUncertain(_),
                ..
            })
        })
    ));
    assert_eq!(queue.status().await.queued, 0);
    assert!(matches!(
        queue
            .enqueue_live(message("1", "before sync"), key.clone())
            .await,
        Err(LiveQueueError {
            kind: LiveQueueErrorKind::Queue(CodexQueueError {
                kind: CodexQueueErrorKind::Durable(QueueError {
                    kind: QueueErrorKind::PriorSync(_),
                    ..
                })
            })
        })
    ));
    queue.test_set_persist_failure(None).await;
    assert!(matches!(
        queue
            .enqueue_live(message("1", "after sync"), key.clone())
            .await,
        Ok(LiveAdmissionOutcome::Duplicate)
    ));
    drop(consumer);
    drop(queue);
    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    assert!(matches!(
        reloaded
            .enqueue_live(message("1", "after reload"), key)
            .await,
        Ok(LiveAdmissionOutcome::Duplicate)
    ));
}

#[tokio::test]
async fn live_enqueue_replay_capacity_fails_closed_but_allows_duplicates() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let resident = queue.register_live_consumer().await.unwrap();
    resident
        .bind_live_thread(Some(CodexThreadId::parse("thread-a").unwrap()))
        .await
        .unwrap();
    queue
        .test_mutate_snapshot(|snapshot| {
            let now = Utc::now();
            for index in 0..MAX_RETAINED_TEAMS_EVENT_KEYS {
                snapshot
                    .acknowledged
                    .push_back(crate::durable::state::ReplayReceipt {
                        key: LegacyReplayIdentity::Provider(teams_key(&format!(
                            "activity-{index}"
                        ))),
                        acknowledged_at: now,
                    });
            }
        })
        .await;
    write_fixture_snapshot(&queue, &temp_path(&dir)).await;
    assert!(matches!(
        queue
            .enqueue_live(message("1", "duplicate"), teams_key("activity-0"))
            .await,
        Ok(LiveAdmissionOutcome::Duplicate)
    ));
    let before = std::fs::read(temp_path(&dir).join(INBOX_FILE_NAME)).unwrap();
    assert!(matches!(
        queue
            .enqueue_live(message("2", "new"), teams_key("activity-new"))
            .await,
        Err(LiveQueueError {
            kind: LiveQueueErrorKind::ReplayCapacity(_)
        })
    ));
    assert_eq!(
        std::fs::read(temp_path(&dir).join(INBOX_FILE_NAME)).unwrap(),
        before
    );
    assert_eq!(queue.status().await.queued, 0);
    {
        queue
            .test_mutate_snapshot(|snapshot| {
                let expired =
                    Utc::now() - duration_delta(PROCESSED_TEAMS_RETENTION) - TimeDelta::seconds(1);
                snapshot.acknowledged[0].acknowledged_at = expired;
            })
            .await;
        write_fixture_snapshot(&queue, &temp_path(&dir)).await;
    }
    let path = temp_path(&dir);
    drop(resident);
    drop(queue);
    let reloaded = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    assert!(matches!(
        reloaded
            .enqueue_live(message("2", "after expiry"), teams_key("activity-new"))
            .await,
        Ok(LiveAdmissionOutcome::Committed)
    ));
    let snapshot = reloaded.test_snapshot().await;
    assert_eq!(
        snapshot
            .entries
            .iter()
            .flat_map(|entry| &entry.replay_keys)
            .filter(|key| matches!(key, LegacyReplayIdentity::Provider(_)))
            .count(),
        1
    );
    assert_eq!(
        snapshot
            .acknowledged
            .iter()
            .filter(|receipt| matches!(receipt.key, LegacyReplayIdentity::Provider(_)))
            .count(),
        MAX_RETAINED_TEAMS_EVENT_KEYS - 1
    );
}

#[tokio::test]
async fn failed_ack_persistence_rolls_back_in_memory_state() {
    let dir = TempDir::new().unwrap();
    let path = temp_path(&dir);
    let queue = CodexEventQueue::load(
        &path,
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = primary_consumer(&queue).await;
    queue.enqueue(message("1", "keep me")).await.unwrap();
    let event = queue
        .next_event(&consumer, Duration::ZERO, Duration::from_secs(60))
        .await
        .unwrap()
        .unwrap();

    queue
        .test_set_persist_failure(Some(PersistFailure::BeforeRename))
        .await;
    assert!(
        queue
            .acknowledge(&consumer, &event.delivery_token)
            .await
            .is_err()
    );
    assert_eq!(queue.status().await.queued, 1);

    queue.test_set_persist_failure(None).await;
    queue
        .acknowledge(&consumer, &event.delivery_token)
        .await
        .unwrap();
    assert_eq!(queue.status().await.queued, 0);
}

#[tokio::test]
async fn durable_inbox_rejects_trailing_input_without_rewriting_bytes() {
    let dir = TempDir::new().unwrap();
    let root = temp_path(&dir);
    let path = root.join(INBOX_FILE_NAME);
    let valid = br#"{"next_id":2,"entries":[{"id":1,"payload":{"params":{"content":"literal ordinary"}}}]}"#;
    let mut invalid = valid.to_vec();
    invalid.extend_from_slice(b" {} ");
    write_private_fixture(&path, &invalid).unwrap();
    let scope = LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
        crate::core::ProviderId::new("discord").unwrap(),
        crate::core::ModuleSlotId::new("primary").unwrap(),
    ));
    assert!(matches!(
        CodexEventQueue::load(&root, scope.clone()),
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::Codec(_),
                ..
            })
        })
    ));
    assert_eq!(std::fs::read(&path).unwrap(), invalid);
    write_private_fixture(&path, valid).unwrap();
    let queue = CodexEventQueue::load(&root, scope).unwrap();
    let consumer = primary_consumer(&queue).await;
    let event = queue
        .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        event.event,
        json!({"params":{"content":"literal ordinary"}})
    );
}

#[tokio::test]
async fn literal_legacy_attention_inbox_preserves_unknown_fields_and_citations() {
    let dir = TempDir::new().unwrap();
    let root = temp_path(&dir);
    let path = root.join(INBOX_FILE_NAME);
    let original = br#"{"next_id":4,"entries":[
        {"id":1,"payload":{"params":{"content":"prompt text\n","meta":{"chat_id":"123","message_id":"101","user_id":"456","attention_record":"record-a","attention_delivery":"prompt","unknown":{"keep":true},"evidence":[{"locator":"literal"}]}}}},
        {"id":2,"payload":{"params":{"content":"next text","meta":{"chat_id":"123","message_id":"102","user_id":"456","attention_record":"record-b","attention_delivery":"next_turn"}}}},
        {"id":3,"payload":{"params":{"content":"ordinary edit","meta":{"chat_id":"123","message_id":"101","type":"message_edit","unknown":[1,2]}}}}
    ]}"#;
    write_private_fixture(&path, original).unwrap();
    let scope = LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
        crate::core::ProviderId::new("discord").unwrap(),
        crate::core::ModuleSlotId::new("primary").unwrap(),
    ));
    let queue = CodexEventQueue::load(&root, scope).unwrap();
    let consumer = primary_consumer(&queue).await;
    let expected = [
        json!({"params":{"content":"prompt text\n","meta":{"chat_id":"123","message_id":"101","user_id":"456","attention_record":"record-a","attention_delivery":"prompt","unknown":{"keep":true},"citation_locators":[{"locator":"literal"}]}}}),
        json!({"params":{"content":"next text","meta":{"chat_id":"123","message_id":"102","user_id":"456","attention_record":"record-b","attention_delivery":"next_turn"}}}),
        json!({"params":{"content":"ordinary edit","meta":{"chat_id":"123","message_id":"101","type":"message_edit","unknown":[1,2]}}}),
    ];
    for payload in expected {
        let leased = queue
            .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(leased.event, payload);
        queue
            .acknowledge(&consumer, &leased.delivery_token)
            .await
            .unwrap();
    }
    assert_eq!(queue.status().await.queued, 0);
}

#[tokio::test]
async fn malformed_legacy_attention_is_rejected_without_rewriting_the_inbox() {
    let valid = json!({"params":{"content":"original\n","meta":{"chat_id":"123","message_id":"101","user_id":"456","attention_record":"record-a","attention_delivery":"prompt","unknown":{"keep":true}}}});
    let mut malformed = Vec::new();
    for method in [
        json!("notifications/claude/channel/permission"),
        json!("notifications/dione/control"),
        json!(7),
        Value::Null,
    ] {
        let mut payload = valid.clone();
        payload["method"] = method;
        payload["params"]["request_id"] = json!("control-a");
        payload["params"]["behavior"] = json!("allow");
        malformed.push(payload);
    }
    for key in [
        "chat_id",
        "message_id",
        "user_id",
        "attention_record",
        "attention_delivery",
    ] {
        let mut payload = valid.clone();
        payload["params"]["meta"]
            .as_object_mut()
            .unwrap()
            .remove(key);
        malformed.push(payload);
    }
    for (key, value) in [
        ("chat_id", json!("0")),
        ("message_id", json!("18446744073709551616")),
        ("user_id", json!(7)),
        ("attention_record", json!("   ")),
        ("attention_record", json!("x".repeat(513))),
        ("attention_delivery", json!("immediate")),
        ("type", json!("message_edit")),
        ("type", json!("message_delete")),
        ("type", json!("reaction")),
    ] {
        let mut payload = valid.clone();
        payload["params"]["meta"][key] = value;
        malformed.push(payload);
    }
    let mut payload = valid.clone();
    payload["params"]["content"] = json!(false);
    malformed.push(payload);
    for payload in malformed {
        let dir = TempDir::new().unwrap();
        let root = temp_path(&dir);
        let path = root.join(INBOX_FILE_NAME);
        let bytes =
            serde_json::to_vec(&json!({"next_id":2,"entries":[{"id":1,"payload":payload}]}))
                .unwrap();
        write_private_fixture(&path, &bytes).unwrap();
        let scope = LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        ));
        assert!(matches!(
            CodexEventQueue::load(&root, scope.clone()),
            Err(CodexQueueError {
                kind: CodexQueueErrorKind::Durable(QueueError {
                    kind: QueueErrorKind::Codec(_),
                    ..
                })
            })
        ));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        std::fs::remove_file(&path).unwrap();
        let queue = CodexEventQueue::load(&root, scope).unwrap();
        assert!(matches!(
            queue.enqueue(payload).await,
            Err(CodexQueueError {
                kind: CodexQueueErrorKind::LegacyAttention(_)
            })
        ));
        assert_eq!(queue.status().await.queued, 0);
        assert!(queue.enqueue(valid.clone()).await.unwrap());
        let mut channel = valid.clone();
        channel["method"] = json!("notifications/claude/channel");
        channel["params"]["meta"]["message_id"] = json!("102");
        assert!(queue.enqueue(channel.clone()).await.unwrap());
        assert_eq!(queue.status().await.queued, 2);
        drop(queue);
        let scope = LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        ));
        let reopened = CodexEventQueue::load(&root, scope).unwrap();
        let consumer = primary_consumer(&reopened).await;
        for expected in [valid.clone(), channel] {
            let event = reopened
                .next_event(&consumer, Duration::ZERO, DEFAULT_LEASE)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(event.event, expected);
            reopened
                .acknowledge(&consumer, &event.delivery_token)
                .await
                .unwrap();
        }
    }
}

#[tokio::test]
async fn attention_invalidation_preserves_same_message_in_another_conversation() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let mut payload = managed_message("101", "record-a", "prompt", "retained source");
    payload["params"]["meta"]["user_id"] = json!("456");
    queue.enqueue(payload).await.unwrap();
    let (binding, _) = tokio::sync::watch::channel(None);
    let connection = crate::harness::HarnessConnection::codex(queue.clone(), binding);
    connection
        .invalidate_attention(crate::core::MessageRef::new(
            crate::core::ConversationRef::new(
                queue.legacy_scope().module().clone(),
                crate::core::ConversationId::numeric(999.try_into().unwrap()),
            ),
            crate::core::MessageId::numeric(101.try_into().unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(queue.status().await.queued, 1);
    let foreign_module = crate::core::ModuleInstanceId::new(
        crate::core::ProviderId::new("discord").unwrap(),
        crate::core::ModuleSlotId::new("secondary").unwrap(),
    );
    connection
        .invalidate_attention(crate::core::MessageRef::new(
            crate::core::ConversationRef::new(
                foreign_module,
                crate::core::ConversationId::numeric(123.try_into().unwrap()),
            ),
            crate::core::MessageId::numeric(101.try_into().unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(queue.status().await.queued, 1);
    connection
        .invalidate_attention(crate::core::MessageRef::new(
            crate::core::ConversationRef::new(
                queue.legacy_scope().module().clone(),
                crate::core::ConversationId::numeric(123.try_into().unwrap()),
            ),
            crate::core::MessageId::opaque("101").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(queue.status().await.queued, 1);
    connection
        .invalidate_attention(crate::core::MessageRef::new(
            crate::core::ConversationRef::new(
                queue.legacy_scope().module().clone(),
                crate::core::ConversationId::numeric(123.try_into().unwrap()),
            ),
            crate::core::MessageId::numeric(101.try_into().unwrap()),
        ))
        .await
        .unwrap();
    assert_eq!(queue.status().await.queued, 0);
}

#[tokio::test]
async fn malformed_attention_markers_are_rejected_at_enqueue() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    for malformed in [
        json!({ "params": { "meta": { "attention_delivery": "prompt" } } }),
        json!({ "params": { "meta": { "attention_record": "record-a" } } }),
        json!({ "params": { "meta": {
            "attention_delivery": "immediate",
            "attention_record": "record-a"
        } } }),
    ] {
        assert!(matches!(
            queue.enqueue(malformed).await,
            Err(CodexQueueError {
                kind: CodexQueueErrorKind::LegacyAttention(_)
            })
        ));
    }
    assert!(queue.enqueue(message("10", "ordinary")).await.unwrap());
    assert_eq!(queue.status().await.queued, 1);
}

#[tokio::test]
async fn attention_invalidation_revokes_leases_and_preserves_unrelated_queue() {
    let dir = TempDir::new().unwrap();
    let queue = CodexEventQueue::load(
        &temp_path(&dir),
        crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
            crate::core::ProviderId::new("discord").unwrap(),
            crate::core::ModuleSlotId::new("primary").unwrap(),
        )),
    )
    .unwrap();
    let consumer = queue.register_live_consumer().await.unwrap();
    let thread = CodexThreadId::parse("invalidation-thread").unwrap();
    consumer
        .bind_live_thread(Some(thread.clone()))
        .await
        .unwrap();
    queue
        .enqueue(managed_message("101", "record-a", "prompt", "managed a"))
        .await
        .unwrap();
    queue.enqueue(message("102", "ordinary")).await.unwrap();
    queue
        .enqueue(managed_message("103", "record-b", "next_turn", "managed b"))
        .await
        .unwrap();

    let leased_managed = consumer
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(leased_managed.event["params"]["content"], "managed a");
    let invalidated = queue
        .invalidate_attention(AttentionInvalidation::Record(
            &RecordId::new("record-a").unwrap(),
        ))
        .await
        .unwrap();
    assert_eq!(
        invalidated,
        AttentionInvalidationResult {
            removed: 1,
            invalidated_leases: 1,
        }
    );
    assert!(matches!(
        consumer
            .defer_attention(&leased_managed.delivery_token, Duration::from_secs(1),)
            .await,
        Err(CodexQueueError {
            kind: CodexQueueErrorKind::Durable(QueueError {
                kind: QueueErrorKind::UnknownDeliveryToken,
                ..
            })
        })
    ));
    let ordinary = consumer
        .next_event(&thread, Duration::ZERO, DEFAULT_LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(ordinary.event["params"]["content"], "ordinary");
    let invalidated = queue
        .invalidate_attention(AttentionInvalidation::Source(
            &crate::core::MessageRef::new(
                crate::core::ConversationRef::new(
                    queue.legacy_scope().module().clone(),
                    crate::core::ConversationId::numeric(123.try_into().unwrap()),
                ),
                crate::core::MessageId::numeric(103.try_into().unwrap()),
            ),
        ))
        .await
        .unwrap();
    assert_eq!(invalidated.removed, 1);
    assert_eq!(invalidated.invalidated_leases, 0);
    consumer
        .acknowledge(&ordinary.delivery_token)
        .await
        .unwrap();
    assert_eq!(queue.status().await.queued, 0);
}
