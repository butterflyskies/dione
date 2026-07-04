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
        /// Online status: "online", "idle", "dnd", or "invisible".
        online_status: String,
        /// Activity type: "playing", "watching", "listening", "competing", or "custom".
        activity_type: Option<String>,
        /// The activity text (e.g. "catena", "the void").
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

/// Valid online statuses for the `set_presence` tool.
const VALID_STATUSES: &[&str] = &["online", "idle", "dnd", "invisible"];

/// Valid activity types for the `set_presence` tool.
const VALID_ACTIVITY_TYPES: &[&str] = &["playing", "watching", "listening", "competing", "custom"];

pub async fn set_presence(
    ctx: &BotStateCtx,
    online_status: &str,
    activity_type: Option<&str>,
    activity_name: Option<&str>,
) -> Value {
    let Some(ref tx) = ctx.discord_cmd_tx else {
        return json!({ "error": "presence updates require the discord gateway command channel" });
    };

    if !VALID_STATUSES.contains(&online_status) {
        return json!({
            "error": format!(
                "invalid online_status: {online_status}; must be one of: {}",
                VALID_STATUSES.join(", ")
            )
        });
    }

    if let Some(at) = activity_type
        && !VALID_ACTIVITY_TYPES.contains(&at)
    {
        return json!({
            "error": format!(
                "invalid activity_type: {at}; must be one of: {}",
                VALID_ACTIVITY_TYPES.join(", ")
            )
        });
    }

    // activity_name is required when activity_type is set.
    if activity_type.is_some() && activity_name.is_none() {
        return json!({ "error": "activity_name is required when activity_type is set" });
    }

    let cmd = DiscordCommand::SetPresence {
        online_status: online_status.to_string(),
        activity_type: activity_type.map(|s| s.to_string()),
        activity_name: activity_name.map(|s| s.to_string()),
    };

    match tx.send(cmd).await {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": format!("failed to send presence command: {e}") }),
    }
}

// ── send_typing ───────────────────────────────────────────────────────────────

pub async fn send_typing(ctx: &BotStateCtx, channel_id: ChannelId) -> Value {
    let allowed = {
        let state = ctx.state.read().await;
        OutboundGate::check_channel_with_threads(
            &ctx.config,
            channel_id.get(),
            &state.dm_channel_ids,
            &state.thread_parents,
        )
    };
    if !allowed {
        return json!({ "error": "channel not in allowlist" });
    }

    match ctx.http.broadcast_typing(channel_id).await {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}
