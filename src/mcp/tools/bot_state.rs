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
    /// Shared presence state (desired-presence store + live sink slot), the
    /// synchronous authority for `set_presence`/`get_presence`. `None` on
    /// transports that carry no gateway.
    pub presence: Option<crate::discord::events::SharedPresence>,
    pub state: State,
    pub config: Arc<LoadedConfig>,
}

impl BotStateCtx {
    /// Core context with no gateway wiring. Gateway-backed transports attach
    /// the command channel and presence store via the `with_*` builders, so
    /// adding optional wiring never breaks existing constructions.
    pub fn new(http: Arc<serenity::http::Http>, state: State, config: Arc<LoadedConfig>) -> Self {
        Self {
            http,
            discord_cmd_tx: None,
            presence: None,
            state,
            config,
        }
    }

    pub fn with_discord_cmd_tx(mut self, tx: Option<mpsc::Sender<DiscordCommand>>) -> Self {
        self.discord_cmd_tx = tx;
        self
    }

    pub fn with_presence(
        mut self,
        presence: Option<crate::discord::events::SharedPresence>,
    ) -> Self {
        self.presence = presence;
        self
    }
}

// ── set_presence ──────────────────────────────────────────────────────────────

pub async fn set_presence(
    ctx: &BotStateCtx,
    online_status: &str,
    activity_type: Option<&str>,
    activity_name: Option<&str>,
) -> Value {
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

    let activity = match (at, activity_name) {
        (Some(kind), Some(name)) => Some(kind.to_activity(name)),
        (Some(_), None) => {
            return json!({ "error": "activity_name is required when activity_type is set" });
        }
        (None, _) => None,
    };

    // Preferred path: update the authoritative desired-state store
    // synchronously. When this returns, the request is stored (and pushed to
    // the live sink if one is installed), so an immediate `get_presence`
    // reflects it — no queue consumer sits between the acknowledgment and
    // the state change.
    if let Some(ref presence) = ctx.presence {
        presence.set_presence(activity, status.to_serenity()).await;
        return json!({ "ok": true, "status": "applied" });
    }

    // Fallback for transports wired with only the command channel: the
    // request is queued, and the weaker status string says so.
    let Some(ref tx) = ctx.discord_cmd_tx else {
        return json!({ "error": "presence updates require the discord gateway command channel" });
    };

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

// ── get_presence ──────────────────────────────────────────────────────────────

fn serenity_status_str(status: serenity::model::user::OnlineStatus) -> &'static str {
    use serenity::model::user::OnlineStatus as S;
    match status {
        S::Online => "online",
        S::Idle => "idle",
        S::DoNotDisturb => "dnd",
        S::Invisible => "invisible",
        S::Offline => "offline",
        _ => "unknown",
    }
}

fn serenity_activity_kind_str(kind: serenity::model::gateway::ActivityType) -> &'static str {
    use serenity::model::gateway::ActivityType as A;
    match kind {
        A::Playing => "playing",
        A::Watching => "watching",
        A::Listening => "listening",
        A::Competing => "competing",
        A::Custom => "custom",
        _ => "unknown",
    }
}

/// Build the `get_presence` response from a snapshot. Pure, so tests pin the
/// shape without a gateway.
pub fn presence_snapshot_json(snap: &crate::discord::events::PresenceSnapshot) -> Value {
    // `sink_installed` is deliberately narrow: the sink is installed on each
    // gateway `ready()` and never cleared on disconnect, so it reports "a
    // shard messenger has been installed since the last ready" — it is NOT a
    // live connection probe, and the field name must not claim one.
    match &snap.desired {
        None => json!({
            "set_this_process": false,
            "sink_installed": snap.sink_installed,
            "note": "no presence has been requested since this process started; \
                     Discord is showing the gateway default. sink_installed reports \
                     whether a shard messenger has been installed since the last \
                     gateway ready — disconnects are not tracked",
        }),
        Some(d) => json!({
            "set_this_process": true,
            "online_status": serenity_status_str(d.status),
            "activity_type": d.activity.as_ref().map(|a| serenity_activity_kind_str(a.kind)),
            // A custom activity carries its human text in `state` (Discord
            // shows the `name` slot as the "~" placeholder); every other kind
            // carries it in `name`.
            "activity_name": d.activity.as_ref().map(|a| {
                a.state.clone().filter(|_| a.kind == serenity::model::gateway::ActivityType::Custom)
                    .unwrap_or_else(|| a.name.clone())
            }),
            "set_at": d.set_at.to_rfc3339(),
            "sink_installed": snap.sink_installed,
            "source": "last requested presence (replayed on reconnect), not a Discord \
                       read-back; sink_installed is not a live connection probe",
        }),
    }
}

pub async fn get_presence(ctx: &BotStateCtx) -> Value {
    let Some(ref presence) = ctx.presence else {
        return json!({ "error": "presence state is not wired on this transport" });
    };
    presence_snapshot_json(&presence.snapshot().await)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::events::SharedPresence;

    fn test_ctx() -> BotStateCtx {
        BotStateCtx::new(
            Arc::new(serenity::http::Http::new("fake")),
            crate::state::new_state(),
            Arc::new(LoadedConfig::from_raw(crate::config::Config::default())),
        )
    }

    #[tokio::test]
    async fn get_presence_before_any_request_reports_unset() {
        let presence = SharedPresence::new();
        let out = presence_snapshot_json(&presence.snapshot().await);
        assert_eq!(out["set_this_process"], false);
        assert_eq!(out["sink_installed"], false);
        assert!(out.get("online_status").is_none());
    }

    #[tokio::test]
    async fn get_presence_reads_back_the_last_request() {
        let presence = SharedPresence::new();
        presence
            .set_presence(
                Some(ActivityType::Custom.to_activity("booting...")),
                OnlineStatus::Dnd.to_serenity(),
            )
            .await;
        let out = presence_snapshot_json(&presence.snapshot().await);
        assert_eq!(out["set_this_process"], true);
        assert_eq!(out["online_status"], "dnd");
        assert_eq!(out["activity_type"], "custom");
        assert_eq!(out["activity_name"], "booting...");
        // sink_installed is false: no sink was ever installed here, so the
        // request is stored for replay — exactly the honest answer.
        assert_eq!(out["sink_installed"], false);
        let stamp = out["set_at"].as_str().expect("set_at is a string");
        assert!(chrono::DateTime::parse_from_rfc3339(stamp).is_ok());
    }

    /// The blocked-consumer regression: `set_presence` must be the
    /// synchronous authority. Even with a command channel whose consumer
    /// never runs, the tool acknowledges "applied" and an immediate
    /// `get_presence` reflects the request.
    #[tokio::test]
    async fn set_presence_applies_synchronously_even_with_a_blocked_consumer() {
        let presence = SharedPresence::new();
        let (tx, _blocked_rx) = mpsc::channel(1);
        let ctx = test_ctx()
            .with_discord_cmd_tx(Some(tx))
            .with_presence(Some(presence.clone()));

        let out = set_presence(&ctx, "idle", Some("custom"), Some("proof")).await;
        assert_eq!(out["ok"], true);
        assert_eq!(out["status"], "applied");

        let read = presence_snapshot_json(&presence.snapshot().await);
        assert_eq!(read["online_status"], "idle");
        assert_eq!(read["activity_name"], "proof");
    }

    /// Transports wired with only the command channel keep the queue path,
    /// and the weaker "accepted" status says so.
    #[tokio::test]
    async fn set_presence_without_a_presence_store_falls_back_to_the_queue() {
        let (tx, mut rx) = mpsc::channel(1);
        let ctx = test_ctx().with_discord_cmd_tx(Some(tx));

        let out = set_presence(&ctx, "online", None, None).await;
        assert_eq!(out["ok"], true);
        assert_eq!(out["status"], "accepted");
        assert!(matches!(
            rx.try_recv(),
            Ok(DiscordCommand::SetPresence { .. })
        ));
    }

    #[tokio::test]
    async fn set_presence_rejects_invalid_input_before_touching_state() {
        let presence = SharedPresence::new();
        let ctx = test_ctx().with_presence(Some(presence.clone()));

        let bad_status = set_presence(&ctx, "sleeping", None, None).await;
        assert!(bad_status.get("error").is_some());
        let missing_name = set_presence(&ctx, "online", Some("custom"), None).await;
        assert!(missing_name.get("error").is_some());
        assert!(presence.snapshot().await.desired.is_none());
    }
}
