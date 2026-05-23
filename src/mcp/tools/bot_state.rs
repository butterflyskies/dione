use std::sync::Arc;

use serde_json::{Value, json};
use serenity::model::id::ChannelId;
use tokio::sync::mpsc;

use crate::config::LoadedConfig;
use crate::gate::OutboundGate;
use crate::state::State;

/// Commands the Discord task can execute on behalf of MCP tools that need
/// gateway-level operations (e.g. presence updates).
#[derive(Debug)]
pub enum DiscordCommand {
    /// Update the bot's presence status and activity.
    SetPresence {
        status: String,
        activity_name: Option<String>,
    },
}

/// Context for bot-state tools.
pub struct BotStateCtx {
    pub http: Arc<serenity::http::Http>,
    pub discord_cmd_tx: Option<mpsc::Sender<DiscordCommand>>,
    pub state: State,
    pub config: Arc<LoadedConfig>,
}

// ── set_presence ──────────────────────────────────────────────────────────────

pub async fn set_presence(ctx: &BotStateCtx, status: &str, activity_name: Option<&str>) -> Value {
    let Some(ref tx) = ctx.discord_cmd_tx else {
        return json!({ "error": "presence updates require the discord gateway command channel" });
    };

    let cmd = DiscordCommand::SetPresence {
        status: status.to_string(),
        activity_name: activity_name.map(|s| s.to_string()),
    };

    match tx.send(cmd).await {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": format!("failed to send presence command: {e}") }),
    }
}

// ── send_typing ───────────────────────────────────────────────────────────────

pub async fn send_typing(ctx: &BotStateCtx, channel_id: u64) -> Value {
    let allowed = {
        let state = ctx.state.read().await;
        OutboundGate::check_channel(&ctx.config, channel_id, &state.dm_channel_ids)
    };
    if !allowed {
        return json!({ "error": "channel not in allowlist" });
    }

    match ctx.http.broadcast_typing(ChannelId::new(channel_id)).await {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}
