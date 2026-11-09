//! Root consumer fixtures enter the same concrete gateway event boundary as the SDK.
use dione_discord_runtime::{GatewayHandler, GatewayServices, IngressLedger};
use serenity::model::id::{ChannelId, MessageId, UserId};
use std::sync::Arc;

pub(crate) fn module() -> dione_contracts::ModuleInstanceId {
    dione_contracts::ModuleInstanceId::new(
        dione_contracts::ProviderId::new("discord").unwrap(),
        dione_contracts::ModuleSlotId::new("fixture").unwrap(),
    )
}

pub(crate) fn handler(
    ledger: Arc<IngressLedger>,
    state: dione_discord_state::State,
    directory: camino::Utf8PathBuf,
) -> (
    GatewayHandler,
    tokio::sync::mpsc::Receiver<dione_discord_events::NotificationEvent>,
) {
    let (tx, rx) = tokio::sync::mpsc::channel(32);
    let (presence, _) = dione_discord_presence::wire_shared_presence();
    let queue = Arc::new(tokio::sync::Mutex::new(
        dione_access_requests::AccessQueue::load(&directory),
    ));
    (
        GatewayHandler::new(GatewayServices::new(
            state,
            queue,
            tx,
            directory,
            ledger,
            presence,
            module(),
        )),
        rx,
    )
}

pub(crate) async fn admit_direct(
    ledger: &Arc<IngressLedger>,
    message: MessageId,
    channel: ChannelId,
    user: UserId,
    content: &str,
) {
    let dir = tempfile::tempdir().unwrap();
    let directory = camino::Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    // This standalone event fixture owns its cached policy. Consumer assertions
    // retain their independently constructed configuration unchanged.
    std::fs::write(
        directory.join("config.toml"),
        format!("[access]\nallow_from = [\"{}\"]\n", user.get()),
    ).unwrap();
    assert!(dione_config::reload_config(&directory).await.1.is_none());
    let state = dione_discord_state::new_state();
    let (handler, mut rx) = handler(ledger.clone(), state, directory.clone());
    let raw = serde_json::json!({
        "id": message.get().to_string(), "channel_id": channel.get().to_string(),
        "author": {"id": user.get().to_string(), "username": "fixture-orbit", "discriminator": "0", "avatar": null, "bot": false},
        "content": content, "timestamp": serenity::model::Timestamp::now().to_rfc3339().unwrap(),
        "edited_timestamp": null, "tts": false, "mention_everyone": false, "mentions": [],
        "mention_roles": [], "attachments": [], "embeds": [], "pinned": false, "type": 0, "flags": 0
    });
    let event = serde_json::from_value(raw).unwrap();
    handler
        .process_message(&serenity::http::Http::new("fixture"), event)
        .await;
    std::fs::write(directory.join("config.toml"), "").unwrap();
    assert!(dione_config::reload_config(&directory).await.1.is_none());
    assert!(
        matches!(rx.try_recv().unwrap(), dione_discord_events::NotificationEvent::Message(value) if value.message_id == message && value.chat_id == channel && value.user_id == user && value.content == content)
    );
}
