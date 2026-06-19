//! Compact batch serialization for coalesced Discord messages.
//!
//! When messages are buffered by [`crate::delivery_buffer::DeliveryBuffer`] and
//! flushed together, this module serializes them into a compact, human-readable
//! text format that uses far fewer tokens than individual JSON-RPC notifications.
//!
//! # Format
//!
//! ```text
//! [batch ch=CHANNEL_ID thread=THREAD_ID n=COUNT tz=TZ latest=MSG_ID]
//! [users shortname1=USER_ID1 shortname2=USER_ID2 ...]
//!
//! MSG_ID|LOCAL_TS|SHORT_NAME
//! message content here
//!
//! MSG_ID|LOCAL_TS|SHORT_NAME|>REPLY_TO_MSG_ID
//! reply content here
//!
//! MSG_ID|LOCAL_TS|SHORT_NAME|+N_ATTACHMENTS
//! message with attachments
//! ```

use crate::discord::events::NotificationEvent;
use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use std::collections::BTreeMap;
use std::fmt::Write;

// ── Types ────────────────────────────────────────────────────────────────────

/// Channel context for batch serialization.
pub struct BatchContext {
    /// The channel ID where the messages were sent.
    pub channel_id: u64,
    /// If the messages are in a thread, the parent channel ID.
    pub thread_id: Option<u64>,
    /// IANA timezone for timestamp localization (e.g. "America/Los_Angeles").
    /// If `None`, timestamps are rendered in UTC.
    pub tz: Option<Tz>,
}

/// Error type for batch serialization.
#[derive(Debug, thiserror::Error)]
pub enum BatchError {
    #[error("no messages to serialize")]
    Empty,
    #[error("event is not a Message variant")]
    NotAMessage,
    #[error("failed to parse timestamp `{ts}`: {source}")]
    TimestampParse {
        ts: String,
        source: chrono::ParseError,
    },
    #[error("format error: {0}")]
    Fmt(#[from] std::fmt::Error),
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Serialize a batch of notification events into the compact batch format.
///
/// Only [`NotificationEvent::Message`] variants are accepted; other variants
/// return [`BatchError::NotAMessage`].
///
/// Events must all belong to the same channel (the caller — typically
/// [`crate::delivery_buffer::DeliveryBuffer`] — guarantees this).
pub fn serialize_batch(
    events: &[NotificationEvent],
    ctx: &BatchContext,
) -> Result<String, BatchError> {
    if events.is_empty() {
        return Err(BatchError::Empty);
    }

    let messages = extract_messages(events)?;
    let roster = build_roster(&messages);
    let latest_id = messages.last().map(|m| m.message_id).unwrap_or(0);

    let mut out = String::new();

    // Header line.
    write_header(&mut out, ctx, messages.len(), latest_id)?;

    // User roster line.
    write_roster(&mut out, &roster)?;

    // Blank line separator.
    out.push('\n');

    // Message entries.
    for (i, msg) in messages.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        write_message(&mut out, msg, &roster, ctx.tz)?;
    }

    Ok(out)
}

// ── Internal types ───────────────────────────────────────────────────────────

/// Extracted message fields from a `NotificationEvent::Message`.
struct MessageFields {
    message_id: u64,
    user: String,
    user_id: u64,
    content: String,
    timestamp: String,
    reply_to_message_id: Option<u64>,
    attachment_count: usize,
}

// ── Extraction ───────────────────────────────────────────────────────────────

fn extract_messages(events: &[NotificationEvent]) -> Result<Vec<MessageFields>, BatchError> {
    events.iter().map(extract_one).collect()
}

fn extract_one(event: &NotificationEvent) -> Result<MessageFields, BatchError> {
    match event {
        NotificationEvent::Message {
            message_id,
            user,
            user_id,
            content,
            timestamp,
            attachments,
            reply_to_message_id,
            ..
        } => Ok(MessageFields {
            message_id: message_id.get(),
            user: user.clone(),
            user_id: user_id.get(),
            content: content.clone(),
            timestamp: timestamp.clone(),
            reply_to_message_id: reply_to_message_id.map(|id| id.get()),
            attachment_count: attachments.len(),
        }),
        _ => Err(BatchError::NotAMessage),
    }
}

// ── Roster ───────────────────────────────────────────────────────────────────

/// User roster: maps short name -> user ID. Uses BTreeMap for deterministic
/// ordering in the serialized output.
type Roster = BTreeMap<String, u64>;

fn build_roster(messages: &[MessageFields]) -> Roster {
    let mut roster = Roster::new();
    for msg in messages {
        roster.entry(msg.user.clone()).or_insert(msg.user_id);
    }
    roster
}

// ── Timestamp formatting ─────────────────────────────────────────────────────

/// Format a timestamp string for batch output.
///
/// If a timezone is configured, converts to local time and emits a compact
/// `HH:MM` or `HH:MM:SS` format. Otherwise emits the raw timestamp.
fn format_timestamp(raw: &str, tz: Option<Tz>) -> Result<String, BatchError> {
    let dt =
        raw.parse::<DateTime<chrono::FixedOffset>>()
            .map_err(|e| BatchError::TimestampParse {
                ts: raw.to_owned(),
                source: e,
            })?;

    match tz {
        Some(tz) => {
            let local = dt.with_timezone(&tz);
            // Compact format: HH:MM if seconds are zero, HH:MM:SS otherwise.
            if local.format("%S").to_string() == "00" {
                Ok(local.format("%H:%M").to_string())
            } else {
                Ok(local.format("%H:%M:%S").to_string())
            }
        }
        None => {
            let utc = dt.with_timezone(&Utc);
            if utc.format("%S").to_string() == "00" {
                Ok(utc.format("%H:%M").to_string())
            } else {
                Ok(utc.format("%H:%M:%S").to_string())
            }
        }
    }
}

// ── Writers ──────────────────────────────────────────────────────────────────

fn write_header(
    out: &mut String,
    ctx: &BatchContext,
    count: usize,
    latest_id: u64,
) -> Result<(), BatchError> {
    write!(out, "[batch ch={}", ctx.channel_id)?;

    if let Some(thread_id) = ctx.thread_id {
        write!(out, " thread={thread_id}")?;
    }

    write!(out, " n={count}")?;

    if let Some(tz) = ctx.tz {
        write!(out, " tz={tz}")?;
    }

    write!(out, " latest={latest_id}")?;
    writeln!(out, "]")?;

    Ok(())
}

fn write_roster(out: &mut String, roster: &Roster) -> Result<(), BatchError> {
    write!(out, "[users")?;
    for (name, id) in roster {
        write!(out, " {name}={id}")?;
    }
    writeln!(out, "]")?;
    Ok(())
}

fn write_message(
    out: &mut String,
    msg: &MessageFields,
    roster: &Roster,
    tz: Option<Tz>,
) -> Result<(), BatchError> {
    let ts = format_timestamp(&msg.timestamp, tz)?;

    // Find the short name for this user.
    let short_name = roster
        .iter()
        .find(|&(_, id)| *id == msg.user_id)
        .map(|(name, _)| name.as_str())
        .unwrap_or(&msg.user);

    // Header: MSG_ID|LOCAL_TS|SHORT_NAME[|suffix]
    write!(out, "{}|{}|{}", msg.message_id, ts, short_name)?;

    if let Some(reply_to) = msg.reply_to_message_id {
        write!(out, "|>{reply_to}")?;
    }

    if msg.attachment_count > 0 {
        write!(out, "|+{}", msg.attachment_count)?;
    }

    writeln!(out)?;

    // Content body.
    writeln!(out, "{}", msg.content)?;

    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::events::AttachmentMeta;
    use serenity::model::id::{ChannelId, MessageId, UserId};

    fn msg(message_id: u64, user: &str, user_id: u64, content: &str) -> NotificationEvent {
        NotificationEvent::Message {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(message_id),
            user: user.to_string(),
            user_id: UserId::new(user_id),
            content: content.to_string(),
            timestamp: "2026-06-19T15:30:00+00:00".to_string(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
        }
    }

    fn msg_with_reply(
        message_id: u64,
        user: &str,
        user_id: u64,
        content: &str,
        reply_to: u64,
    ) -> NotificationEvent {
        NotificationEvent::Message {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(message_id),
            user: user.to_string(),
            user_id: UserId::new(user_id),
            content: content.to_string(),
            timestamp: "2026-06-19T15:30:00+00:00".to_string(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(reply_to)),
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
        }
    }

    fn msg_with_attachments(
        message_id: u64,
        user: &str,
        user_id: u64,
        content: &str,
        attachment_count: usize,
    ) -> NotificationEvent {
        let attachments = (0..attachment_count)
            .map(|i| AttachmentMeta {
                name: format!("file{i}.png"),
                content_type: Some("image/png".to_string()),
                size: 1024,
            })
            .collect();

        NotificationEvent::Message {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(message_id),
            user: user.to_string(),
            user_id: UserId::new(user_id),
            content: content.to_string(),
            timestamp: "2026-06-19T15:30:00+00:00".to_string(),
            attachments,
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
        }
    }

    fn ctx_basic() -> BatchContext {
        BatchContext {
            channel_id: 100,
            thread_id: None,
            tz: None,
        }
    }

    fn ctx_with_thread() -> BatchContext {
        BatchContext {
            channel_id: 100,
            thread_id: Some(200),
            tz: None,
        }
    }

    fn ctx_with_tz() -> BatchContext {
        BatchContext {
            channel_id: 100,
            thread_id: None,
            tz: Some("America/Los_Angeles".parse().expect("valid tz")),
        }
    }

    // ── Basic serialization ──────────────────────────────────────────────────

    #[test]
    fn single_message() {
        let events = vec![msg(1001, "alice", 42, "hello world")];
        let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

        assert!(result.starts_with("[batch ch=100 n=1 latest=1001]\n"));
        assert!(result.contains("[users alice=42]\n"));
        assert!(result.contains("1001|"));
        assert!(result.contains("|alice\n"));
        assert!(result.contains("hello world\n"));
    }

    #[test]
    fn multiple_messages_multiple_users() {
        let events = vec![
            msg(1001, "alice", 42, "hello"),
            msg(1002, "bob", 99, "hi there"),
            msg(1003, "alice", 42, "how are you?"),
        ];
        let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

        // Header should show n=3 and latest=1003.
        assert!(result.contains("n=3"));
        assert!(result.contains("latest=1003"));

        // Roster should have both users (BTreeMap: alphabetical order).
        assert!(result.contains("[users alice=42 bob=99]"));

        // All three messages should be present.
        assert!(result.contains("hello\n"));
        assert!(result.contains("hi there\n"));
        assert!(result.contains("how are you?\n"));
    }

    #[test]
    fn user_deduplication() {
        let events = vec![
            msg(1001, "alice", 42, "first"),
            msg(1002, "alice", 42, "second"),
            msg(1003, "alice", 42, "third"),
        ];
        let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

        // Roster should only list alice once.
        let roster_line = result.lines().nth(1).expect("roster line");
        assert_eq!(roster_line, "[users alice=42]");
    }

    // ── Thread context ───────────────────────────────────────────────────────

    #[test]
    fn thread_id_in_header() {
        let events = vec![msg(1001, "alice", 42, "in a thread")];
        let result = serialize_batch(&events, &ctx_with_thread()).expect("should serialize");

        assert!(result.starts_with("[batch ch=100 thread=200 n=1 latest=1001]\n"));
    }

    // ── Timezone handling ────────────────────────────────────────────────────

    #[test]
    fn timezone_in_header_and_timestamps() {
        let events = vec![msg(1001, "alice", 42, "morning!")];
        let result = serialize_batch(&events, &ctx_with_tz()).expect("should serialize");

        // Header should include tz.
        assert!(result.contains("tz=America/Los_Angeles"));

        // 15:30 UTC -> 08:30 PDT (June = daylight saving).
        assert!(result.contains("08:30"));
    }

    #[test]
    fn utc_fallback_when_no_tz() {
        let events = vec![msg(1001, "alice", 42, "utc time")];
        let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

        // Should show UTC time: 15:30.
        assert!(result.contains("15:30"));
        // Should NOT contain tz= in header.
        assert!(!result.contains("tz="));
    }

    #[test]
    fn timestamp_with_nonzero_seconds() {
        let events = vec![NotificationEvent::Message {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(1001),
            user: "alice".to_string(),
            user_id: UserId::new(42),
            content: "with seconds".to_string(),
            timestamp: "2026-06-19T15:30:45+00:00".to_string(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
        }];
        let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

        // Non-zero seconds should show HH:MM:SS.
        assert!(result.contains("15:30:45"));
    }

    // ── Reply-to ─────────────────────────────────────────────────────────────

    #[test]
    fn reply_to_suffix() {
        let events = vec![msg_with_reply(1002, "bob", 99, "yes!", 1001)];
        let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

        assert!(result.contains("|>1001\n"));
    }

    // ── Attachments ──────────────────────────────────────────────────────────

    #[test]
    fn attachment_count_suffix() {
        let events = vec![msg_with_attachments(1003, "carol", 77, "look at this", 3)];
        let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

        assert!(result.contains("|+3\n"));
    }

    #[test]
    fn reply_and_attachments_both_present() {
        let events = vec![NotificationEvent::Message {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(1004),
            user: "dave".to_string(),
            user_id: UserId::new(55),
            content: "replying with files".to_string(),
            timestamp: "2026-06-19T15:30:00+00:00".to_string(),
            attachments: vec![AttachmentMeta {
                name: "doc.pdf".to_string(),
                content_type: Some("application/pdf".to_string()),
                size: 2048,
            }],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: Some(MessageId::new(1000)),
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
        }];
        let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

        // Should have both suffixes.
        assert!(result.contains("|>1000|+1\n"));
    }

    // ── Error cases ──────────────────────────────────────────────────────────

    #[test]
    fn empty_events_returns_error() {
        let events: Vec<NotificationEvent> = vec![];
        let result = serialize_batch(&events, &ctx_basic());
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BatchError::Empty));
    }

    #[test]
    fn non_message_event_returns_error() {
        let events = vec![NotificationEvent::Reaction {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(1),
            user: "alice".to_string(),
            user_id: UserId::new(42),
            emoji: "thumbsup".to_string(),
        }];
        let result = serialize_batch(&events, &ctx_basic());
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), BatchError::NotAMessage));
    }

    // ── Full round-trip ──────────────────────────────────────────────────────

    #[test]
    fn full_conversation_batch() {
        let tz: Tz = "US/Eastern".parse().expect("valid tz");
        let ctx = BatchContext {
            channel_id: 555,
            thread_id: Some(666),
            tz: Some(tz),
        };

        let events = vec![
            NotificationEvent::Message {
                chat_id: ChannelId::new(555),
                message_id: MessageId::new(2001),
                user: "lina".to_string(),
                user_id: UserId::new(10),
                content: "hey, check this out".to_string(),
                timestamp: "2026-06-19T20:00:00+00:00".to_string(),
                attachments: vec![],
                is_voice_message: false,
                thread_parent_id: Some(ChannelId::new(666)),
                reply_to_message_id: None,
                reply_to_user_id: None,
                reply_to_user: None,
                reply_to_content_preview: None,
            },
            NotificationEvent::Message {
                chat_id: ChannelId::new(555),
                message_id: MessageId::new(2002),
                user: "ros".to_string(),
                user_id: UserId::new(20),
                content: "oh nice!".to_string(),
                timestamp: "2026-06-19T20:01:30+00:00".to_string(),
                attachments: vec![],
                is_voice_message: false,
                thread_parent_id: Some(ChannelId::new(666)),
                reply_to_message_id: Some(MessageId::new(2001)),
                reply_to_user_id: Some(UserId::new(10)),
                reply_to_user: Some("lina".to_string()),
                reply_to_content_preview: Some("hey, check this out".to_string()),
            },
            NotificationEvent::Message {
                chat_id: ChannelId::new(555),
                message_id: MessageId::new(2003),
                user: "lina".to_string(),
                user_id: UserId::new(10),
                content: "here's the file".to_string(),
                timestamp: "2026-06-19T20:02:00+00:00".to_string(),
                attachments: vec![AttachmentMeta {
                    name: "design.png".to_string(),
                    content_type: Some("image/png".to_string()),
                    size: 4096,
                }],
                is_voice_message: false,
                thread_parent_id: Some(ChannelId::new(666)),
                reply_to_message_id: None,
                reply_to_user_id: None,
                reply_to_user: None,
                reply_to_content_preview: None,
            },
        ];

        let result = serialize_batch(&events, &ctx).expect("should serialize");

        // Verify structure.
        let lines: Vec<&str> = result.lines().collect();

        // Header: ch=555, thread=666, n=3, tz=US/Eastern, latest=2003
        assert_eq!(
            lines[0],
            "[batch ch=555 thread=666 n=3 tz=US/Eastern latest=2003]"
        );

        // Roster: lina and ros (alphabetical).
        assert_eq!(lines[1], "[users lina=10 ros=20]");

        // Blank separator.
        assert_eq!(lines[2], "");

        // 20:00 UTC -> 16:00 EDT (June = daylight saving, US/Eastern = -4).
        assert!(lines[3].starts_with("2001|16:00|lina"));
        assert_eq!(lines[4], "hey, check this out");

        // Reply: 20:01:30 UTC -> 16:01:30 EDT.
        assert!(lines[6].contains("|>2001"));
        assert!(lines[6].contains("16:01:30"));
        assert_eq!(lines[7], "oh nice!");

        // Attachment: 20:02 UTC -> 16:02 EDT.
        assert!(lines[9].contains("|+1"));
        assert_eq!(lines[10], "here's the file");
    }

    // ── Multiline content ────────────────────────────────────────────────────

    #[test]
    fn multiline_message_content() {
        let events = vec![NotificationEvent::Message {
            chat_id: ChannelId::new(100),
            message_id: MessageId::new(1001),
            user: "alice".to_string(),
            user_id: UserId::new(42),
            content: "line one\nline two\nline three".to_string(),
            timestamp: "2026-06-19T15:30:00+00:00".to_string(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
        }];
        let result = serialize_batch(&events, &ctx_basic()).expect("should serialize");

        // The content should be preserved as-is (multiline).
        assert!(result.contains("line one\nline two\nline three\n"));
    }
}
