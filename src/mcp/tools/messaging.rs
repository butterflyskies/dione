use std::sync::Arc;

use camino::Utf8PathBuf;
use serde_json::{Value, json};
use serenity::builder::{CreateAllowedMentions, CreateAttachment, CreateMessage, EditMessage};
use serenity::http::MessagePagination;
use serenity::model::Timestamp;
use serenity::model::channel::Message;
use serenity::model::id::{ChannelId, MessageId, UserId};

use crate::config::{ChunkMode, DmPolicy, LoadedConfig};
use crate::discord::chunk;
use crate::gate::OutboundGate;
use crate::state::State;

/// Context available to all messaging tools.
pub struct MessagingCtx {
    pub http: Arc<serenity::http::Http>,
    pub state: State,
    pub config: Arc<LoadedConfig>,
    pub state_dir: Utf8PathBuf,
}

// ── Gate helper ───────────────────────────────────────────────────────────────

/// Returns `Ok(())` if the channel is permitted, or `Err(json_error)` if not.
pub(crate) async fn check_outbound(ctx: &MessagingCtx, channel_id: ChannelId) -> Result<(), Value> {
    let state = ctx.state.read().await;
    if !OutboundGate::check_channel_with_threads(
        &ctx.config,
        channel_id.get(),
        &state.dm_channel_ids,
        &state.thread_parents,
    ) {
        return Err(
            json!({ "error": format!("channel {channel_id} is not a permitted outbound target") }),
        );
    }
    Ok(())
}

// ── reply ─────────────────────────────────────────────────────────────────────

pub async fn reply(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    content: &str,
    reply_to_message_id: Option<MessageId>,
    suppress_ping: bool,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    let ch = channel_id;

    // Fire typing indicator now that we've committed to sending a reply.
    let _ = ctx.http.broadcast_typing(ch).await;

    let limit = ctx.config.delivery.text_chunk_limit;
    let mode = ctx.config.delivery.chunk_mode;
    let reply_mode = ctx.config.delivery.reply_to_mode;

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
                    builder = builder.reference_message((ch, mid));
                }
            } else if let Some(prev_id) = first_msg_id {
                builder = builder.reference_message((ch, prev_id));
            }
        }

        if suppress_ping {
            builder = builder.allowed_mentions(CreateAllowedMentions::new().replied_user(false));
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
                tracing::warn!(channel_id = channel_id.get(), chunk = i, error = %e, "failed to send chunk");
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

pub async fn react(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    emoji: &str,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    let reaction = match parse_reaction_type(emoji) {
        Ok(r) => r,
        Err(e) => return json!({ "error": e }),
    };
    match ctx
        .http
        .create_reaction(channel_id, message_id, &reaction)
        .await
    {
        Ok(()) => json!({ "ok": true }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── edit_message ──────────────────────────────────────────────────────────────

pub async fn edit_message(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
    new_content: &str,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    let builder = EditMessage::new().content(new_content);
    match ctx
        .http
        .edit_message(channel_id, message_id, &builder, vec![])
        .await
    {
        Ok(msg) => json!({ "ok": true, "message_id": msg.id.get().to_string() }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── fetch_messages ────────────────────────────────────────────────────────────

pub async fn fetch_messages(ctx: &MessagingCtx, channel_id: ChannelId, limit: u8) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    match ctx.http.get_messages(channel_id, None, Some(limit)).await {
        Ok(messages) => {
            let msgs: Vec<Value> = messages
                .iter()
                .map(|m| message_json(&ctx.config, m))
                .collect();
            json!({ "messages": msgs })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Serializes one message into the wire shape shared by `fetch_messages`,
/// `fetch_new_since`, and `search_messages`, so the tools cannot drift apart.
pub(crate) fn message_json(config: &LoadedConfig, m: &Message) -> Value {
    json!({
        "id": m.id.get().to_string(),
        "author": m.author.name,
        "author_id": m.author.id.get().to_string(),
        "content": m.content,
        "timestamp": config.localize_rfc3339(&serenity_ts_to_rfc3339(&m.timestamp)),
        "attachments": m.attachments.iter().map(|a| json!({
            "name": a.filename,
            "url": a.url,
            "size": a.size,
        })).collect::<Vec<_>>(),
    })
}

// ── fetch_new_since ───────────────────────────────────────────────────────────

pub async fn fetch_new_since(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    after_message_id: MessageId,
    limit: u8,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    match ctx
        .http
        .get_messages(
            channel_id,
            Some(MessagePagination::After(after_message_id)),
            Some(limit),
        )
        .await
    {
        Ok(messages) => new_since_response(&ctx.config, messages, limit),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Assembles the `fetch_new_since` response: messages sorted oldest-first,
/// plus `count` and a `has_more` pagination hint.
///
/// Discord returns messages newest-first on the wire. The caller owns the
/// pagination cursor (the `id` of the last returned message), so chronological
/// order is part of the tool's contract rather than an accident of the wire
/// format.
fn new_since_response(config: &LoadedConfig, mut messages: Vec<Message>, limit: u8) -> Value {
    messages.sort_unstable_by_key(|m| m.id);
    let count = messages.len();
    let msgs: Vec<Value> = messages.iter().map(|m| message_json(config, m)).collect();
    json!({
        "messages": msgs,
        "count": count,
        // `limit > 0` guards against an empty page claiming more data:
        // dispatch clamps limit to 1..=100, but this function must not
        // produce `{count: 0, has_more: true}` even if that ever regresses.
        "has_more": limit > 0 && count == usize::from(limit),
    })
}

// ── download_attachment ───────────────────────────────────────────────────────

pub async fn download_attachment(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    // Fetch the message to get attachment URLs.
    let msg = match ctx.http.get_message(channel_id, message_id).await {
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
    channel_id: ChannelId,
    attachment: CreateAttachment,
    caption: Option<&str>,
) -> Value {
    let _ = ctx.http.broadcast_typing(channel_id).await;
    let mut builder = CreateMessage::new().add_file(attachment);
    if let Some(text) = caption {
        builder = builder.content(text);
    }
    match channel_id.send_message(&ctx.http, builder).await {
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
    channel_id: ChannelId,
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

// ── DM helpers ───────────────────────────────────────────────────────────────

pub(crate) async fn create_dm_channel(
    http: &serenity::http::Http,
    user_id: UserId,
) -> Result<serenity::model::channel::PrivateChannel, String> {
    let dm_body = json!({ "recipient_id": user_id.get().to_string() });
    http.create_private_channel(&dm_body)
        .await
        .map_err(|e| format!("failed to create DM channel: {e}"))
}

// ── send_dm ──────────────────────────────────────────────────────────────────

pub async fn send_dm(ctx: &MessagingCtx, user_id: UserId, content: &str) -> Value {
    if ctx.config.access.dm_policy == DmPolicy::Disabled {
        return json!({ "error": "dm_policy is set to disabled; cannot initiate DMs" });
    }

    let channel = match create_dm_channel(&ctx.http, user_id).await {
        Ok(c) => c,
        Err(e) => return json!({ "error": e }),
    };

    let channel_id = channel.id;

    {
        let mut state = ctx.state.write().await;
        state.record_dm_channel(user_id.get(), channel_id.get());
    }

    let result = reply(ctx, channel_id, content, None, false).await;

    if result.get("error").is_some() {
        return result;
    }

    let message_ids = result["message_ids"].clone();
    json!({
        "ok": true,
        "channel_id": channel_id.get().to_string(),
        "message_ids": message_ids,
    })
}

// ── Helpers ──────────────────────────────────────────────────────────────────

/// Parses an emoji string into the appropriate serenity ReactionType.
/// Handles both Unicode emoji ("👍") and custom Discord emoji ("<:name:id>" or "<a:name:id>").
///
/// Returns an error for custom emoji with a zero ID: snowflakes are nonzero,
/// and serenity's `EmojiId::new` (NonZeroU64-backed) panics on 0.
fn parse_reaction_type(emoji: &str) -> Result<serenity::model::channel::ReactionType, String> {
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
                if id == 0 {
                    return Err(format!(
                        "invalid custom emoji {trimmed:?}: emoji ID must be nonzero"
                    ));
                }
                return Ok(ReactionType::Custom {
                    animated,
                    id: EmojiId::new(id),
                    name: Some(name),
                });
            }
        }
    }

    Ok(ReactionType::Unicode(emoji.to_string()))
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

pub async fn get_message(
    ctx: &MessagingCtx,
    channel_id: ChannelId,
    message_id: MessageId,
) -> Value {
    if let Err(e) = check_outbound(ctx, channel_id).await {
        return e;
    }

    match ctx.http.get_message(channel_id, message_id).await {
        Ok(m) => {
            json!({
                "id": m.id.get().to_string(),
                "author": m.author.name,
                "author_id": m.author.id.get().to_string(),
                "content": m.content,
                "timestamp": ctx.config.localize_rfc3339(&serenity_ts_to_rfc3339(&m.timestamp)),
                "attachments": m.attachments.iter().map(|a| json!({
                    "name": a.filename,
                    "url": a.url,
                    "size": a.size,
                    "content_type": a.content_type,
                })).collect::<Vec<_>>(),
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

/// Converts a serenity [`Timestamp`] to an RFC 3339 string.
///
/// If `to_rfc3339()` returns `None` — which indicates the timestamp is broken
/// at the Discord API level — logs a warning and falls back to the current UTC
/// time so tool responses never contain an empty timestamp string.
fn serenity_ts_to_rfc3339(ts: &Timestamp) -> String {
    match ts.to_rfc3339() {
        Some(s) => s,
        None => {
            let fallback = chrono::Utc::now().to_rfc3339();
            tracing::warn!(
                fallback = %fallback,
                "Discord timestamp failed to_rfc3339(); using current UTC time as fallback"
            );
            fallback
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ChannelConfig, Config};
    use crate::state::new_state;

    fn test_config() -> LoadedConfig {
        LoadedConfig::from_raw(Config::default())
    }

    fn messaging_ctx(config: LoadedConfig) -> MessagingCtx {
        MessagingCtx {
            http: Arc::new(serenity::http::Http::new("fake")),
            state: new_state(),
            config: Arc::new(config),
            state_dir: "/tmp".into(),
        }
    }

    // ── check_outbound ───────────────────────────────────────────────────────

    /// Mirrors the state write that `send_dm` performs on success:
    ///   `state.record_dm_channel(user_id.get(), channel_id.get())`
    /// After that write, outbound traffic to the DM channel must be permitted.
    #[tokio::test]
    async fn dm_channel_allowed_via_record_dm_channel() {
        let user_id = 1000u64;
        let dm_channel = 2000u64;
        let ctx = messaging_ctx(test_config());
        {
            let mut state = ctx.state.write().await;
            state.record_dm_channel(user_id, dm_channel);
        }
        assert!(
            check_outbound(&ctx, ChannelId::new(dm_channel))
                .await
                .is_ok(),
            "DM channel must be allowed after record_dm_channel"
        );
    }

    #[tokio::test]
    async fn channel_not_in_config_or_state_is_denied() {
        let ctx = messaging_ctx(test_config());
        assert!(
            check_outbound(&ctx, ChannelId::new(999)).await.is_err(),
            "channel absent from config and state must be denied"
        );
    }

    #[tokio::test]
    async fn configured_channel_is_allowed() {
        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: "42".into(),
            ..Default::default()
        });
        let ctx = messaging_ctx(LoadedConfig::from_raw(raw));
        assert!(
            check_outbound(&ctx, ChannelId::new(42)).await.is_ok(),
            "channel present in config must be allowed"
        );
    }

    /// One message in the shape Discord's REST API returns from
    /// `GET /channels/{channel.id}/messages` (captured shape, trimmed to the
    /// fields serenity requires).
    fn wire_message(id: u64, content: &str, timestamp: &str, attachments: Value) -> Value {
        json!({
            "id": id.to_string(),
            "type": 0,
            "channel_id": "1080000000000000001",
            "author": {
                "id": "210987654321098765",
                "username": "miranda",
                "global_name": "Miranda",
                "avatar": null,
                "discriminator": "0",
                "public_flags": 0,
                "bot": false
            },
            "content": content,
            "timestamp": timestamp,
            "edited_timestamp": null,
            "tts": false,
            "mention_everyone": false,
            "mentions": [],
            "mention_roles": [],
            "attachments": attachments,
            "embeds": [],
            "pinned": false,
            "flags": 0,
            "components": []
        })
    }

    /// Deserializes a wire payload exactly as serenity's `Http::get_messages`
    /// does.
    fn from_wire(payload: Value) -> Vec<Message> {
        serde_json::from_value(payload).expect("captured payload must deserialize as Vec<Message>")
    }

    /// Three messages newest-first, as Discord returns them on the wire.
    fn newest_first_batch() -> Vec<Message> {
        from_wire(json!([
            wire_message(3003, "third", "2026-06-09T12:02:00.000000+00:00", json!([])),
            wire_message(
                3002,
                "second",
                "2026-06-09T12:01:00.000000+00:00",
                json!([])
            ),
            wire_message(3001, "first", "2026-06-09T12:00:00.000000+00:00", json!([])),
        ]))
    }

    #[test]
    fn new_since_sorts_oldest_first() {
        let resp = new_since_response(&test_config(), newest_first_batch(), 20);
        let ids: Vec<&str> = resp["messages"]
            .as_array()
            .expect("messages array")
            .iter()
            .map(|m| m["id"].as_str().expect("string id"))
            .collect();
        assert_eq!(
            ids,
            ["3001", "3002", "3003"],
            "messages must be sorted oldest-first regardless of wire order"
        );
    }

    #[test]
    fn new_since_count_matches_returned_messages() {
        let resp = new_since_response(&test_config(), newest_first_batch(), 20);
        assert_eq!(resp["count"], 3);
        assert_eq!(
            resp["messages"].as_array().expect("messages array").len(),
            3
        );
    }

    #[test]
    fn new_since_has_more_set_at_exactly_limit() {
        let resp = new_since_response(&test_config(), newest_first_batch(), 3);
        assert_eq!(
            resp["has_more"],
            json!(true),
            "a full page (count == limit) must signal that more may follow"
        );
    }

    #[test]
    fn new_since_has_more_unset_below_limit() {
        let resp = new_since_response(&test_config(), newest_first_batch(), 4);
        assert_eq!(
            resp["has_more"],
            json!(false),
            "a partial page must signal that the caller is caught up"
        );
    }

    #[test]
    fn new_since_empty_when_caught_up() {
        let resp = new_since_response(&test_config(), Vec::new(), 20);
        assert_eq!(resp["messages"], json!([]));
        assert_eq!(resp["count"], 0);
        assert_eq!(resp["has_more"], json!(false));
    }

    #[test]
    fn new_since_zero_limit_never_signals_more() {
        // Dispatch clamps limit to 1..=100, but the response builder must
        // stay safe on its own: limit 0 with an empty page would otherwise
        // satisfy `count == limit` vacuously and claim more data exists.
        let resp = new_since_response(&test_config(), Vec::new(), 0);
        assert_eq!(resp["count"], 0);
        assert_eq!(
            resp["has_more"],
            json!(false),
            "an empty page must never claim more data is available"
        );
    }

    #[test]
    fn new_since_message_shape_matches_fetch_messages() {
        let batch = from_wire(json!([wire_message(
            4001,
            "with attachment",
            "2026-06-09T12:00:00.000000+00:00",
            json!([{
                "id": "111",
                "filename": "photo.png",
                "size": 2048,
                "url": "https://cdn.discordapp.com/attachments/1/111/photo.png",
                "proxy_url": "https://media.discordapp.net/attachments/1/111/photo.png",
                "content_type": "image/png",
                "height": null,
                "width": null
            }])
        )]));
        let resp = new_since_response(&test_config(), batch, 20);
        let msg = &resp["messages"][0];

        let mut keys: Vec<&str> = msg
            .as_object()
            .expect("message object")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            [
                "attachments",
                "author",
                "author_id",
                "content",
                "id",
                "timestamp"
            ],
            "fetch_new_since message objects must keep wire-shape parity with fetch_messages"
        );

        let mut attachment_keys: Vec<&str> = msg["attachments"][0]
            .as_object()
            .expect("attachment object")
            .keys()
            .map(String::as_str)
            .collect();
        attachment_keys.sort_unstable();
        assert_eq!(attachment_keys, ["name", "size", "url"]);

        assert_eq!(msg["author"], "miranda");
        assert_eq!(msg["author_id"], "210987654321098765");
        assert_eq!(msg["attachments"][0]["name"], "photo.png");
        assert!(
            msg["timestamp"]
                .as_str()
                .expect("string timestamp")
                .starts_with("2026-06-09T12:00:00"),
            "timestamp must round-trip from the wire payload"
        );
    }

    // ── parse_reaction_type ──────────────────────────────────────────────────

    #[test]
    fn parse_reaction_type_rejects_zero_custom_emoji_id() {
        // Regression: `<:name:0>` used to reach `EmojiId::new(0)`, which
        // panics (serenity Ids are NonZeroU64). It must be a graceful error.
        let result = parse_reaction_type("<:name:0>");
        assert!(
            result.is_err(),
            "zero emoji ID must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn parse_reaction_type_rejects_zero_animated_emoji_id() {
        let result = parse_reaction_type("<a:party:0>");
        assert!(
            result.is_err(),
            "zero animated emoji ID must be rejected, got: {result:?}"
        );
    }

    #[test]
    fn parse_reaction_type_accepts_valid_custom_emoji() {
        use serenity::model::channel::ReactionType;

        match parse_reaction_type("<:blob:123456789012345678>") {
            Ok(ReactionType::Custom { animated, id, name }) => {
                assert!(!animated);
                assert_eq!(id.get(), 123456789012345678);
                assert_eq!(name.as_deref(), Some("blob"));
            }
            other => panic!("expected Custom reaction, got: {other:?}"),
        }
    }

    #[test]
    fn parse_reaction_type_passes_unicode_through() {
        use serenity::model::channel::ReactionType;

        assert_eq!(
            parse_reaction_type("👍"),
            Ok(ReactionType::Unicode("👍".to_string()))
        );
    }

    /// Live smoke test against the real Discord API. Ignored by default; run
    /// with `cargo nextest run --run-ignored=ignored-only -E 'test(live_fetch_new_since)'`
    /// after exporting the three environment variables named below.
    #[tokio::test]
    #[ignore = "live Discord smoke test; requires DISCORD_BOT_TOKEN, DIONE_TEST_CHANNEL_ID, DIONE_TEST_AFTER_MESSAGE_ID"]
    async fn live_fetch_new_since_smoke() {
        let token = std::env::var("DISCORD_BOT_TOKEN").expect("DISCORD_BOT_TOKEN must be set");
        let channel_id = ChannelId::new(
            std::env::var("DIONE_TEST_CHANNEL_ID")
                .expect("DIONE_TEST_CHANNEL_ID must be set")
                .parse::<u64>()
                .expect("DIONE_TEST_CHANNEL_ID must be a u64"),
        );
        let after_message_id = MessageId::new(
            std::env::var("DIONE_TEST_AFTER_MESSAGE_ID")
                .expect("DIONE_TEST_AFTER_MESSAGE_ID must be set")
                .parse::<u64>()
                .expect("DIONE_TEST_AFTER_MESSAGE_ID must be a u64"),
        );

        let mut raw = Config::default();
        raw.channels.push(ChannelConfig {
            id: channel_id.get().to_string(),
            require_mention: false,
            allow_from: vec![],
            ..Default::default()
        });

        let dir = tempfile::TempDir::new().expect("tempdir");
        let state_dir = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).expect("utf-8 path");
        let ctx = MessagingCtx {
            http: Arc::new(serenity::http::Http::new(&token)),
            state: crate::state::new_state(),
            config: Arc::new(LoadedConfig::from_raw(raw)),
            state_dir,
        };

        let resp = fetch_new_since(&ctx, channel_id, after_message_id, 20).await;
        assert!(resp.get("error").is_none(), "live fetch failed: {resp}");

        let messages = resp["messages"].as_array().expect("messages array");
        assert_eq!(resp["count"], messages.len() as u64);
        let ids: Vec<u64> = messages
            .iter()
            .map(|m| {
                m["id"]
                    .as_str()
                    .expect("string id")
                    .parse()
                    .expect("u64 id")
            })
            .collect();
        assert!(
            ids.windows(2).all(|w| w[0] < w[1]),
            "ids must be strictly ascending: {ids:?}"
        );
        assert!(
            ids.iter().all(|&id| id > after_message_id.get()),
            "all returned ids must be after the cursor {}: {ids:?}",
            after_message_id.get()
        );
    }

    mod proptests {
        use proptest::prelude::*;

        use super::*;

        /// Unique nonzero snowflakes in arbitrary (shuffled) order, mimicking
        /// any ordering Discord could put on the wire.
        fn ids_strategy() -> impl Strategy<Value = Vec<u64>> {
            prop::collection::hash_set(1u64..=u64::MAX, 0..=8)
                .prop_map(|set| set.into_iter().collect::<Vec<_>>())
                .prop_shuffle()
        }

        fn batch_from_ids(ids: &[u64]) -> Vec<Message> {
            from_wire(json!(
                ids.iter()
                    .map(|&id| wire_message(id, "m", "2026-06-09T12:00:00.000000+00:00", json!([])))
                    .collect::<Vec<_>>()
            ))
        }

        proptest! {
            /// The full `fetch_new_since` response contract, for any wire
            /// ordering and any limit (including the 0 edge case):
            ///
            /// 1. `count` always equals `messages.len()`
            /// 2. `has_more` is true iff `count == limit` and `limit >= 1` —
            ///    an empty page must never claim more data (the limit-0 bug)
            /// 3. messages are in chronological order (oldest first)
            /// 4. no messages are invented or dropped
            #[test]
            fn new_since_response_invariants(ids in ids_strategy(), limit in 0u8..=100) {
                let resp = new_since_response(&test_config(), batch_from_ids(&ids), limit);

                let msgs = resp["messages"].as_array().expect("messages array");
                prop_assert_eq!(
                    resp["count"].as_u64().expect("count"),
                    msgs.len() as u64,
                    "count must equal messages.len()"
                );

                let expected_more = limit >= 1 && msgs.len() == usize::from(limit);
                prop_assert_eq!(
                    resp["has_more"].as_bool().expect("has_more"),
                    expected_more,
                    "has_more must be true iff a full page (>= 1) was returned"
                );

                let out_ids: Vec<u64> = msgs
                    .iter()
                    .map(|m| m["id"].as_str().expect("string id").parse().expect("u64 id"))
                    .collect();
                prop_assert!(
                    out_ids.windows(2).all(|w| w[0] < w[1]),
                    "ids must be strictly ascending (oldest first): {:?}",
                    out_ids
                );

                let mut sorted_input = ids.clone();
                sorted_input.sort_unstable();
                prop_assert_eq!(
                    out_ids,
                    sorted_input,
                    "response must be a permutation of the input batch"
                );
            }
        }
    }
}
