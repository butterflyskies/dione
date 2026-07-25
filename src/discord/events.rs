use crate::{
    bell_rings::BellStatus,
    gate::{GateDecision, InboundGate, MentionDetector, MentionKind},
    mcp::tools::bot_state::DiscordCommand,
    mcp::tools::messaging::create_dm_channel,
    queue::AccessRequest,
    timestamp::Timestamp,
};
use serenity::{
    async_trait,
    builder::{CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage},
    model::{event::MessageUpdateEvent, prelude::*},
    prelude::*,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

// ── Event types ───────────────────────────────────────────────────────────────

/// Metadata about a Discord attachment, forwarded to the MCP client.
#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub name: String,
    pub content_type: Option<String>,
    pub size: u64,
}

/// A Discord message forwarded from the gateway to the MCP notification stream.
#[derive(Debug, Clone)]
pub struct MessageEvent {
    pub chat_id: ChannelId,
    pub message_id: MessageId,
    pub user: String,
    pub user_id: UserId,
    pub content: String,
    /// Typed targeting evidence captured at the Discord ingress boundary.
    pub targeting: MessageTargeting,
    pub timestamp: Timestamp,
    pub attachments: Vec<AttachmentMeta>,
    pub is_voice_message: bool,
    /// If the message was sent in a thread, the parent channel ID.
    pub thread_parent_id: Option<ChannelId>,
    /// If the message is a reply, the ID of the message being replied to.
    pub reply_to_message_id: Option<MessageId>,
    /// If the message is a reply, the author ID of the replied-to message.
    pub reply_to_user_id: Option<UserId>,
    /// If the message is a reply, the author name of the replied-to message.
    pub reply_to_user: Option<String>,
    /// If the message is a reply, a short preview of the replied-to content.
    pub reply_to_content_preview: Option<String>,
    /// Pre-rendered bell metadata (live mode only).
    pub bells: Option<String>,
    /// Retrieval status for bell evaluation.
    pub bells_status: Option<BellStatus>,
}

/// Whether Discord ingress had explicit evidence that a message targeted this construct.
#[non_exhaustive]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageTargeting {
    /// An accepted direct message.
    DirectMessage,
    /// A guild message with explicit typed mention evidence.
    GuildDirected(MentionKind),
    /// An opted-in guild message delivered without directed evidence.
    Ambient,
}

impl MessageTargeting {
    /// Returns whether the message is eligible for directed-only processing.
    pub fn is_directed(self) -> bool {
        !matches!(self, Self::Ambient)
    }
}

/// Events forwarded from the Discord gateway to the MCP notification stream.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    Message(MessageEvent),
    Reaction {
        chat_id: ChannelId,
        message_id: MessageId,
        user: String,
        user_id: UserId,
        emoji: String,
    },
    PermissionResponse {
        request_id: String,
        granted: bool,
    },
    Trace {
        level: String,
        target: String,
        message: String,
        fields: Vec<(String, String)>,
    },
    MessageEdit {
        chat_id: ChannelId,
        message_id: MessageId,
        user: String,
        user_id: UserId,
        new_content: String,
        timestamp: Timestamp,
        /// If the edit was in a thread, the parent channel ID.
        thread_parent_id: Option<ChannelId>,
        /// If the edited message is a reply, the ID of the message being replied to.
        reply_to_message_id: Option<MessageId>,
    },
    MessageDelete {
        chat_id: ChannelId,
        message_id: MessageId,
        /// If the delete was in a thread, the parent channel ID.
        thread_parent_id: Option<ChannelId>,
    },
    ConfigError {
        error: String,
    },
}

// ── Handler struct ────────────────────────────────────────────────────────────

/// Serenity event handler — bridges Discord gateway events to the MCP layer.
pub struct Handler {
    pub state: crate::state::State,
    pub queue: Arc<tokio::sync::Mutex<crate::queue::AccessQueue>>,
    pub tx: tokio::sync::mpsc::Sender<NotificationEvent>,
    pub state_dir: camino::Utf8PathBuf,
    pub bot_user_id: AtomicU64,
    /// Receiver for gateway-level commands from MCP tools (e.g. presence updates).
    /// Taken once during `ready()` to spawn the command processing task.
    pub discord_cmd_rx: tokio::sync::Mutex<Option<tokio::sync::mpsc::Receiver<DiscordCommand>>>,
}

impl Handler {
    /// This construct's own user ID, or `None` before `ready()` has populated it.
    fn self_user_id(&self) -> Option<UserId> {
        match self.bot_user_id.load(Ordering::Relaxed) {
            0 => None,
            id => Some(UserId::new(id)),
        }
    }
}

// ── EventHandler impl ─────────────────────────────────────────────────────────

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, ctx: Context, ready: Ready) {
        let id = ready.user.id.get();
        self.bot_user_id.store(id, Ordering::Relaxed);
        tracing::info!(
            user = %ready.user.name,
            id,
            "Discord gateway ready"
        );

        // Take the command receiver and spawn a task that processes gateway
        // commands (e.g. presence updates) using this Context.
        if let Some(rx) = self.discord_cmd_rx.lock().await.take() {
            tokio::spawn(run_discord_commands(ctx, rx));
        }
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let config = crate::config::load_config(&self.state_dir);

        let bot_ctx = BotMessageContext {
            is_bot: msg.author.bot,
            user_id: msg.author.id,
            channel_id: msg.channel_id,
            is_dm: msg.guild_id.is_none(),
            webhook_id: msg.webhook_id.map(|w| w.get()),
            self_id: self.self_user_id(),
            is_edit: false,
        };
        if should_filter_bot_message(&ctx.http, &self.state, &config, &bot_ctx).await {
            return;
        }
        let bot_user_id = self.bot_user_id.load(Ordering::Relaxed);

        {
            let mut state = self.state.write().await;
            state.cache_username(msg.author.id.get(), display_name(&msg));
        }

        let is_dm = msg.guild_id.is_none();

        if is_dm {
            let sender_id = msg.author.id.get();
            let decision = InboundGate::check_dm(&config, sender_id);

            match decision {
                GateDecision::Deliver => {
                    let channel_id = msg.channel_id.get();

                    // Record DM channel mapping.
                    {
                        let mut state = self.state.write().await;
                        state.record_dm_channel(sender_id, channel_id);
                    }

                    let event =
                        build_message_event(&msg, &config, None, MessageTargeting::DirectMessage);
                    if let Err(e) = self.tx.send(event).await {
                        tracing::warn!(error = %e, "failed to send DM notification event");
                    }
                }
                GateDecision::Queue => {
                    let max_pending = config.access_requests.max_pending;
                    let cooldown = std::time::Duration::from_secs(
                        config.access_requests.notify_cooldown_seconds,
                    );
                    let sender_name = display_name(&msg);
                    let request = AccessRequest {
                        user_id: sender_id,
                        username: sender_name.clone(),
                        message_preview: msg.content.clone(),
                        timestamp: chrono::Utc::now(),
                    };

                    let should_notify = {
                        let mut queue = self.queue.lock().await;
                        let added = queue.enqueue(request, max_pending);
                        if added && queue.should_notify_admin(cooldown) {
                            queue.mark_notified();
                            true
                        } else {
                            false
                        }
                    };

                    if should_notify {
                        for &admin_id in &config.admin_ids {
                            notify_admin_dm(
                                &ctx.http,
                                admin_id,
                                &sender_name,
                                sender_id,
                                &msg.content,
                            )
                            .await;
                        }
                    }
                }
                GateDecision::Drop => {
                    tracing::trace!(sender_id = msg.author.id.get(), "DM dropped by gate");
                }
            }
        } else {
            // Guild message.
            let message_mentions: Vec<u64> = msg.mentions.iter().map(|u| u.id.get()).collect();
            let referenced_author_id = msg.referenced_message.as_deref().map(|m| m.author.id.get());
            let mention_kind = MentionDetector::classify(
                bot_user_id,
                &message_mentions,
                &msg.content,
                referenced_author_id,
                config.mention_patterns.as_ref(),
            );

            let channel_id = msg.channel_id.get();

            let resolved =
                resolve_guild_channel(&ctx.http, &self.state, &config, channel_id, false).await;

            let decision = InboundGate::check_guild(
                &config,
                resolved.gate_channel_id,
                msg.author.id.get(),
                mention_kind.is_some(),
            );

            match decision {
                GateDecision::Deliver => {
                    let targeting = mention_kind
                        .map_or(MessageTargeting::Ambient, MessageTargeting::GuildDirected);
                    let event =
                        build_message_event(&msg, &config, resolved.thread_parent_id, targeting);
                    if let Err(e) = self.tx.send(event).await {
                        tracing::warn!(error = %e, "failed to send guild notification event");
                    }
                }
                GateDecision::Queue => {
                    // Guild messages don't queue — this case shouldn't occur from check_guild.
                    tracing::debug!(channel_id, "guild message: unexpected Queue decision");
                }
                GateDecision::Drop => {
                    tracing::trace!(
                        channel_id,
                        sender_id = msg.author.id.get(),
                        "guild message dropped by gate"
                    );
                }
            }
        }
    }

    async fn reaction_add(&self, ctx: Context, reaction: Reaction) {
        let message_id = reaction.message_id.get();
        let channel_id = reaction.channel_id;
        let bot_id = self.bot_user_id.load(Ordering::Relaxed);

        // Discard reactions with no user attribution or from the bot itself
        // before the potentially-expensive message authorship lookup.
        let Some(user_id) = reaction.user_id else {
            return;
        };
        if user_id.get() == bot_id {
            return;
        }

        let cached = {
            let state = self.state.read().await;
            if state.recent_sent_ids.contains(&message_id) {
                Some(true)
            } else if state.non_bot_message_ids.contains(&message_id) {
                Some(false)
            } else {
                None
            }
        };

        match cached {
            Some(true) => {}
            Some(false) => return,
            None => match ctx.http.get_message(channel_id, reaction.message_id).await {
                Ok(msg) if msg.author.id.get() == bot_id => {
                    let mut state = self.state.write().await;
                    state.note_sent(message_id);
                }
                Ok(_) => {
                    let mut state = self.state.write().await;
                    state.note_non_bot(message_id);
                    return;
                }
                Err(e) => {
                    tracing::debug!(
                        message_id,
                        error = %e,
                        "could not verify reaction target authorship"
                    );
                    return;
                }
            },
        }

        let emoji = match &reaction.emoji {
            ReactionType::Unicode(s) => s.clone(),
            ReactionType::Custom { name, id, .. } => {
                name.clone().unwrap_or_else(|| id.get().to_string())
            }
            _ => return,
        };

        let cached_name = {
            let state = self.state.read().await;
            state.user_names.get(&user_id.get()).cloned()
        };

        // When we have no cached display name for the reactor, fall back to
        // their Discord username before defaulting to "dione" (#153). Resolve
        // the username from the event's member payload when present, else via a
        // direct user fetch. Skip the fetch entirely on the common cache-hit
        // path.
        let username = if is_blank(cached_name.as_deref()) {
            match &reaction.member {
                Some(member) => Some(member.user.name.clone()),
                None => ctx.http.get_user(user_id).await.map(|u| u.name).ok(),
            }
        } else {
            None
        };
        let user_name = resolve_user_identity(cached_name.as_deref(), username.as_deref());

        let event = NotificationEvent::Reaction {
            chat_id: channel_id,
            message_id: reaction.message_id,
            user: user_name,
            user_id,
            emoji,
        };

        if let Err(e) = self.tx.send(event).await {
            tracing::warn!(error = %e, "failed to send reaction notification event");
        }
    }

    async fn message_update(
        &self,
        ctx: Context,
        old_if_available: Option<Message>,
        new: Option<Message>,
        event: MessageUpdateEvent,
    ) {
        let Some(edited_ts) = event.edited_timestamp else {
            return;
        };

        let author = event
            .author
            .as_ref()
            .or_else(|| new.as_ref().map(|m| &m.author))
            .or_else(|| old_if_available.as_ref().map(|m| &m.author));
        let Some(author) = author else {
            return;
        };

        let config = crate::config::load_config(&self.state_dir);

        let bot_ctx = BotMessageContext {
            is_bot: author.bot,
            user_id: author.id,
            channel_id: event.channel_id,
            is_dm: event.guild_id.is_none(),
            webhook_id: event.webhook_id.flatten().map(|w| w.get()),
            self_id: self.self_user_id(),
            is_edit: true,
        };
        if should_filter_bot_message(&ctx.http, &self.state, &config, &bot_ctx).await {
            return;
        }

        let new_content = event
            .content
            .or_else(|| new.as_ref().map(|m| m.content.clone()));
        let Some(new_content) = new_content else {
            return;
        };

        let channel_id = event.channel_id.get();

        let is_dm = event.guild_id.is_none();

        let resolved =
            resolve_guild_channel(&ctx.http, &self.state, &config, channel_id, is_dm).await;

        let decision = if is_dm {
            InboundGate::check_dm(&config, author.id.get())
        } else {
            InboundGate::check_guild_passive(&config, resolved.gate_channel_id, author.id.get())
        };
        if !matches!(decision, GateDecision::Deliver) {
            tracing::trace!(
                channel_id,
                sender_id = author.id.get(),
                ?decision,
                "message edit dropped by gate"
            );
            return;
        }

        let sender_name = {
            let mut state = self.state.write().await;
            if is_dm {
                state.record_dm_channel(author.id.get(), channel_id);
            }
            let resolved = new
                .as_ref()
                .map(display_name)
                .unwrap_or_else(|| display_name_from_user(author));
            let sender_name = resolve_user_identity(Some(&resolved), Some(&author.name));
            state.cache_username(author.id.get(), sender_name.clone());
            sender_name
        };

        let timestamp = config.localize_rfc3339(&serenity_ts_to_rfc3339("edited_ts", &edited_ts));

        // message_reference is Option<Option<MessageReference>> in update events:
        // outer Option = field present in update, inner Option = nullable value.
        let reply_to_message_id = event
            .message_reference
            .as_ref()
            .and_then(|outer| outer.as_ref())
            .and_then(reply_to_id);

        let ev = NotificationEvent::MessageEdit {
            chat_id: event.channel_id,
            message_id: event.id,
            user: sender_name,
            user_id: author.id,
            new_content,
            timestamp,
            thread_parent_id: resolved.thread_parent_id.map(ChannelId::new),
            reply_to_message_id,
        };

        if let Err(e) = self.tx.send(ev).await {
            tracing::warn!(error = %e, "failed to send message edit notification");
        }
    }

    async fn message_delete(
        &self,
        ctx: Context,
        channel_id: ChannelId,
        deleted_message_id: MessageId,
        guild_id: Option<GuildId>,
    ) {
        let cid = channel_id.get();
        let mid = deleted_message_id.get();

        {
            let state = self.state.read().await;
            if state.recent_sent_ids.contains(&mid) {
                return;
            }
        }

        let config = crate::config::load_config(&self.state_dir);

        let is_dm = guild_id.is_none();

        let resolved = resolve_guild_channel(&ctx.http, &self.state, &config, cid, is_dm).await;

        let is_known = if is_dm {
            config.access.dm_policy != crate::config::DmPolicy::Disabled && {
                let state = self.state.read().await;
                state.dm_channel_ids.contains(&cid)
            }
        } else {
            config.channel_policy(resolved.gate_channel_id).is_some()
        };
        if !is_known {
            return;
        }

        let ev = NotificationEvent::MessageDelete {
            chat_id: channel_id,
            message_id: deleted_message_id,
            thread_parent_id: resolved.thread_parent_id.map(ChannelId::new),
        };

        if let Err(e) = self.tx.send(ev).await {
            tracing::warn!(error = %e, "failed to send message delete notification");
        }
    }

    async fn interaction_create(&self, ctx: Context, interaction: Interaction) {
        let Interaction::Component(component) = interaction else {
            return;
        };

        let custom_id = &component.data.custom_id;

        // Parse permission response pattern: "perm:allow:{request_id}" or "perm:deny:{request_id}"
        let (granted, request_id) = if let Some(rid) = custom_id.strip_prefix("perm:allow:") {
            (true, rid.to_string())
        } else if let Some(rid) = custom_id.strip_prefix("perm:deny:") {
            (false, rid.to_string())
        } else {
            return;
        };

        let config = crate::config::load_config(&self.state_dir);
        let sender_id = component.user.id.get();
        let is_admin = config.is_admin(sender_id);

        if !is_admin {
            // Respond with ephemeral "not authorized".
            let resp = CreateInteractionResponse::Message(
                CreateInteractionResponseMessage::new()
                    .content("Not authorized.")
                    .ephemeral(true),
            );
            if let Err(e) = component.create_response(&ctx.http, resp).await {
                tracing::warn!(error = %e, "failed to send ephemeral not-authorized response");
            }
            return;
        }

        // Acknowledge the interaction first — Discord requires a response within 3 seconds.
        // Only send the PermissionResponse event if the acknowledgment succeeds.
        let label = if granted { "Allowed" } else { "Denied" };
        let updated_content = format!(
            "Permission request `{request_id}` — **{label}** by <@{}>",
            component.user.id.get()
        );
        let resp = CreateInteractionResponse::UpdateMessage(
            CreateInteractionResponseMessage::new()
                .content(updated_content)
                .components(vec![]),
        );
        if let Err(e) = component.create_response(&ctx.http, resp).await {
            tracing::warn!(error = %e, "failed to update permission button message; not forwarding event");
            return;
        }

        // Remove all pending entries for this request_id (multi-admin: each admin
        // gets a separate DM, so there may be sibling messages to clean up).
        // Removal also re-marks the message IDs as bot-sent, so the gateway
        // message_delete events triggered by the cleanup below are suppressed
        // rather than delivered to the MCP client.
        let siblings = {
            let mut state = self.state.write().await;
            state.remove_permissions_by_request_id(&request_id)
        };

        // Delete all permission DMs — siblings from other admins and the
        // clicked message itself. The interaction response already acknowledged
        // the click, so the message can be cleaned up.
        for (channel_id, msg_id) in &siblings {
            if let Err(e) = ctx.http.delete_message(*channel_id, *msg_id, None).await {
                tracing::warn!(msg_id = msg_id.get(), error = %e, "failed to delete permission DM");
            }
        }

        // Only send the event if we actually owned this request (guard against
        // duplicate clicks after prune already cleared the entries).
        if siblings.is_empty() {
            return;
        }

        let event = NotificationEvent::PermissionResponse {
            request_id: request_id.clone(),
            granted,
        };
        if let Err(e) = self.tx.send(event).await {
            tracing::warn!(error = %e, "failed to send permission response event");
        }
    }
}

// ── Discord command processing ───────────────────────────────────────────────

/// Processes gateway-level commands sent from MCP tools.
///
/// Spawned in `ready()` with a cloned `Context` so it can call
/// `ctx.set_presence()` and other gateway operations that require the
/// shard messenger.
async fn run_discord_commands(ctx: Context, mut rx: tokio::sync::mpsc::Receiver<DiscordCommand>) {
    tracing::debug!("discord command processor started");
    while let Some(cmd) = rx.recv().await {
        match cmd {
            DiscordCommand::SetPresence {
                online_status,
                activity_type,
                activity_name,
            } => {
                let status = online_status.to_serenity();
                let activity = match (activity_type, activity_name.as_deref()) {
                    (Some(kind), Some(name)) => Some(kind.to_activity(name)),
                    _ => None,
                };
                tracing::info!(
                    ?status,
                    ?activity_type,
                    activity_name = activity_name.as_deref(),
                    "setting presence"
                );
                ctx.set_presence(activity, status);
            }
        }
    }
    tracing::debug!("discord command processor stopped (channel closed)");
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Result of resolving a channel's thread parentage and computing the
/// effective channel ID for gate decisions.
struct ResolvedChannel {
    /// If the message was in a thread, the parent channel ID.
    thread_parent_id: Option<u64>,
    /// The channel ID to use for gate decisions: parent if this is a thread,
    /// otherwise the original channel ID.
    gate_channel_id: u64,
}

/// Resolves thread parentage for a channel and returns the effective gate
/// channel ID.
///
/// If the channel is directly configured, it is used as-is (no thread
/// resolution needed). Otherwise, we check whether the channel is a thread
/// and, if so, use its parent for gate decisions.
///
/// For DM channels (`is_dm = true`), thread resolution is skipped entirely.
async fn resolve_guild_channel(
    http: &serenity::http::Http,
    state: &crate::state::State,
    config: &crate::config::LoadedConfig,
    channel_id: u64,
    is_dm: bool,
) -> ResolvedChannel {
    let thread_parent_id = if !is_dm && config.channel_policy(channel_id).is_none() {
        resolve_thread_parent(http, state, channel_id).await
    } else {
        None
    };
    let gate_channel_id = thread_parent_id.unwrap_or(channel_id);
    ResolvedChannel {
        thread_parent_id,
        gate_channel_id,
    }
}

/// Converts a serenity [`Timestamp`] to an RFC 3339 string.
///
/// If `to_rfc3339()` returns `None` — which indicates the timestamp is broken
/// at the Discord API level — logs a warning and falls back to the current UTC
/// time so callers never receive an empty string.
fn serenity_ts_to_rfc3339(field: &str, ts: &serenity::model::Timestamp) -> String {
    match ts.to_rfc3339() {
        Some(s) => s,
        None => {
            let fallback = chrono::Utc::now().to_rfc3339();
            tracing::warn!(
                field,
                fallback = %fallback,
                "Discord timestamp failed to_rfc3339(); using current UTC time as fallback"
            );
            fallback
        }
    }
}

/// True when the reference denotes a genuine reply (not a forward/crosspost).
fn is_reply_reference(reference: &MessageReference) -> bool {
    matches!(reference.kind, MessageReferenceKind::Default)
}

/// Extracts the replied-to message ID from a Discord message reference.
///
/// A reference can exist without a message ID (for example a channel-only
/// forward or crosspost), so the inner `message_id` is itself optional.
fn reply_to_id(reference: &MessageReference) -> Option<MessageId> {
    is_reply_reference(reference)
        .then_some(reference.message_id)
        .flatten()
}

/// Max characters retained from a replied-to message preview.
const REPLY_PREVIEW_MAX_CHARS: usize = 100;

/// Best-effort extraction of reply author + content preview from the parent
/// message embedded by Discord. Returns `(user_id, user, content_preview)`,
/// each independently optional. Only populated for genuine replies
/// (`MessageReferenceKind::Default`); forwards/crossposts yield all `None`.
fn reply_context(msg: &Message) -> (Option<UserId>, Option<String>, Option<String>) {
    let is_reply = msg
        .message_reference
        .as_ref()
        .is_some_and(is_reply_reference);
    if !is_reply {
        return (None, None, None);
    }
    let Some(parent) = msg.referenced_message.as_deref() else {
        return (None, None, None);
    };
    let preview = reply_preview(&parent.content);
    (Some(parent.author.id), Some(display_name(parent)), preview)
}

/// Resolves the best available display name for a message author.
///
/// Priority: server nickname > global display name > username.
/// For webhook messages (e.g. PluralKit), `author.name` is already the
/// alter's display name, so the fallback is correct.
fn display_name(msg: &Message) -> String {
    let raw = msg
        .member
        .as_ref()
        .and_then(|m| m.nick.as_ref())
        .or(msg.author.global_name.as_ref())
        .unwrap_or(&msg.author.name);
    strip_invisible(raw)
}

/// Display name from a bare `User` (no guild member context).
/// Falls back global_name > username.
fn display_name_from_user(user: &serenity::model::user::User) -> String {
    let raw = user.global_name.as_ref().unwrap_or(&user.name);
    strip_invisible(raw)
}

/// Literal identity used for an outbound notification's `user` field when
/// neither a resolved display name nor a Discord username is available.
const FALLBACK_USER_IDENTITY: &str = "dione";

/// Returns `true` when the candidate is absent or whitespace-only.
fn is_blank(candidate: Option<&str>) -> bool {
    candidate.map(str::trim).unwrap_or("").is_empty()
}

/// Resolves the `user` field for an outbound notification.
///
/// Fallback chain (see #153): resolved display name → Discord username →
/// the literal `"dione"`. Blank or whitespace-only candidates are treated as
/// absent so an empty display name never leaks into a notification (which
/// would otherwise surface as the substrate name "dione" downstream).
fn resolve_user_identity(display_name: Option<&str>, username: Option<&str>) -> String {
    [display_name, username]
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .unwrap_or(FALLBACK_USER_IDENTITY)
        .to_string()
}

/// Strip invisible/zero-width characters that proxy bots (e.g. PluralKit)
/// pad onto short names to prevent Discord formatting issues.
fn strip_invisible(s: &str) -> String {
    s.chars()
        .filter(|c| {
            !matches!(
                *c,
                '\u{200B}'  // Zero Width Space
                | '\u{200C}' // Zero Width Non-Joiner
                | '\u{200D}' // Zero Width Joiner
                | '\u{FEFF}' // Zero Width No-Break Space
                | '\u{17B5}' // Khmer Vowel Inherent AA (PluralKit anti-ping)
                | '\u{034F}' // Combining Grapheme Joiner
            )
        })
        .collect()
}

/// UTF-8-safe truncation to `REPLY_PREVIEW_MAX_CHARS` chars, with an ellipsis
/// when truncated. Returns `None` for empty content (e.g. attachment-only parent).
fn reply_preview(content: &str) -> Option<String> {
    if content.is_empty() {
        return None;
    }
    let mut chars = content.chars();
    let preview: String = chars.by_ref().take(REPLY_PREVIEW_MAX_CHARS).collect();
    if chars.next().is_some() {
        Some(format!("{preview}\u{2026}"))
    } else {
        Some(preview)
    }
}

fn build_message_event(
    msg: &Message,
    config: &crate::config::LoadedConfig,
    thread_parent_id: Option<u64>,
    targeting: MessageTargeting,
) -> NotificationEvent {
    let attachments = msg
        .attachments
        .iter()
        .map(|a| AttachmentMeta {
            name: a.filename.clone(),
            content_type: a.content_type.clone(),
            size: u64::from(a.size),
        })
        .collect();

    let is_voice_message = msg
        .flags
        .map(|f| f.contains(MessageFlags::IS_VOICE_MESSAGE))
        .unwrap_or(false);

    let reply_to_message_id = msg.message_reference.as_ref().and_then(reply_to_id);
    let (reply_to_user_id, reply_to_user, reply_to_content_preview) = reply_context(msg);

    NotificationEvent::Message(MessageEvent {
        chat_id: msg.channel_id,
        message_id: msg.id,
        user: resolve_user_identity(Some(&display_name(msg)), Some(&msg.author.name)),
        user_id: msg.author.id,
        content: msg.content.clone(),
        targeting,
        timestamp: config
            .localize_rfc3339(&serenity_ts_to_rfc3339("msg.timestamp", &msg.timestamp)),
        attachments,
        is_voice_message,
        thread_parent_id: thread_parent_id.map(ChannelId::new),
        reply_to_message_id,
        reply_to_user_id,
        reply_to_user,
        reply_to_content_preview,
        bells: None,
        bells_status: None,
    })
}

/// Resolves the parent channel ID for a thread channel.
///
/// First checks the in-memory cache (including negative entries for channels
/// confirmed not to be threads), then falls back to an API call. Caches
/// the result either way. Returns `None` if the channel is not a thread.
async fn resolve_thread_parent(
    http: &serenity::http::Http,
    state: &crate::state::State,
    channel_id: u64,
) -> Option<u64> {
    // Check cache first — includes negative entries (Some(None)).
    {
        let state = state.read().await;
        if let Some(cached) = state.thread_parents.get(&channel_id) {
            return *cached;
        }
    }

    // Not cached — ask Discord.
    let channel = match http.get_channel(ChannelId::new(channel_id)).await {
        Ok(ch) => ch,
        Err(e) => {
            tracing::debug!(channel_id, error = %e, "failed to look up channel for thread detection");
            return None;
        }
    };

    let parent_id = match channel {
        serenity::model::channel::Channel::Guild(gc)
            if matches!(
                gc.kind,
                ChannelType::PublicThread | ChannelType::PrivateThread | ChannelType::NewsThread
            ) =>
        {
            gc.parent_id.map(|p| p.get())
        }
        _ => None,
    };

    // Cache the result — including None for non-threads (negative cache).
    {
        let mut state = state.write().await;
        state.record_thread_parent(channel_id, parent_id);
    }

    parent_id
}

/// Sends a DM to an admin about a pending access request.
async fn notify_admin_dm(
    http: &serenity::http::Http,
    admin_id: u64,
    requester_name: &str,
    requester_id: u64,
    message_preview: &str,
) {
    // Create DM channel with admin.
    let Some(admin_uid) = crate::mcp::ids::Snowflake::new(admin_id) else {
        tracing::warn!(admin_id, "skipping admin DM notification: admin_id is zero");
        return;
    };
    let channel = match create_dm_channel(http, admin_uid.user()).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(
                admin_id,
                error = %e,
                "failed to open DM with admin for access notification"
            );
            return;
        }
    };

    let preview: String = message_preview.chars().take(100).collect();
    let content = format!(
        "**Access request** from `{requester_name}` (<@{requester_id}>)\n\
         Preview: {preview}\n\
         Use the `list_access_requests` MCP tool to review and approve/deny."
    );

    let msg = CreateMessage::new().content(content);
    if let Err(e) = channel.id.send_message(http, msg).await {
        tracing::warn!(admin_id, error = %e, "failed to send access notification DM");
    }
}

/// Returns `true` if the message should be dropped because the author is a bot
/// whose user ID is **not** in the `allow_from` list.
///
/// Human messages (is_bot = false) always pass through. Bot messages pass
/// through only if the bot's user ID is in the config's allow_from list.
fn should_drop_bot_message(
    is_bot: bool,
    user_id: u64,
    config: &crate::config::LoadedConfig,
) -> bool {
    is_bot && !config.is_allowed(user_id)
}

/// How long to suppress repeat "dropped bot message" warnings for the same
/// (bot user ID, channel ID) pair.
const DROPPED_BOT_WARN_COOLDOWN: Duration = Duration::from_secs(3600);

/// The inbound message facts the bot filter needs, gathered at the gateway.
///
/// These arrive as a struct rather than a run of positional arguments because
/// several are same-typed IDs: a transposed `user_id` and `channel_id` would
/// compile cleanly and produce a warning naming the channel as the sender.
pub(crate) struct BotMessageContext {
    /// Whether Discord marked the author as a bot.
    pub is_bot: bool,
    /// The author's ID. For webhook-authored messages this is the webhook's ID.
    pub user_id: UserId,
    /// The channel the message arrived in.
    pub channel_id: ChannelId,
    /// Whether the message arrived in a DM rather than a guild channel.
    pub is_dm: bool,
    /// The webhook that authored the message, if any.
    pub webhook_id: Option<u64>,
    /// This construct's own user ID, or `None` before `ready()` has fired.
    pub self_id: Option<UserId>,
    /// Whether this came from `message_update` rather than `message`. Edits gate
    /// through [`InboundGate::check_guild_passive`], which enforces fewer rules.
    pub is_edit: bool,
}

/// Returns `true` if this bot message should be filtered (dropped).
/// Proxy bot webhooks (e.g. PluralKit) are allowed through.
///
/// Dropping is silent to Discord but not to the operator: drops are reported
/// through [`warn_dropped_bot_message`], which decides which of them are worth
/// a warning.
async fn should_filter_bot_message(
    http: &serenity::http::Http,
    state: &crate::state::State,
    config: &crate::config::LoadedConfig,
    ctx: &BotMessageContext,
) -> bool {
    if !should_drop_bot_message(ctx.is_bot, ctx.user_id.get(), config) {
        return false;
    }
    let filtered = match ctx.webhook_id {
        Some(wh_id) => !is_proxy_webhook(http, state, wh_id).await,
        None => true,
    };
    if filtered {
        warn_dropped_bot_message(http, state, config, ctx).await;
    }
    filtered
}

/// Logs that a bot's message was dropped for not being in the allowlist, and
/// how to let it through.
///
/// The warning is deliberately narrow. It fires only where adding the sender to
/// `[access].allow_from` genuinely moves the message forward, because a warning
/// that names the wrong remedy costs more diagnosis time than silence does —
/// that misdirection is the whole of #228. Three classes of drop are therefore
/// reported at `debug` instead:
///
/// - **Our own gateway echoes.** Every message this construct sends comes back
///   over `MESSAGE_CREATE` and is dropped by this same filter. Following the
///   remediation would make the construct ingest its own output. An unknown
///   self-ID counts as possibly-us, so the startup window before `ready()` fails
///   closed rather than accusing ourselves.
/// - **Webhook-authored messages.** Discord reports the webhook's ID as the
///   author rather than a bot user's, so the advice would name an ID that is not
///   stable across webhook recreation. Bridges that keep a stable webhook per
///   channel are a real exception, tracked in #228.
/// - **Channels this construct was never configured to read.** The guild gate
///   would drop those regardless of the allowlist, so `allow_from` is not what
///   is standing in the way.
///
/// Where the channel keeps gates of its own, the warning names them rather than
/// claiming the global list is the whole story — see [`ChannelRemedy`].
///
/// What survives is throttled per (bot user ID, channel ID) pair — see
/// [`DROPPED_BOT_WARN_COOLDOWN`] — so a chatty peer bot in a busy channel
/// cannot turn the warning into a flood.
async fn warn_dropped_bot_message(
    http: &serenity::http::Http,
    state: &crate::state::State,
    config: &crate::config::LoadedConfig,
    ctx: &BotMessageContext,
) {
    let user_id = ctx.user_id.get();
    let channel_id = ctx.channel_id.get();

    // Unthrottled, so raising the log level always reconstructs the full drop
    // history even where the warning stays quiet.
    // Keyed under the same field name as the warning, so one query correlates
    // the full drop history with the warnings raised from it.
    tracing::debug!(
        bot_user_id = user_id,
        channel_id,
        webhook = ctx.webhook_id.is_some(),
        "dropped bot message"
    );

    if ctx.self_id.is_none_or(|self_id| self_id == ctx.user_id) || ctx.webhook_id.is_some() {
        return;
    }

    // One diagnosis attempt per (bot, channel) pair per cooldown. The read
    // check first, so the common suppressed case never takes the write lock.
    let now = Instant::now();
    {
        let state = state.read().await;
        if state.dropped_bot_warning_suppressed(user_id, channel_id, now, DROPPED_BOT_WARN_COOLDOWN)
        {
            return;
        }
    }
    // Claim before resolving the channel. This bounds the Discord calls the
    // resolution can make, not just the log lines: `resolve_thread_parent`
    // caches a confirmed non-thread but *not* a failed lookup, so resolving
    // first would re-issue `get_channel` for every dropped message from a
    // channel whose lookup keeps erroring. A drop that turns out not to be
    // worth warning about therefore spends a slot, which is the intended
    // trade — it is one attempt per pair per hour either way.
    let claimed = {
        let mut state = state.write().await;
        state.claim_dropped_bot_warning(user_id, channel_id, now, DROPPED_BOT_WARN_COOLDOWN)
    };
    if !claimed {
        return;
    }

    let Some(remedy) = channel_remedy(http, state, config, ctx).await else {
        return;
    };

    // One static literal per case rather than an assembled string, so each
    // stays greppable and none of them interpolates.
    match remedy.channel_allowlist {
        ChannelAllowlist::AlsoBlocks => tracing::warn!(
            bot_user_id = user_id,
            channel_id,
            config_channel_id = remedy.config_channel_id,
            mention_required = remedy.mention_required,
            "dropped message from bot not in the allowlist — add this bot_user_id to the \
             global [access].allow_from in config.toml AND to the allow_from of the \
             [[channels]] entry for config_channel_id; each gates this sender separately"
        ),
        ChannelAllowlist::AlreadyAdmits => tracing::warn!(
            bot_user_id = user_id,
            channel_id,
            config_channel_id = remedy.config_channel_id,
            mention_required = remedy.mention_required,
            "dropped message from bot not in the allowlist — the [[channels]] entry for \
             config_channel_id already lists this bot_user_id; the global \
             [access].allow_from in config.toml is the one still missing it"
        ),
        ChannelAllowlist::Absent => tracing::warn!(
            bot_user_id = user_id,
            channel_id,
            config_channel_id = remedy.config_channel_id,
            mention_required = remedy.mention_required,
            "dropped message from bot not in the allowlist — add this bot_user_id to the \
             global [access].allow_from in config.toml; no per-channel allow_from stands \
             in this sender's way"
        ),
    }

    // Emitted separately so the allowlist advice above stays a single literal
    // per case instead of doubling into a mention-aware variant of each. Both
    // lines are covered by the one claim, so the pair still yields one
    // diagnosis per cooldown.
    if remedy.mention_required {
        tracing::warn!(
            bot_user_id = user_id,
            channel_id,
            config_channel_id = remedy.config_channel_id,
            "the same channel also gates on require_mention, so allowlisting alone will \
             surface only this bot's messages that mention this construct, not its \
             ambient chatter"
        );
    }
}

/// Whether a channel's own `allow_from` stands between a sender and delivery.
///
/// Three states rather than a boolean because "does not block" has two causes
/// that need different advice: no list at all, and a list this sender is
/// already on. Collapsing them made the warning assert the channel kept no
/// `allow_from` while the operator was looking at one.
enum ChannelAllowlist {
    /// The channel keeps no `allow_from` of its own.
    Absent,
    /// It keeps one, and this sender is already on it.
    AlreadyAdmits,
    /// It keeps one that omits this sender, so that list needs the ID too.
    AlsoBlocks,
}

/// The channel-level gates that still apply to a sender once the global
/// allowlist admits it.
///
/// The global `[access].allow_from` list is never the only thing between a bot
/// and delivery, and #228 was made expensive by exactly that assumption. These
/// fields let the warning say what else is in play, and name the configuration
/// entry to edit, instead of implying the global list is the whole story.
struct ChannelRemedy {
    /// How the channel's own `allow_from` treats this sender.
    channel_allowlist: ChannelAllowlist,
    /// The channel whose `[[channels]]` entry holds the policy in force. For a
    /// message in a thread this is the parent, which is the only one of the two
    /// IDs that appears in `config.toml`.
    config_channel_id: u64,
    /// Whether this message needed to mention the construct to get through.
    /// False on the edit path, which gates through
    /// [`InboundGate::check_guild_passive`] and does not enforce mentions.
    mention_required: bool,
}

/// Returns what still stands between this sender and delivery, or `None` when
/// the global allowlist is not the blocker at all.
///
/// Mirrors the configuration half of [`InboundGate::check_guild`] — channel
/// opt-in and the channel's own `allow_from` — so the warning fires only where
/// the global list is genuinely part of the answer. `require_mention` is
/// reported rather than suppressed: it turns on individual messages, so the
/// allowlist is still the right first edit.
///
/// Thread parents resolve through [`resolve_thread_parent`], which caches a
/// confirmed non-thread but not a failed lookup — the caller claims the throttle
/// first so that a failing lookup cannot be retried per message.
async fn channel_remedy(
    http: &serenity::http::Http,
    state: &crate::state::State,
    config: &crate::config::LoadedConfig,
    ctx: &BotMessageContext,
) -> Option<ChannelRemedy> {
    if ctx.is_dm {
        return (config.access.dm_policy != crate::config::DmPolicy::Disabled).then(|| {
            ChannelRemedy {
                channel_allowlist: ChannelAllowlist::Absent,
                config_channel_id: ctx.channel_id.get(),
                mention_required: false,
            }
        });
    }

    let channel_id = ctx.channel_id.get();
    let config_channel_id = if config.channel_policy(channel_id).is_some() {
        channel_id
    } else {
        resolve_thread_parent(http, state, channel_id).await?
    };
    let policy = config.channel_policy(config_channel_id)?;

    let channel_allowlist = if policy.allow_from.is_empty() {
        ChannelAllowlist::Absent
    } else if policy.allow_from.contains(&ctx.user_id.get()) {
        ChannelAllowlist::AlreadyAdmits
    } else {
        ChannelAllowlist::AlsoBlocks
    };

    Some(ChannelRemedy {
        channel_allowlist,
        config_channel_id,
        // Edits gate through `check_guild_passive`, which does not enforce
        // mentions, so reporting the channel's setting there would name a gate
        // that did not apply to this message.
        mention_required: policy.require_mention && !ctx.is_edit,
    })
}

/// Bot user IDs of known proxy bots (e.g. PluralKit) whose webhook messages
/// should be treated as human-authored.
const PROXY_BOT_IDS: &[u64] = &[
    466378653216014359, // PluralKit
];

/// Checks whether a webhook was created by a known proxy bot.
///
/// First checks the in-memory cache, then falls back to a Discord API call
/// to fetch the webhook's creator. Caches the result either way. PluralKit
/// reuses webhooks per-channel, so the cache is very effective — typically
/// one API call per channel, ever.
async fn is_proxy_webhook(
    http: &serenity::http::Http,
    state: &crate::state::State,
    webhook_id: u64,
) -> bool {
    // Check cache first.
    {
        let state = state.read().await;
        if let Some(&is_proxy) = state.proxy_webhooks.get(&webhook_id) {
            return is_proxy;
        }
    }

    // Cache miss — ask Discord for the webhook's creator.
    match http.get_webhook(WebhookId::new(webhook_id)).await {
        Ok(webhook) => {
            let is_proxy = webhook
                .user
                .as_ref()
                .is_some_and(|u| PROXY_BOT_IDS.contains(&u.id.get()));
            let mut state = state.write().await;
            state.record_proxy_webhook(webhook_id, is_proxy);
            is_proxy
        }
        Err(e) => {
            tracing::debug!(
                webhook_id,
                error = %e,
                "failed to look up webhook for proxy bot detection — not caching"
            );
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AccessConfig, Config, DmPolicy, LoadedConfig};

    fn config_with_allow_from(ids: Vec<&str>) -> LoadedConfig {
        let raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: ids.into_iter().map(String::from).collect(),
                admins: vec![],
                admin_only_mutations: false,
            },
            ..Default::default()
        };
        LoadedConfig::from_raw(raw)
    }

    // ── Bot message filter tests ─────────────────────────────────────────────

    /// Bot whose user ID is NOT in allow_from must be dropped.
    #[test]
    fn test_bot_not_in_allow_from_is_dropped() {
        let config = config_with_allow_from(vec!["100"]);
        assert!(
            should_drop_bot_message(true, 999, &config),
            "bot user 999 is not in allow_from and must be dropped"
        );
    }

    /// Bot whose user ID IS in allow_from must pass through.
    #[test]
    fn test_bot_in_allow_from_passes_through() {
        let config = config_with_allow_from(vec!["100", "200"]);
        assert!(
            !should_drop_bot_message(true, 200, &config),
            "bot user 200 is in allow_from and must not be dropped"
        );
    }

    /// Human message (is_bot = false) is never dropped, regardless of allow_from.
    #[test]
    fn test_human_message_not_dropped() {
        let config = config_with_allow_from(vec!["100"]);
        // Human in allow_from.
        assert!(
            !should_drop_bot_message(false, 100, &config),
            "human user in allow_from must not be dropped"
        );
        // Human NOT in allow_from.
        assert!(
            !should_drop_bot_message(false, 999, &config),
            "human user not in allow_from must not be dropped by bot filter"
        );
    }

    /// Bot with empty allow_from list is always dropped.
    #[test]
    fn test_bot_with_empty_allow_from_is_dropped() {
        let config = config_with_allow_from(vec![]);
        assert!(
            should_drop_bot_message(true, 42, &config),
            "bot must be dropped when allow_from is empty"
        );
    }

    // ── dropped bot warning tests ──────────────────────────────────────────────

    /// Target used by the harness canary. Distinct from any production target,
    /// so a canary can never be mistaken for a real warning.
    const CANARY_TARGET: &str = "dione_test_canary";

    /// A `warn`-level event captured from the tracing pipeline.
    struct CapturedWarning {
        target: String,
        message: String,
        fields: Vec<(String, String)>,
    }

    impl CapturedWarning {
        /// Returns the value of a structured field, if the event carried it.
        fn field(&self, name: &str) -> Option<&str> {
            self.fields
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
        }

        /// The `(bot_user_id, channel_id)` pair the warning names.
        fn pair(&self) -> (Option<&str>, Option<&str>) {
            (self.field("bot_user_id"), self.field("channel_id"))
        }
    }

    /// Installs a thread-local tracing subscriber, runs `f`, and returns the
    /// `warn`-level events it emitted — excluding the canary described below.
    ///
    /// `DefaultGuard` is `!Send`, which makes this future `!Send` and therefore
    /// unspawnable; `block_on` drives it on the thread holding the guard under
    /// either runtime flavor, so capture cannot silently detach.
    ///
    /// A canary `warn` is emitted at the end of every capture and asserted on.
    /// Without it, a harness that captured nothing would make every
    /// "does not warn" test pass for the wrong reason — the tests would still
    /// be green with the feature deleted, which is exactly the failure this
    /// suite is meant to rule out.
    async fn capture_warnings<F, Fut>(f: F) -> Vec<CapturedWarning>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = ()>,
    {
        use tracing_subscriber::prelude::*;

        let (layer, mut rx) = crate::tracing_channel::TracingChannelLayer::new();
        {
            let _guard =
                tracing::subscriber::set_default(tracing_subscriber::registry().with(layer));
            f().await;
            tracing::warn!(target: "dione_test_canary", "canary");
        }

        let mut warnings = Vec::new();
        while let Ok(event) = rx.try_recv() {
            if let NotificationEvent::Trace {
                level,
                target,
                message,
                fields,
            } = event
                && level == "WARN"
            {
                warnings.push(CapturedWarning {
                    target,
                    message,
                    fields,
                });
            }
        }

        let canaries = warnings
            .iter()
            .filter(|w| w.target == CANARY_TARGET)
            .count();
        assert_eq!(
            canaries, 1,
            "capture harness is broken: expected exactly 1 canary warning, saw {canaries}"
        );
        warnings.retain(|w| w.target != CANARY_TARGET);
        warnings
    }

    /// A config with one guild channel and `100` globally allowlisted.
    fn config_with_channel(channel: crate::config::ChannelConfig) -> LoadedConfig {
        LoadedConfig::from_raw(Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec!["100".to_string()],
                admins: vec![],
                admin_only_mutations: false,
            },
            channels: vec![channel],
            ..Default::default()
        })
    }

    /// A watched channel that does not gate on mentions, so the allowlist
    /// warning stands alone. Mention-gated channels are covered separately.
    fn watched_config() -> LoadedConfig {
        config_with_channel(crate::config::ChannelConfig {
            id: "12345".to_string(),
            require_mention: false,
            ..Default::default()
        })
    }

    /// A bot-authored message in the watched channel from a non-allowlisted
    /// sender — the case the warning exists for.
    fn dropped_bot_ctx() -> BotMessageContext {
        BotMessageContext {
            is_bot: true,
            user_id: UserId::new(999),
            channel_id: ChannelId::new(12345),
            is_dm: false,
            webhook_id: None,
            self_id: Some(UserId::new(7)),
            is_edit: false,
        }
    }

    /// Runs the bot filter against a fresh state, reporting its decision and
    /// the warnings it emitted.
    async fn filter_and_capture(
        config: &LoadedConfig,
        ctx: &BotMessageContext,
    ) -> (bool, Vec<CapturedWarning>) {
        filter_with_state(&crate::state::new_state(), config, ctx).await
    }

    /// As [`filter_and_capture`], against a caller-supplied state so the
    /// proxy-webhook cache can be pre-seeded.
    ///
    /// The HTTP client is only reachable on a `webhook_id` cache miss; every
    /// caller here either passes `None` or seeds the cache first.
    async fn filter_with_state(
        state: &crate::state::State,
        config: &LoadedConfig,
        ctx: &BotMessageContext,
    ) -> (bool, Vec<CapturedWarning>) {
        let http = serenity::http::Http::new("fake");
        let mut filtered = false;
        let warnings = capture_warnings(|| async {
            filtered = should_filter_bot_message(&http, state, config, ctx).await;
        })
        .await;
        (filtered, warnings)
    }

    /// Dropping a non-allowlisted bot emits a warning naming the bot, the
    /// channel, the config entry to edit, and which allowlists are in play.
    #[tokio::test]
    async fn test_dropped_bot_message_warns_with_remediation() {
        let (filtered, warnings) = filter_and_capture(&watched_config(), &dropped_bot_ctx()).await;

        assert!(filtered, "non-allowlisted bot must still be dropped");
        assert_eq!(
            warnings.len(),
            1,
            "exactly one warning expected, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
        let warning = &warnings[0];
        assert_eq!(
            warning.field("bot_user_id"),
            Some("999"),
            "warning must name the sending bot's user ID"
        );
        assert_eq!(
            warning.field("channel_id"),
            Some("12345"),
            "warning must name the channel the message was dropped in"
        );
        assert_eq!(
            warning.field("config_channel_id"),
            Some("12345"),
            "warning must name the channel whose config entry holds the policy"
        );
        assert!(
            warning.message.contains("[access].allow_from"),
            "warning must name the global allowlist key, got: {}",
            warning.message
        );
    }

    /// With no per-channel `allow_from`, the warning may say so — but only
    /// because it is true. The two states that look alike from the outside are
    /// pinned apart by the two tests below.
    #[tokio::test]
    async fn test_absent_channel_allowlist_is_reported_as_absent() {
        let (_, warnings) = filter_and_capture(&watched_config(), &dropped_bot_ctx()).await;

        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].message.contains("no per-channel allow_from"),
            "warning should say no per-channel list is in the way, got: {}",
            warnings[0].message
        );
    }

    /// Where the channel keeps its own `allow_from` that omits the bot, the
    /// global list alone will not deliver the message. Saying otherwise is the
    /// wrong-config-layer misdirection that #228 is about.
    #[tokio::test]
    async fn test_channel_allowlist_blocker_is_named() {
        let config = config_with_channel(crate::config::ChannelConfig {
            id: "12345".to_string(),
            require_mention: false,
            allow_from: vec!["4242".to_string()],
            ..Default::default()
        });
        let (filtered, warnings) = filter_and_capture(&config, &dropped_bot_ctx()).await;

        assert!(filtered);
        assert_eq!(warnings.len(), 1);
        assert!(
            warnings[0].message.contains("AND to the allow_from"),
            "warning must name the per-channel allow_from as a second required edit, got: {}",
            warnings[0].message
        );
        assert!(
            !warnings[0].message.contains("no per-channel allow_from"),
            "warning must not deny a list that is blocking, got: {}",
            warnings[0].message
        );
    }

    /// A sender already on the channel's own `allow_from` needs only the global
    /// list. The warning must neither demand an edit to a list the bot is
    /// already in, nor claim the channel keeps no list — the operator is
    /// looking straight at one.
    #[tokio::test]
    async fn test_channel_allowlist_containing_bot_is_reported_as_satisfied() {
        let config = config_with_channel(crate::config::ChannelConfig {
            id: "12345".to_string(),
            require_mention: false,
            allow_from: vec!["999".to_string()],
            ..Default::default()
        });
        let (_, warnings) = filter_and_capture(&config, &dropped_bot_ctx()).await;

        assert_eq!(warnings.len(), 1);
        let message = &warnings[0].message;
        assert!(
            message.contains("already lists"),
            "warning must credit the channel list the bot is already on, got: {message}"
        );
        assert!(
            !message.contains("no per-channel allow_from"),
            "warning must not deny a list that exists, got: {message}"
        );
        assert!(
            !message.contains("AND to the allow_from"),
            "must not demand an edit to a list the bot is already in, got: {message}"
        );
    }

    /// `require_mention` defaults to true and gates individual messages rather
    /// than the sender, so allowlisting alone will not surface a bot's ambient
    /// chatter. That gets its own line rather than being left to a bare field.
    #[tokio::test]
    async fn test_mention_gated_channel_adds_a_mention_notice() {
        let config = config_with_channel(crate::config::ChannelConfig {
            id: "12345".to_string(),
            ..Default::default()
        });
        let (_, warnings) = filter_and_capture(&config, &dropped_bot_ctx()).await;

        assert_eq!(
            warnings.len(),
            2,
            "a mention-gated channel warns about the allowlist and the mention gate, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
        assert_eq!(
            warnings[0].field("mention_required"),
            Some("true"),
            "the mention gate must be reported from config, not hardcoded"
        );
        assert!(
            warnings[1].message.contains("require_mention"),
            "the second line must explain the mention gate, got: {}",
            warnings[1].message
        );
    }

    /// The mention notice must track config rather than always appearing.
    #[tokio::test]
    async fn test_unmentioned_channel_has_no_mention_notice() {
        let (_, warnings) = filter_and_capture(&watched_config(), &dropped_bot_ctx()).await;

        assert_eq!(warnings.len(), 1, "no mention notice when the gate is off");
        assert_eq!(warnings[0].field("mention_required"), Some("false"));
    }

    /// Edits gate through `check_guild_passive`, which does not enforce
    /// mentions, so naming that gate on the edit path would send the operator
    /// after a rule that did not apply to the message they are chasing.
    #[tokio::test]
    async fn test_edit_path_does_not_report_the_mention_gate() {
        let config = config_with_channel(crate::config::ChannelConfig {
            id: "12345".to_string(),
            ..Default::default()
        });
        let ctx = BotMessageContext {
            is_edit: true,
            ..dropped_bot_ctx()
        };
        let (_, warnings) = filter_and_capture(&config, &ctx).await;

        assert_eq!(
            warnings.len(),
            1,
            "the edit path must not add a mention notice, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
        assert_eq!(
            warnings[0].field("mention_required"),
            Some("false"),
            "require_mention is not enforced on edits and must not be claimed"
        );
    }

    /// A bot DM is droppable and allowlistable, so it warns.
    #[tokio::test]
    async fn test_bot_dm_warns() {
        let ctx = BotMessageContext {
            is_dm: true,
            channel_id: ChannelId::new(4000),
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_and_capture(&watched_config(), &ctx).await;

        assert!(filtered);
        assert_eq!(warnings.len(), 1, "a bot DM must warn like a guild message");
        assert_eq!(warnings[0].field("channel_id"), Some("4000"));
    }

    /// With DMs disabled the message goes nowhere regardless of the allowlist,
    /// so the allowlist is not the fix.
    #[tokio::test]
    async fn test_bot_dm_with_dms_disabled_does_not_warn() {
        let raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Disabled,
                allow_from: vec!["100".to_string()],
                admins: vec![],
                admin_only_mutations: false,
            },
            ..Default::default()
        };
        let config = LoadedConfig::from_raw(raw);
        let ctx = BotMessageContext {
            is_dm: true,
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_and_capture(&config, &ctx).await;

        assert!(filtered);
        assert!(
            warnings.is_empty(),
            "must not advise allowlisting when DMs are disabled outright, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// A thread whose cached parent is not configured must stay quiet, like any
    /// other unwatched channel.
    #[tokio::test]
    async fn test_thread_of_unwatched_channel_does_not_warn() {
        let state = crate::state::new_state();
        state.write().await.record_thread_parent(777, Some(55555));
        let ctx = BotMessageContext {
            channel_id: ChannelId::new(777),
            ..dropped_bot_ctx()
        };
        let (_, warnings) = filter_with_state(&state, &watched_config(), &ctx).await;

        assert!(
            warnings.is_empty(),
            "a thread under an unconfigured parent must not warn, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// Before `ready()` lands we cannot tell our own echo from a peer's
    /// message, so we stay quiet rather than risk advising the operator to
    /// allowlist this construct into its own input.
    #[tokio::test]
    async fn test_unknown_self_id_does_not_warn() {
        let ctx = BotMessageContext {
            self_id: None,
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_and_capture(&watched_config(), &ctx).await;

        assert!(
            filtered,
            "the drop itself must not depend on knowing our ID"
        );
        assert!(
            warnings.is_empty(),
            "an unknown self-ID must fail closed, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// Human messages are not dropped and must not warn, even when the human
    /// is absent from `allow_from`.
    #[tokio::test]
    async fn test_human_message_does_not_warn() {
        let ctx = BotMessageContext {
            is_bot: false,
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_and_capture(&watched_config(), &ctx).await;

        assert!(!filtered, "human message must not be filtered");
        assert!(
            warnings.is_empty(),
            "human message must not warn, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// An allowlisted bot passes through silently.
    #[tokio::test]
    async fn test_allowlisted_bot_does_not_warn() {
        let ctx = BotMessageContext {
            user_id: UserId::new(100),
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_and_capture(&watched_config(), &ctx).await;

        assert!(!filtered, "allowlisted bot must not be filtered");
        assert!(
            warnings.is_empty(),
            "allowlisted bot must not warn, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// The construct's own gateway echoes are dropped by this filter too.
    /// Warning about them would name our own ID and advise allowlisting
    /// ourselves, which would make the construct ingest its own output.
    #[tokio::test]
    async fn test_own_message_is_dropped_without_warning() {
        let ctx = BotMessageContext {
            user_id: UserId::new(7),
            self_id: Some(UserId::new(7)),
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_and_capture(&watched_config(), &ctx).await;

        assert!(filtered, "own echo must still be dropped");
        assert!(
            warnings.is_empty(),
            "must not warn about our own messages, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// In a channel that was never configured, the guild gate drops the message
    /// regardless of the allowlist, so naming `allow_from` would be wrong.
    ///
    /// The channel is seeded as a confirmed non-thread so the lookup resolves
    /// from cache; without that, `resolve_thread_parent` would reach for the
    /// Discord API.
    #[tokio::test]
    async fn test_unwatched_channel_does_not_warn() {
        let state = crate::state::new_state();
        state.write().await.record_thread_parent(55555, None);
        let ctx = BotMessageContext {
            channel_id: ChannelId::new(55555),
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_with_state(&state, &watched_config(), &ctx).await;

        assert!(filtered, "bot must still be dropped in unwatched channels");
        assert!(
            warnings.is_empty(),
            "must not warn for channels we never opted into, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// A thread whose cached parent is watched still warns.
    #[tokio::test]
    async fn test_thread_of_watched_channel_warns() {
        let state = crate::state::new_state();
        state.write().await.record_thread_parent(777, Some(12345));
        let ctx = BotMessageContext {
            channel_id: ChannelId::new(777),
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_with_state(&state, &watched_config(), &ctx).await;

        assert!(filtered);
        assert_eq!(
            warnings.len(),
            1,
            "a thread of a watched channel must warn like its parent"
        );
        assert_eq!(
            warnings[0].field("channel_id"),
            Some("777"),
            "the thread is where the message was dropped"
        );
        // The thread has no `[[channels]]` entry — only the parent does. Naming
        // the thread here would send the operator grepping config.toml for an ID
        // that is not in it, or worse, adding a stanza keyed on an ephemeral
        // thread ID and silently forking the gating rules.
        assert_eq!(
            warnings[0].field("config_channel_id"),
            Some("12345"),
            "the parent holds the config entry the operator has to edit"
        );
    }

    /// A proxy-webhook message (PluralKit) is human-authored: it is neither
    /// dropped nor warned about. Pins the warning below the proxy check — a
    /// warning here would be doubly wrong, since the message is delivered.
    #[tokio::test]
    async fn test_proxy_webhook_message_is_delivered_without_warning() {
        let state = crate::state::new_state();
        state.write().await.record_proxy_webhook(4242, true);
        let ctx = BotMessageContext {
            webhook_id: Some(4242),
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_with_state(&state, &watched_config(), &ctx).await;

        assert!(!filtered, "proxy webhook messages must pass through");
        assert!(
            warnings.is_empty(),
            "must not warn about a message that was delivered, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// A non-proxy webhook is dropped, but `allow_from` is not the remedy —
    /// the ID Discord reports is the webhook's, and webhook IDs are recreated
    /// freely. It is dropped quietly.
    #[tokio::test]
    async fn test_non_proxy_webhook_is_dropped_without_warning() {
        let state = crate::state::new_state();
        state.write().await.record_proxy_webhook(4242, false);
        let ctx = BotMessageContext {
            webhook_id: Some(4242),
            ..dropped_bot_ctx()
        };
        let (filtered, warnings) = filter_with_state(&state, &watched_config(), &ctx).await;

        assert!(filtered, "non-proxy webhook messages must still be dropped");
        assert!(
            warnings.is_empty(),
            "webhook IDs are not allowlistable; must not advise it, got: {:?}",
            warnings.iter().map(|w| &w.message).collect::<Vec<_>>()
        );
    }

    /// A chatty bot warns once per channel, not once per message — but a new
    /// channel or a new bot is a new diagnosis and warns again.
    #[tokio::test]
    async fn test_repeat_drops_are_throttled_per_channel() {
        let mut config_raw = Config {
            access: AccessConfig {
                dm_policy: DmPolicy::Queue,
                allow_from: vec!["100".to_string()],
                admins: vec![],
                admin_only_mutations: false,
            },
            ..Default::default()
        };
        // Mentions off, so each drop yields exactly one warning and the count
        // below measures the throttle rather than the mention notice.
        for id in ["12345", "67890"] {
            config_raw.channels.push(crate::config::ChannelConfig {
                id: id.to_string(),
                require_mention: false,
                ..Default::default()
            });
        }
        let config = LoadedConfig::from_raw(config_raw);

        let http = serenity::http::Http::new("fake");
        let state = crate::state::new_state();
        let base = dropped_bot_ctx();

        let warnings = capture_warnings(|| async {
            for _ in 0..5 {
                should_filter_bot_message(&http, &state, &config, &base).await;
            }
            let other_channel = BotMessageContext {
                channel_id: ChannelId::new(67890),
                ..dropped_bot_ctx()
            };
            should_filter_bot_message(&http, &state, &config, &other_channel).await;
            let other_bot = BotMessageContext {
                user_id: UserId::new(888),
                ..dropped_bot_ctx()
            };
            should_filter_bot_message(&http, &state, &config, &other_bot).await;
        })
        .await;

        let pairs: Vec<_> = warnings.iter().map(CapturedWarning::pair).collect();
        assert_eq!(
            pairs,
            vec![
                (Some("999"), Some("12345")),
                (Some("999"), Some("67890")),
                (Some("888"), Some("12345")),
            ],
            "expected exactly one warning per (bot, channel) pair, in order"
        );
    }
    // ── proxy bot constant tests ───────────────────────────────────────────────

    /// PluralKit's bot ID is in the proxy bot list.
    #[test]
    fn test_pluralkit_is_in_proxy_bot_ids() {
        assert!(
            PROXY_BOT_IDS.contains(&466378653216014359),
            "PluralKit bot ID must be in PROXY_BOT_IDS"
        );
    }

    /// Carl-bot (a non-proxy bot) is not in the proxy bot list.
    #[test]
    fn test_carlbot_not_in_proxy_bot_ids() {
        assert!(
            !PROXY_BOT_IDS.contains(&235148962103951360),
            "Carl-bot must not be in PROXY_BOT_IDS"
        );
    }

    // ── reply_to_id tests ──────────────────────────────────────────────────────

    #[test]
    fn reply_to_id_returns_message_id_when_present() {
        use serenity::model::{
            channel::{MessageReference, MessageReferenceKind},
            id::{ChannelId, MessageId},
        };

        let message_id = MessageId::new(42);
        let reference = MessageReference::new(MessageReferenceKind::Default, ChannelId::new(1))
            .message_id(message_id);

        assert_eq!(reply_to_id(&reference), Some(message_id));
    }

    #[test]
    fn reply_to_id_returns_none_when_message_id_absent() {
        use serenity::model::{
            channel::{MessageReference, MessageReferenceKind},
            id::ChannelId,
        };

        let reference = MessageReference::new(MessageReferenceKind::Default, ChannelId::new(1));

        assert_eq!(reply_to_id(&reference), None);
    }

    #[test]
    fn reply_to_id_returns_none_for_forward_reference() {
        use serenity::model::{
            channel::{MessageReference, MessageReferenceKind},
            id::{ChannelId, MessageId},
        };

        let message_id = MessageId::new(99);
        let reference = MessageReference::new(MessageReferenceKind::Forward, ChannelId::new(1))
            .message_id(message_id);

        assert_eq!(reply_to_id(&reference), None);
    }

    // ── reply_preview tests ───────────────────────────────────────────────────

    #[test]
    fn reply_preview_returns_none_for_empty() {
        assert_eq!(reply_preview(""), None);
    }

    #[test]
    fn reply_preview_passes_through_short_content() {
        assert_eq!(reply_preview("hello"), Some("hello".to_string()));
    }

    #[test]
    fn reply_preview_truncates_and_appends_ellipsis() {
        let content: String = "a".repeat(100);
        let preview = reply_preview(&content).expect("non-empty content");
        assert_eq!(preview.chars().count(), 100);
        assert!(!preview.ends_with('…'));

        let long: String = "b".repeat(101);
        let preview = reply_preview(&long).expect("non-empty content");
        assert_eq!(preview.chars().count(), 101);
        assert!(preview.ends_with('…'));

        let emoji: String = "🐌".repeat(101);
        let preview = reply_preview(&emoji).expect("emoji content");
        assert_eq!(preview.chars().count(), 101);
        assert!(preview.ends_with('…'));
    }

    // ── reply_context tests ───────────────────────────────────────────────────

    fn wire_author(id: u64, username: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id.to_string(),
            "username": username,
            "discriminator": "0",
        })
    }

    fn wire_message_body(id: u64, author: serde_json::Value, content: &str) -> serde_json::Value {
        serde_json::json!({
            "id": id.to_string(),
            "type": 0,
            "channel_id": "1",
            "author": author,
            "content": content,
            "timestamp": "2026-01-01T00:00:00.000000+00:00",
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": [],
            "embeds": [],
            "pinned": false,
            "flags": 0,
            "components": [],
        })
    }

    fn message_from_wire(payload: serde_json::Value) -> Message {
        serde_json::from_value(payload).expect("valid Message JSON")
    }

    fn wire_reply_message(
        content: &str,
        reference: serde_json::Value,
        referenced_message: Option<serde_json::Value>,
    ) -> Message {
        let mut payload = wire_message_body(100, wire_author(1, "alice"), content);
        payload["message_reference"] = reference;
        if let Some(parent) = referenced_message {
            payload["referenced_message"] = parent;
        }
        message_from_wire(payload)
    }

    #[test]
    fn reply_context_returns_none_for_non_reply() {
        let msg = message_from_wire(wire_message_body(
            100,
            wire_author(1, "alice"),
            "not a reply",
        ));
        assert_eq!(reply_context(&msg), (None, None, None));
    }

    #[test]
    fn reply_context_returns_none_for_forward() {
        let msg = wire_reply_message(
            "forwarded",
            serde_json::json!({
                "type": 1,
                "channel_id": "1",
                "message_id": "999",
            }),
            Some(wire_message_body(
                999,
                wire_author(500, "parent"),
                "parent text",
            )),
        );
        assert_eq!(reply_context(&msg), (None, None, None));
    }

    #[test]
    fn reply_context_returns_none_when_referenced_message_absent() {
        let msg = wire_reply_message(
            "reply without parent body",
            serde_json::json!({
                "type": 0,
                "channel_id": "1",
                "message_id": "999",
            }),
            None,
        );
        assert_eq!(reply_context(&msg), (None, None, None));
    }

    #[test]
    fn reply_context_extracts_author_and_preview_for_reply() {
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({
                "type": 0,
                "channel_id": "1",
                "message_id": "999",
            }),
            Some(wire_message_body(
                999,
                wire_author(500, "parentuser"),
                "parent message content",
            )),
        );
        let (uid, user, preview) = reply_context(&msg);
        assert_eq!(uid, Some(UserId::new(500)));
        assert_eq!(user.as_deref(), Some("parentuser"));
        assert_eq!(preview.as_deref(), Some("parent message content"));
    }

    #[test]
    fn reply_context_omits_preview_for_empty_parent_content() {
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({
                "type": 0,
                "channel_id": "1",
                "message_id": "999",
            }),
            Some(wire_message_body(999, wire_author(500, "parentuser"), "")),
        );
        let (uid, user, preview) = reply_context(&msg);
        assert_eq!(uid, Some(UserId::new(500)));
        assert_eq!(user.as_deref(), Some("parentuser"));
        assert_eq!(preview, None);
    }

    // ── build_message_event tests ─────────────────────────────────────────────

    #[test]
    fn build_message_event_populates_reply_context_for_reply() {
        let config = LoadedConfig::from_raw(Config::default());
        let msg = wire_reply_message(
            "my reply",
            serde_json::json!({
                "type": 0,
                "channel_id": "1",
                "message_id": "999",
            }),
            Some(wire_message_body(
                999,
                wire_author(500, "parentuser"),
                "parent message content",
            )),
        );

        let event = build_message_event(&msg, &config, None, MessageTargeting::Ambient);
        let NotificationEvent::Message(MessageEvent {
            reply_to_message_id,
            reply_to_user_id,
            reply_to_user,
            reply_to_content_preview,
            content,
            user,
            ..
        }) = event
        else {
            panic!("expected Message event");
        };

        assert_eq!(reply_to_message_id, Some(MessageId::new(999)));
        assert_eq!(reply_to_user_id, Some(UserId::new(500)));
        assert_eq!(reply_to_user.as_deref(), Some("parentuser"));
        assert_eq!(
            reply_to_content_preview.as_deref(),
            Some("parent message content")
        );
        assert_eq!(content, "my reply");
        assert_eq!(user, "alice");
    }

    // ── display_name tests ───────────────────────────────────────────────────

    #[test]
    fn display_name_falls_back_to_username() {
        let msg = message_from_wire(wire_message_body(1, wire_author(1, "alice"), "hi"));
        assert_eq!(display_name(&msg), "alice");
    }

    #[test]
    fn display_name_prefers_global_name() {
        let mut author = wire_author(1, "alice");
        author["global_name"] = serde_json::json!("Alice Display");
        let msg = message_from_wire(wire_message_body(1, author, "hi"));
        assert_eq!(display_name(&msg), "Alice Display");
    }

    #[test]
    fn display_name_prefers_server_nick() {
        let mut author = wire_author(1, "alice");
        author["global_name"] = serde_json::json!("Alice Display");
        let mut payload = wire_message_body(1, author, "hi");
        payload["member"] = serde_json::json!({
            "roles": [],
            "joined_at": "2026-01-01T00:00:00.000000+00:00",
            "deaf": false,
            "mute": false,
            "nick": "Server Alice",
        });
        let msg = message_from_wire(payload);
        assert_eq!(display_name(&msg), "Server Alice");
    }

    #[test]
    fn display_name_from_user_falls_back_to_username() {
        let user_json = wire_author(1, "bob_iverse");
        let user: serenity::model::user::User =
            serde_json::from_value(user_json).expect("valid User JSON");
        assert_eq!(display_name_from_user(&user), "bob_iverse");
    }

    #[test]
    fn display_name_from_user_prefers_global_name() {
        let mut user_json = wire_author(1, "bob_iverse");
        user_json["global_name"] = serde_json::json!("Bob Iverse");
        let user: serenity::model::user::User =
            serde_json::from_value(user_json).expect("valid User JSON");
        assert_eq!(display_name_from_user(&user), "Bob Iverse");
    }

    // ── resolve_user_identity tests (#153) ───────────────────────────────────

    #[test]
    fn resolve_user_identity_prefers_display_name() {
        assert_eq!(
            resolve_user_identity(Some("Vesper"), Some("vesper_bot")),
            "Vesper"
        );
    }

    #[test]
    fn resolve_user_identity_falls_back_to_username_when_display_name_unset() {
        assert_eq!(
            resolve_user_identity(None, Some("vesper_bot")),
            "vesper_bot"
        );
    }

    #[test]
    fn resolve_user_identity_falls_back_to_username_when_display_name_blank() {
        // An empty (or whitespace-only) display name must not leak through; it
        // is treated as absent so the Discord username is used instead.
        assert_eq!(
            resolve_user_identity(Some(""), Some("vesper_bot")),
            "vesper_bot"
        );
        assert_eq!(
            resolve_user_identity(Some("   "), Some("vesper_bot")),
            "vesper_bot"
        );
    }

    #[test]
    fn resolve_user_identity_defaults_to_dione_when_neither_present() {
        assert_eq!(resolve_user_identity(None, None), "dione");
        assert_eq!(resolve_user_identity(Some(""), Some("")), "dione");
        assert_eq!(resolve_user_identity(Some("  "), None), "dione");
    }

    // ── strip_invisible tests ────────────────────────────────────────────────

    #[test]
    fn strip_invisible_removes_pk_padding() {
        assert_eq!(strip_invisible("B\u{17B5}"), "B");
    }

    #[test]
    fn strip_invisible_removes_zero_width_space() {
        assert_eq!(strip_invisible("test\u{200B}name"), "testname");
    }

    #[test]
    fn strip_invisible_preserves_normal_text() {
        assert_eq!(strip_invisible("Benedicta"), "Benedicta");
    }

    #[test]
    fn display_name_strips_pk_padding() {
        let mut author = wire_author(1, "B\u{17B5}");
        author["bot"] = serde_json::json!(true);
        let msg = message_from_wire(wire_message_body(1, author, "hello"));
        assert_eq!(display_name(&msg), "B");
    }
}
