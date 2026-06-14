use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serenity::async_trait;
use serenity::builder::{
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
};
use serenity::model::event::MessageUpdateEvent;
use serenity::model::prelude::*;
use serenity::prelude::*;

use crate::gate::{GateDecision, InboundGate, MentionDetector};
use crate::mcp::tools::messaging::create_dm_channel;
use crate::queue::AccessRequest;

// ── Event types ───────────────────────────────────────────────────────────────

/// Metadata about a Discord attachment, forwarded to the MCP client.
#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    pub name: String,
    pub content_type: Option<String>,
    pub size: u64,
}

/// Events forwarded from the Discord gateway to the MCP notification stream.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    Message {
        chat_id: ChannelId,
        message_id: MessageId,
        user: String,
        user_id: UserId,
        content: String,
        timestamp: String,
        attachments: Vec<AttachmentMeta>,
        is_voice_message: bool,
        /// If the message was sent in a thread, the parent channel ID.
        thread_parent_id: Option<ChannelId>,
        /// If the message is a reply, the ID of the message being replied to.
        reply_to_message_id: Option<String>,
    },
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
        timestamp: String,
        /// If the edit was in a thread, the parent channel ID.
        thread_parent_id: Option<ChannelId>,
        /// If the edited message is a reply, the ID of the message being replied to.
        reply_to_message_id: Option<String>,
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
            state.cache_username(msg.author.id.get(), msg.author.name.clone());
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
                    let request = AccessRequest {
                        user_id: sender_id,
                        username: msg.author.name.clone(),
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
                                &msg.author.name,
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

        {
            let mut state = self.state.write().await;
            if is_dm {
                state.record_dm_channel(author.id.get(), channel_id);
            }
            state.cache_username(author.id.get(), author.name.clone());
        }

        let timestamp = config
            .localize_rfc3339(&serenity_ts_to_rfc3339("edited_ts", &edited_ts))
            .into();

        // message_reference is Option<Option<MessageReference>> in update events:
        // outer Option = field present in update, inner Option = nullable value.
        // Flatten to get the actual reference, then extract the replied-to message ID.
        let reply_to_message_id = event
            .message_reference
            .as_ref()
            .and_then(|outer| outer.as_ref())
            .and_then(|r| r.message_id)
            .map(|id| id.get().to_string());

        let ev = NotificationEvent::MessageEdit {
            chat_id: event.channel_id,
            message_id: event.id,
            user: author.name.clone(),
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

    let reply_to_message_id = msg
        .message_reference
        .as_ref()
        .and_then(|r| r.message_id)
        .map(|id| id.get().to_string());

    NotificationEvent::Message {
        chat_id: msg.channel_id,
        message_id: msg.id,
        user: msg.author.name.clone(),
        user_id: msg.author.id,
        content: msg.content.clone(),
        timestamp: config
            .localize_rfc3339(&serenity_ts_to_rfc3339("msg.timestamp", &msg.timestamp))
            .into(),
        attachments,
        is_voice_message,
        thread_parent_id: thread_parent_id.map(ChannelId::new),
        reply_to_message_id,
    }
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
}
