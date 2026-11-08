use dione_config::{Config, LoadedConfig};
use dione_discord_state::new_state;
use dione_discord_typing::{TypingCtx, TypingError, send_typing, wire};
use serenity::{http::Http, model::id::ChannelId};
use std::sync::Arc;

#[tokio::test]
async fn ordinary_consumer_denial_uses_live_state_without_http() {
    let ctx = TypingCtx::new(
        Arc::new(Http::new("synthetic-unused-token")),
        new_state(),
        Arc::new(LoadedConfig::try_from_raw(Config::default()).unwrap()),
    );
    let result = send_typing(&ctx, ChannelId::new(999)).await;
    assert!(matches!(result, Err(TypingError::Denied)));
    assert_eq!(
        wire::typing(result),
        serde_json::json!({"error": "channel not in allowlist"})
    );
}
