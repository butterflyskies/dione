use crate::{
    config::LoadedConfig, gate::OutboundGate, ingress_ledger::IngressLedger,
    mcp::tools::messaging::verify_message_target, state::State,
};
use serde_json::{Value, json};
use serenity::{
    builder::CreateThread,
    model::{
        channel::ChannelType,
        id::{ChannelId, MessageId},
    },
};
use std::sync::Arc;

/// Context for channel management tools.
pub struct ManagementCtx {
    pub http: Arc<serenity::http::Http>,
    pub state: State,
    pub config: Arc<LoadedConfig>,
    pub ingress_ledger: Arc<IngressLedger>,
}

async fn check_outbound(ctx: &ManagementCtx, channel_id: ChannelId) -> Result<(), Value> {
    let state = ctx.state.read().await;
    if !OutboundGate::check_channel_with_threads(
        &ctx.config,
        channel_id.get(),
        &state.dm_channel_ids,
        &state.thread_parents,
    ) {
        return Err(json!({ "error": "channel not in allowlist" }));
    }
    Ok(())
}

// ── pin_message ───────────────────────────────────────────────────────────────

pub async fn pin_message(
    ctx: &ManagementCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }
    let own_send = ctx.state.read().await.is_own_send(message_id.get());
    if let Err(e) = verify_message_target(
        &ctx.ingress_ledger,
        &ctx.http,
        &ctx.config,
        message_id,
        channel_id,
        "pin_message",
        own_send,
    ) {
        return e;
    }
    match ctx.http.pin_message(channel_id, message_id, None).await {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── unpin_message ─────────────────────────────────────────────────────────────

pub async fn unpin_message(
    ctx: &ManagementCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }
    let own_send = ctx.state.read().await.is_own_send(message_id.get());
    if let Err(e) = verify_message_target(
        &ctx.ingress_ledger,
        &ctx.http,
        &ctx.config,
        message_id,
        channel_id,
        "unpin_message",
        own_send,
    ) {
        return e;
    }
    match ctx.http.unpin_message(channel_id, message_id, None).await {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── create_thread ─────────────────────────────────────────────────────────────

pub async fn create_thread(
    ctx: &ManagementCtx,
    channel_id: ChannelId,
    message_id: Option<MessageId>,
    name: &str,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }
    if let Some(mid) = message_id {
        let own_send = ctx.state.read().await.is_own_send(mid.get());
        if let Err(e) = verify_message_target(
            &ctx.ingress_ledger,
            &ctx.http,
            &ctx.config,
            mid,
            channel_id,
            "create_thread",
            own_send,
        ) {
            return e;
        }
    }

    let thread_builder = CreateThread::new(name).kind(ChannelType::PublicThread);

    let result = match message_id {
        Some(mid) => {
            ctx.http
                .create_thread_from_message(channel_id, mid, &thread_builder, None)
                .await
        }
        None => {
            ctx.http
                .create_thread(channel_id, &thread_builder, None)
                .await
        }
    };

    match result {
        Ok(ch) => {
            let thread_id = ch.id.get();
            // Record thread → parent mapping so the gate allows sending to this thread.
            {
                let mut state = ctx.state.write().await;
                state.record_thread_parent(thread_id, Some(channel_id.get()));
            }
            json!({
                "ok": true,
                "thread_id": thread_id.to_string(),
                "name": ch.name,
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── delete_message ────────────────────────────────────────────────────────────

pub async fn delete_message(
    ctx: &ManagementCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }
    let own_send = ctx.state.read().await.is_own_send(message_id.get());
    if let Err(e) = verify_message_target(
        &ctx.ingress_ledger,
        &ctx.http,
        &ctx.config,
        message_id,
        channel_id,
        "delete_message",
        own_send,
    ) {
        return e;
    }
    match ctx.http.delete_message(channel_id, message_id, None).await {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{ChannelConfig, Config, LoadedConfig},
        state::new_state,
    };
    use serenity::model::id::ChannelId;
    use std::sync::Arc;

    fn config_with_channel(channel_id: u64) -> LoadedConfig {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: channel_id.to_string(),
            ..Default::default()
        });
        LoadedConfig::from_raw(raw)
    }

    fn ctx(config: LoadedConfig) -> ManagementCtx {
        ManagementCtx {
            http: Arc::new(serenity::http::Http::new("fake")),
            state: new_state(),
            config: Arc::new(config),
            ingress_ledger: Arc::new(crate::ingress_ledger::IngressLedger::new()),
        }
    }

    #[tokio::test]
    async fn configured_channel_is_allowed() {
        let ctx = ctx(config_with_channel(100));
        assert!(check_outbound(&ctx, ChannelId::new(100)).await.is_ok());
    }

    #[tokio::test]
    async fn unknown_channel_is_denied() {
        let ctx = ctx(config_with_channel(100));
        assert!(check_outbound(&ctx, ChannelId::new(999)).await.is_err());
    }

    /// Mirrors the state write that `create_thread` performs on success:
    ///   `state.record_thread_parent(thread_id, Some(channel_id.get()))`
    /// After that write, outbound traffic to the new thread ID must be permitted.
    #[tokio::test]
    async fn thread_allowed_via_record_thread_parent() {
        let parent = 100u64;
        let thread = 200u64;
        let ctx = ctx(config_with_channel(parent));
        {
            let mut state = ctx.state.write().await;
            state.record_thread_parent(thread, Some(parent));
        }
        assert!(
            check_outbound(&ctx, ChannelId::new(thread)).await.is_ok(),
            "thread whose parent is in config must be allowed"
        );
    }

    #[tokio::test]
    async fn thread_denied_when_parent_not_in_config() {
        let parent = 100u64;
        let thread = 200u64;
        let ctx = ctx(config_with_channel(999));
        {
            let mut state = ctx.state.write().await;
            state.record_thread_parent(thread, Some(parent));
        }
        assert!(
            check_outbound(&ctx, ChannelId::new(thread)).await.is_err(),
            "thread whose parent is absent from config must be denied"
        );
    }

    /// dione#334: every management call site must consult the own-send signal.
    /// With an empty ledger and empty `recent_sent_ids`, `own_send` is false and
    /// every target is Unknown, so the phantom canary must block before any
    /// Discord call. This goes red if any site hardcodes `own_send = true`
    /// (the exact mutation that otherwise survives the whole suite) — or drops
    /// the check entirely.
    #[tokio::test]
    async fn management_ops_block_non_own_unknown_targets() {
        let ctx = ctx(config_with_channel(42));
        let ch = ChannelId::new(42);
        let target = MessageId::new(8);
        let cases = [
            ("delete_message", delete_message(&ctx, ch, target).await),
            ("pin_message", pin_message(&ctx, ch, target).await),
            ("unpin_message", unpin_message(&ctx, ch, target).await),
            (
                "create_thread",
                create_thread(&ctx, ch, Some(target), "t").await,
            ),
        ];
        for (op, result) in cases {
            assert!(
                result["error"]
                    .as_str()
                    .is_some_and(|error| error.contains("possible phantom")),
                "{op} on a non-own unknown target must trip the phantom canary; got {result}"
            );
        }
    }
}
