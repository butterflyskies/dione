use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serenity::async_trait;
use serenity::builder::{
    CreateInteractionResponse, CreateInteractionResponseMessage, CreateMessage,
};
use serenity::model::prelude::*;
use serenity::prelude::*;

use crate::gate::{GateDecision, InboundGate, MentionDetector};
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
        chat_id: String,
        message_id: String,
        user: String,
        user_id: String,
        content: String,
        timestamp: String,
        attachments: Vec<AttachmentMeta>,
        is_voice_message: bool,
    },
    Reaction {
        chat_id: String,
        message_id: String,
        user: String,
        user_id: String,
        emoji: String,
    },
    PermissionResponse {
        request_id: String,
        granted: bool,
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
        // Ignore bot messages.
        if msg.author.bot {
            return;
        }

        let config = crate::config::load_config(&self.state_dir);
        let bot_user_id = self.bot_user_id.load(Ordering::Relaxed);

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

                    let event = build_message_event(&msg);
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

            // Determine effective channel ID — use parent for threads if available.
            let channel_id = msg.channel_id.get();

            let decision =
                InboundGate::check_guild(&config, channel_id, msg.author.id.get(), is_mentioned);

            match decision {
                GateDecision::Deliver => {
                    let event = build_message_event(&msg);
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

        let user_id = reaction.user_id.map(|u| u.get()).unwrap_or(0);
        if user_id == 0 {
            return;
        }

        let emoji = match &reaction.emoji {
            ReactionType::Unicode(s) => s.clone(),
            ReactionType::Custom { name, id, .. } => {
                name.clone().unwrap_or_else(|| id.get().to_string())
            }
            _ => return,
        };

        let event = NotificationEvent::Reaction {
            chat_id: channel_id.get().to_string(),
            message_id: message_id.to_string(),
            user: String::new(),
            user_id: user_id.to_string(),
            emoji,
        };

        if let Err(e) = self.tx.send(event).await {
            tracing::warn!(error = %e, "failed to send reaction notification event");
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

        // Discord acknowledged — forward the permission response event.
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

fn build_message_event(msg: &Message) -> NotificationEvent {
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

    NotificationEvent::Message {
        chat_id: msg.channel_id.get().to_string(),
        message_id: msg.id.get().to_string(),
        user: msg.author.name.clone(),
        user_id: msg.author.id.get().to_string(),
        content: msg.content.clone(),
        timestamp: msg.timestamp.to_rfc3339().unwrap_or_default(),
        attachments,
        is_voice_message,
    }
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
    let dm_body = serde_json::json!({ "recipient_id": admin_id.to_string() });
    let channel = match http.create_private_channel(&dm_body).await {
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
