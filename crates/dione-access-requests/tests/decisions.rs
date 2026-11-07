use camino::Utf8PathBuf;
use dione_access_requests::{AccessCtx, AccessQueue, AccessRequest, wire};
use dione_config::{Config, ConfigDurability, LoadedConfig};
use serde_json::json;
use serenity::model::id::UserId;
use std::sync::Arc;
use tokio::sync::Mutex;

#[tokio::test]
async fn public_decisions_use_real_queue_config_and_reload() {
    let directory = tempfile::tempdir().unwrap();
    let path = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    std::fs::write(path.join("config.toml"), "").unwrap();
    let queue = Arc::new(Mutex::new(AccessQueue::load(&path)));
    let access = AccessCtx::new(
        queue.clone(),
        Arc::new(LoadedConfig::try_from_raw(Config::default()).unwrap()),
        path.clone(),
    );
    let request = |id| AccessRequest {
        user_id: id,
        username: format!("user-{id}"),
        message_preview: "please retain this payload".into(),
        timestamp: "2026-01-02T12:00:00Z".parse().unwrap(),
    };
    queue.lock().await.enqueue(request(42), 50);
    queue.lock().await.enqueue(request(43), 50);
    let listed = access.list().await;
    assert_eq!(listed.len(), 2);
    assert_eq!(listed[0].user_id().to_string(), "42");
    assert_eq!(listed[0].message_preview(), "please retain this payload");
    let receipt = access.approve(UserId::new(42)).await.unwrap();
    assert_eq!(receipt.user_id(), UserId::new(42));
    assert_eq!(receipt.username(), "user-42");
    assert!(receipt.generation() > 0);
    assert!(matches!(receipt.durability(), ConfigDurability::Durable));
    let projected = wire::approval(Ok(receipt));
    assert_eq!(projected["durability"], "durable");
    assert_eq!(projected["request_retained"], false);
    assert_eq!(
        dione_config::load_config(&path).access.allow_from,
        vec!["42"]
    );
    assert!(AccessQueue::load(&path).peek(42).is_none());
    assert!(AccessQueue::load(&path).peek(43).is_some());
    assert_eq!(
        wire::approval(access.approve(UserId::new(42)).await),
        json!({"error":"no pending request for user 42"})
    );
    assert_eq!(
        wire::denial(access.deny(UserId::new(43)).await),
        json!({"ok":true,"username":"user-43","user_id":"43"})
    );
    assert!(AccessQueue::load(&path).list().is_empty());
    assert_eq!(
        dione_config::load_config(&path).access.allow_from,
        vec!["42"]
    );
}
