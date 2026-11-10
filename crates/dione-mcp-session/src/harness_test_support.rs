//! Typed fixtures shared by adapter and routing tests.

use dione_contracts::{ModuleInstanceId, ModuleSlotId, ProviderId};
use serde_json::Value;
use {
    dione_discord_events::{self as delivery, NotificationEvent},
    dione_messages::delivery::{Delivery, EvidenceVisibility, PresentationContext},
};

pub(crate) fn module() -> ModuleInstanceId {
    ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    )
}
pub(crate) fn context() -> PresentationContext {
    PresentationContext::new(None, EvidenceVisibility::Enabled)
}
pub(crate) fn delivery(event: NotificationEvent) -> Delivery {
    delivery::single(&module(), event, context()).unwrap()
}
pub(crate) async fn snapshot(value: &Delivery) -> Value {
    let directory = tempfile::TempDir::new().unwrap();
    let path = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let queue =
        dione_codex::CodexEventQueue::open(&path, dione_codex::LegacyDiscordScope::new(module()))
            .await
            .unwrap();
    let consumer = queue
        .register_consumer(
            "fixture".into(),
            std::time::Duration::from_secs(60),
            true,
            true,
        )
        .await
        .unwrap()
        .consumer_id;
    let connection = dione_codex::connect(queue.clone(), tokio::sync::watch::channel(None).0);
    connection.deliver(value).await.unwrap();
    queue
        .next_event(
            &consumer,
            std::time::Duration::ZERO,
            std::time::Duration::from_secs(30),
        )
        .await
        .unwrap()
        .expect("actual Codex adapter retained notification")
        .event
}
