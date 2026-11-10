use dione_contracts::{ModuleInstanceId, ModuleSlotId, ProviderId};
use dione_discord_events::NotificationEvent;
use dione_messages::delivery::Delivery;
use serde_json::Value;
use std::time::Duration;

type Error = Box<dyn std::error::Error + Send + Sync>;
pub async fn snapshot(delivery: &Delivery) -> Result<Value, Error> {
    let directory = tempfile::TempDir::new()?;
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let module = ModuleInstanceId::new(ProviderId::new("discord")?, ModuleSlotId::new("fixture")?);
    let queue =
        dione_codex::CodexEventQueue::open(&path, dione_codex::LegacyDiscordScope::new(module))
            .await?;
    let consumer = queue
        .register_consumer("fixture".into(), Duration::from_secs(60), true, true)
        .await?
        .consumer_id;
    let connection = dione_codex::connect(queue.clone(), tokio::sync::watch::channel(None).0);
    connection.deliver(delivery).await?;
    Ok(queue
        .next_event(&consumer, Duration::ZERO, Duration::from_secs(30))
        .await?
        .expect("actual adapter retained notification")
        .event)
}
pub async fn make_notification(event: NotificationEvent) -> Result<Value, Error> {
    snapshot(&crate::mcp::server::test_helpers::make_notification(event)?).await
}
