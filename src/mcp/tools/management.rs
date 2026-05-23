use std::sync::Arc;

use serde_json::{Value, json};
use serenity::builder::CreateThread;
use serenity::model::channel::ChannelType;
use serenity::model::id::{ChannelId, MessageId};

use crate::config::LoadedConfig;
use crate::gate::OutboundGate;
use crate::state::State;

/// Context for channel management tools.
pub struct ManagementCtx {
    pub http: Arc<serenity::http::Http>,
    pub state: State,
    pub config: Arc<LoadedConfig>,
}

async fn check_outbound(ctx: &ManagementCtx, channel_id: u64) -> Result<(), Value> {
    let state = ctx.state.read().await;
    if !OutboundGate::check_channel(&ctx.config, channel_id, &state.dm_channel_ids) {
        return Err(json!({ "error": "channel not in allowlist" }));
    }
    Ok(())
}

// ── pin_message ───────────────────────────────────────────────────────────────

pub async fn pin_message(ctx: &ManagementCtx, channel_id: u64, message_id: u64) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }
    match ctx
        .http
        .pin_message(ChannelId::new(channel_id), MessageId::new(message_id), None)
        .await
    {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── unpin_message ─────────────────────────────────────────────────────────────

pub async fn unpin_message(ctx: &ManagementCtx, channel_id: u64, message_id: u64) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }
    match ctx
        .http
        .unpin_message(ChannelId::new(channel_id), MessageId::new(message_id), None)
        .await
    {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── create_thread ─────────────────────────────────────────────────────────────

pub async fn create_thread(
    ctx: &ManagementCtx,
    channel_id: u64,
    message_id: Option<u64>,
    name: &str,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }
    let thread_builder = CreateThread::new(name).kind(ChannelType::PublicThread);

    match message_id {
        Some(mid) => {
            match ctx
                .http
                .create_thread_from_message(
                    ChannelId::new(channel_id),
                    MessageId::new(mid),
                    &thread_builder,
                    None,
                )
                .await
            {
                Ok(ch) => json!({
                    "ok": true,
                    "thread_id": ch.id.get().to_string(),
                    "name": ch.name,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        None => {
            match ctx
                .http
                .create_thread(ChannelId::new(channel_id), &thread_builder, None)
                .await
            {
                Ok(ch) => json!({
                    "ok": true,
                    "thread_id": ch.id.get().to_string(),
                    "name": ch.name,
                }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
    }
}

// ── delete_message ────────────────────────────────────────────────────────────

pub async fn delete_message(ctx: &ManagementCtx, channel_id: u64, message_id: u64) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }
    match ctx
        .http
        .delete_message(ChannelId::new(channel_id), MessageId::new(message_id), None)
        .await
    {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}
