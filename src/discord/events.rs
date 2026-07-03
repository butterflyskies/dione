use crate::{
    gate::{GateDecision, InboundGate, MentionDetector},
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
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
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
}

// ── EventHandler impl ─────────────────────────────────────────────────────────

#[async_trait]
impl EventHandler for Handler {
    async fn ready(&self, _ctx: Context, ready: Ready) {
        let id = ready.user.id.get();
        self.bot_user_id.store(id, Ordering::Relaxed);
        tracing::info!(
            user = %ready.user.name,
            id,
            "Discord gateway ready"
        );
    }

    async fn message(&self, ctx: Context, msg: Message) {
        let config = crate::config::load_config(&self.state_dir);

        // Allow bot messages only if the bot's user ID is in the allow_from list.
        if should_drop_bot_message(msg.author.bot, msg.author.id.get(), &config) {
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

                    let event = build_message_event(&msg, &config, None);
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
            let is_mentioned = MentionDetector::is_mentioned(
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
                is_mentioned,
            );

            match decision {
                GateDecision::Deliver => {
                    let event = build_message_event(&msg, &config, resolved.thread_parent_id);
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

        let user_name = {
            let state = self.state.read().await;
            state
                .user_names
                .get(&user_id.get())
                .cloned()
                .unwrap_or_default()
        };

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

        // Allow bot messages only if the bot's user ID is in the allow_from list.
        if should_drop_bot_message(author.bot, author.id.get(), &config) {
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
            let sender_name = new
                .as_ref()
                .map(display_name)
                .unwrap_or_else(|| display_name_from_user(author));
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
    msg.member
        .as_ref()
        .and_then(|m| m.nick.as_ref())
        .or(msg.author.global_name.as_ref())
        .unwrap_or(&msg.author.name)
        .clone()
}

/// Display name from a bare `User` (no guild member context).
/// Falls back global_name > username.
fn display_name_from_user(user: &serenity::model::user::User) -> String {
    user.global_name.as_ref().unwrap_or(&user.name).clone()
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
        user: display_name(msg),
        user_id: msg.author.id,
        content: msg.content.clone(),
        timestamp: config
            .localize_rfc3339(&serenity_ts_to_rfc3339("msg.timestamp", &msg.timestamp)),
        attachments,
        is_voice_message,
        thread_parent_id: thread_parent_id.map(ChannelId::new),
        reply_to_message_id,
        reply_to_user_id,
        reply_to_user,
        reply_to_content_preview,
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

        let event = build_message_event(&msg, &config, None);
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
}
