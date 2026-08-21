use crate::{config::LoadedConfig, gate::OutboundGate, state::State};
use serde_json::{Value, json};
use serenity::model::id::ChannelId;
use std::{fmt, str::FromStr, sync::Arc};
use tokio::sync::mpsc;

/// Bot online status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnlineStatus {
    Online,
    Idle,
    Dnd,
    Invisible,
}

impl OnlineStatus {
    pub const ALL: &[OnlineStatus] = &[
        OnlineStatus::Online,
        OnlineStatus::Idle,
        OnlineStatus::Dnd,
        OnlineStatus::Invisible,
    ];

    pub fn json_enum() -> Value {
        Value::Array(
            Self::ALL
                .iter()
                .map(|s| Value::String(s.to_string()))
                .collect(),
        )
    }

    pub fn to_serenity(self) -> serenity::model::user::OnlineStatus {
        match self {
            Self::Online => serenity::model::user::OnlineStatus::Online,
            Self::Idle => serenity::model::user::OnlineStatus::Idle,
            Self::Dnd => serenity::model::user::OnlineStatus::DoNotDisturb,
            Self::Invisible => serenity::model::user::OnlineStatus::Invisible,
        }
    }
}

impl fmt::Display for OnlineStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Online => write!(f, "online"),
            Self::Idle => write!(f, "idle"),
            Self::Dnd => write!(f, "dnd"),
            Self::Invisible => write!(f, "invisible"),
        }
    }
}

impl FromStr for OnlineStatus {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "online" => Ok(Self::Online),
            "idle" => Ok(Self::Idle),
            "dnd" => Ok(Self::Dnd),
            "invisible" => Ok(Self::Invisible),
            other => Err(format!(
                "invalid online_status: {other}; must be one of: {}",
                Self::ALL
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Bot activity type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityType {
    Playing,
    Watching,
    Listening,
    Competing,
    Custom,
}

impl ActivityType {
    pub const ALL: &[ActivityType] = &[
        ActivityType::Playing,
        ActivityType::Watching,
        ActivityType::Listening,
        ActivityType::Competing,
        ActivityType::Custom,
    ];

    pub fn json_enum() -> Value {
        Value::Array(
            Self::ALL
                .iter()
                .map(|s| Value::String(s.to_string()))
                .collect(),
        )
    }

    pub fn to_activity(self, name: &str) -> serenity::gateway::ActivityData {
        use serenity::gateway::ActivityData;
        match self {
            Self::Playing => ActivityData::playing(name),
            Self::Watching => ActivityData::watching(name),
            Self::Listening => ActivityData::listening(name),
            Self::Competing => ActivityData::competing(name),
            Self::Custom => ActivityData::custom(name),
        }
    }
}

impl fmt::Display for ActivityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Playing => write!(f, "playing"),
            Self::Watching => write!(f, "watching"),
            Self::Listening => write!(f, "listening"),
            Self::Competing => write!(f, "competing"),
            Self::Custom => write!(f, "custom"),
        }
    }
}

impl FromStr for ActivityType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "playing" => Ok(Self::Playing),
            "watching" => Ok(Self::Watching),
            "listening" => Ok(Self::Listening),
            "competing" => Ok(Self::Competing),
            "custom" => Ok(Self::Custom),
            other => Err(format!(
                "invalid activity_type: {other}; must be one of: {}",
                Self::ALL
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )),
        }
    }
}

/// Commands the Discord task can execute on behalf of MCP tools that need
/// gateway-level operations (e.g. presence updates).
#[derive(Debug, Clone)]
pub enum DiscordCommand {
    /// Update the bot's presence status and activity.
    SetPresence {
        online_status: OnlineStatus,
        activity_type: Option<ActivityType>,
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

pub async fn set_presence(
    ctx: &BotStateCtx,
    online_status: &str,
    activity_type: Option<&str>,
    activity_name: Option<&str>,
) -> Value {
    let Some(ref tx) = ctx.discord_cmd_tx else {
        return json!({ "error": "presence updates require the discord gateway command channel" });
    };

    let status = match OnlineStatus::from_str(online_status) {
        Ok(s) => s,
        Err(e) => return json!({ "error": e }),
    };

    let at = match activity_type {
        Some(s) => match ActivityType::from_str(s) {
            Ok(a) => Some(a),
            Err(e) => return json!({ "error": e }),
        },
        None => None,
    };

    if at.is_some() && activity_name.is_none() {
        return json!({ "error": "activity_name is required when activity_type is set" });
    }

    let cmd = DiscordCommand::SetPresence {
        online_status: status,
        activity_type: at,
        activity_name: activity_name.map(|s| s.to_string()),
    };

    match tx.send(cmd).await {
        Ok(()) => json!({ "ok": true, "status": "accepted" }),
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
