use std::sync::Arc;

use camino::Utf8PathBuf;
use serde_json::{Value, json};
use serenity::builder::{CreateAttachment, CreateMessage, EditMessage};
use serenity::model::id::{ChannelId, MessageId};

use crate::config::{ChunkMode, load_config};
use crate::discord::chunk;
use crate::gate::OutboundGate;
use crate::state::State;

/// Context available to all messaging tools.
pub struct MessagingCtx {
    pub http: Arc<serenity::http::Http>,
    pub state: State,
    pub state_dir: Utf8PathBuf,
}

// ── Gate helper ───────────────────────────────────────────────────────────────

/// Returns `Ok(())` if the channel is permitted, or `Err(json_error)` if not.
pub(crate) async fn check_outbound(ctx: &MessagingCtx, channel_id: u64) -> Result<(), Value> {
    let config = load_config(&ctx.state_dir);
    let state = ctx.state.read().await;
    if !OutboundGate::check_channel(&config, channel_id, &state.dm_channel_ids) {
        return Err(
            json!({ "error": format!("channel {channel_id} is not a permitted outbound target") }),
        );
    }
    Ok(())
}

// ── reply ─────────────────────────────────────────────────────────────────────

pub async fn reply(
    ctx: &MessagingCtx,
    channel_id: u64,
    content: &str,
    reply_to_message_id: Option<u64>,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    let config = load_config(&ctx.state_dir);
    let ch = ChannelId::new(channel_id);

    // Fire typing indicator now that we've committed to sending a reply.
    let _ = ctx.http.broadcast_typing(ch).await;

    let limit = config.delivery.text_chunk_limit;
    let mode = config.delivery.chunk_mode;
    let reply_mode = config.delivery.reply_to_mode;

    // Determine chunk mode default.
    let effective_mode = if limit == 0 {
        ChunkMode::Paragraph
    } else {
        mode
    };
    let effective_limit = if limit == 0 { 2000 } else { limit };

    let chunks = chunk(content, effective_limit, effective_mode);
    let mut sent_ids: Vec<u64> = Vec::new();
    let mut first_msg_id: Option<MessageId> = None;

    for (i, chunk_text) in chunks.iter().enumerate() {
        let mut builder = CreateMessage::new().content(*chunk_text);

        // Reply threading.
        let should_reply = match reply_mode {
            crate::config::ReplyToMode::Off => false,
            crate::config::ReplyToMode::First => i == 0,
            crate::config::ReplyToMode::All => true,
        };

        if should_reply {
            if i == 0 {
                if let Some(mid) = reply_to_message_id {
                    builder = builder.reference_message((ch, MessageId::new(mid)));
                }
            } else if let Some(prev_id) = first_msg_id {
                builder = builder.reference_message((ch, prev_id));
            }
        }

        match ch.send_message(&ctx.http, builder).await {
            Ok(msg) => {
                let mid = msg.id.get();
                sent_ids.push(mid);
                if i == 0 {
                    first_msg_id = Some(msg.id);
                }
                // Record sent IDs in state.
                let mut state = ctx.state.write().await;
                state.note_sent(mid);
            }
            Err(e) => {
                tracing::warn!(channel_id, chunk = i, error = %e, "failed to send chunk");
                return json!({ "error": format!("failed to send chunk {i}: {e}") });
            }
        }
    }

    json!({
        "ok": true,
        "message_ids": sent_ids,
    })
}

// ── react ─────────────────────────────────────────────────────────────────────

pub async fn react(ctx: &MessagingCtx, channel_id: u64, message_id: u64, emoji: &str) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    let reaction = parse_reaction_type(emoji);
    match ctx
        .http
        .create_reaction(
            ChannelId::new(channel_id),
            MessageId::new(message_id),
            &reaction,
        )
        .await
    {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── edit_message ──────────────────────────────────────────────────────────────

pub async fn edit_message(
    ctx: &MessagingCtx,
    channel_id: u64,
    message_id: u64,
    new_content: &str,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    let builder = EditMessage::new().content(new_content);
    match ctx
        .http
        .edit_message(
            ChannelId::new(channel_id),
            MessageId::new(message_id),
            &builder,
            vec![],
        )
        .await
    {
        Ok(msg) => json!({ "ok": true, "message_id": msg.id.get().to_string() }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── fetch_messages ────────────────────────────────────────────────────────────

pub async fn fetch_messages(ctx: &MessagingCtx, channel_id: u64, limit: u8) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    match ctx
        .http
        .get_messages(ChannelId::new(channel_id), None, Some(limit))
        .await
    {
        Ok(messages) => {
            let msgs: Vec<Value> = messages
                .iter()
                .map(|m| {
                    json!({
                        "id": m.id.get().to_string(),
                        "author": m.author.name,
                        "author_id": m.author.id.get().to_string(),
                        "content": m.content,
                        "timestamp": m.timestamp.to_rfc3339(),
                        "attachments": m.attachments.iter().map(|a| json!({
                            "name": a.filename,
                            "url": a.url,
                            "size": a.size,
                        })).collect::<Vec<_>>(),
                    })
                })
                .collect();
            json!({ "messages": msgs })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── download_attachment ───────────────────────────────────────────────────────

pub async fn download_attachment(ctx: &MessagingCtx, channel_id: u64, message_id: u64) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    // Fetch the message to get attachment URLs.
    let msg = match ctx
        .http
        .get_message(ChannelId::new(channel_id), MessageId::new(message_id))
        .await
    {
        Ok(m) => m,
        Err(e) => return json!({ "error": format!("failed to fetch message: {e}") }),
    };

    if msg.attachments.is_empty() {
        return json!({ "error": "message has no attachments" });
    }

    // Ensure inbox directory exists.
    let inbox_dir = ctx.state_dir.join("inbox");
    if let Err(e) = tokio::fs::create_dir_all(&inbox_dir).await {
        return json!({ "error": format!("failed to create inbox: {e}") });
    }

    let mut saved_paths: Vec<String> = Vec::new();

    for (idx, attachment) in msg.attachments.iter().enumerate() {
        let safe_name = crate::gate::sanitize_filename(&attachment.filename);
        let dest = {
            let candidate = inbox_dir.join(&safe_name);
            if candidate.exists() {
                inbox_dir.join(format!("{idx}-{safe_name}"))
            } else {
                candidate
            }
        };

        // Download attachment bytes.
        match download_url(&attachment.url).await {
            Ok(bytes) => {
                if let Err(e) = tokio::fs::write(&dest, &bytes).await {
                    tracing::warn!(
                        name = %safe_name,
                        error = %e,
                        "failed to write attachment to inbox"
                    );
                } else {
                    saved_paths.push(dest.to_string());
                }
            }
            Err(e) => {
                tracing::warn!(url = %attachment.url, error = %e, "failed to download attachment");
            }
        }
    }

    json!({ "saved": saved_paths })
}

// ── send_attachment (shared helper) ──────────────────────────────────────────

pub(crate) async fn send_attachment(
    ctx: &MessagingCtx,
    channel_id: u64,
    attachment: CreateAttachment,
    caption: Option<&str>,
) -> Value {
    let ch = ChannelId::new(channel_id);
    let _ = ctx.http.broadcast_typing(ch).await;
    let mut builder = CreateMessage::new().add_file(attachment);
    if let Some(text) = caption {
        builder = builder.content(text);
    }
    match ch.send_message(&ctx.http, builder).await {
        Ok(msg) => {
            let mid = msg.id.get();
            let mut state = ctx.state.write().await;
            state.note_sent(mid);
            json!({ "ok": true, "message_id": mid.to_string() })
        }
        Err(e) => json!({ "error": format!("failed to send: {e}") }),
    }
}

// ── send_file ────────────────────────────────────────────────────────────────

pub async fn send_file(
    ctx: &MessagingCtx,
    channel_id: u64,
    file_path: &str,
    caption: Option<&str>,
) -> Value {
    let path = std::path::Path::new(file_path);
    if !path.is_absolute() {
        return json!({ "error": "file_path must be absolute" });
    }

    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    let utf8_path = camino::Utf8Path::new(file_path);
    if !OutboundGate::check_file_send(utf8_path, &ctx.state_dir) {
        return json!({ "error": "file_path is not permitted for upload" });
    }

    let attachment = match CreateAttachment::path(path).await {
        Ok(a) => a,
        Err(e) => return json!({ "error": format!("failed to read file: {e}") }),
    };

    send_attachment(ctx, channel_id, attachment, caption).await
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parses an emoji string into the appropriate serenity ReactionType.
/// Handles both Unicode emoji ("👍") and custom Discord emoji ("<:name:id>" or "<a:name:id>").
fn parse_reaction_type(emoji: &str) -> serenity::model::channel::ReactionType {
    use serenity::model::channel::ReactionType;
    use serenity::model::id::EmojiId;

    // Custom emoji: <:name:id> or <a:name:id>
    let trimmed = emoji.trim();
    if trimmed.starts_with('<') && trimmed.ends_with('>') {
        let inner = &trimmed[1..trimmed.len() - 1];
        let parts: Vec<&str> = inner.split(':').collect();
        if parts.len() == 3 {
            let animated = parts[0] == "a";
            let name = parts[1].to_string();
            if let Ok(id) = parts[2].parse::<u64>() {
                return ReactionType::Custom {
                    animated,
                    id: EmojiId::new(id),
                    name: Some(name),
                };
            }
        }
    }

    ReactionType::Unicode(emoji.to_string())
}

const MAX_ATTACHMENT_BYTES: u64 = 25 * 1024 * 1024;

async fn download_url(url: &str) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
    let resp = reqwest::get(url).await?.error_for_status()?;

    if let Some(len) = resp.content_length()
        && len > MAX_ATTACHMENT_BYTES
    {
        return Err(
            format!("attachment too large: {len} bytes (max {MAX_ATTACHMENT_BYTES})").into(),
        );
    }

    let bytes = resp.bytes().await?;
    if bytes.len() as u64 > MAX_ATTACHMENT_BYTES {
        return Err(format!(
            "attachment too large: {} bytes (max {MAX_ATTACHMENT_BYTES})",
            bytes.len()
        )
        .into());
    }

    Ok(bytes.to_vec())
}

// ── get_message ───────────────────────────────────────────────────────────────

pub async fn get_message(ctx: &MessagingCtx, channel_id: u64, message_id: u64) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    match ctx
        .http
        .get_message(ChannelId::new(channel_id), MessageId::new(message_id))
        .await
    {
        Ok(m) => json!({
            "id": m.id.get().to_string(),
            "author": m.author.name,
            "author_id": m.author.id.get().to_string(),
            "content": m.content,
            "timestamp": m.timestamp.to_rfc3339(),
            "attachments": m.attachments.iter().map(|a| json!({
                "name": a.filename,
                "url": a.url,
                "size": a.size,
                "content_type": a.content_type,
            })).collect::<Vec<_>>(),
        }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}
