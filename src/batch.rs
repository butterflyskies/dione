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
//! [users USER_ID1=shortname1 USER_ID2=shortname2 ...]
//!
//! MSG_ID|LOCAL_TS|SHORT_NAME|L=LINECOUNT
//! message content here
//!
//! MSG_ID|LOCAL_TS|SHORT_NAME|L=LINECOUNT|>REPLY_TO_MSG_ID
//! reply content here
//!
//! MSG_ID|LOCAL_TS|SHORT_NAME|L=LINECOUNT|+N_ATTACHMENTS
//! message with attachments
//! ```

use crate::{
    discord::events::{MessageEvent, NotificationEvent},
    timestamp::{Timestamp, format_compact},
};
use chrono_tz::Tz;
use serenity::model::id::{ChannelId, UserId};
use std::{collections::BTreeSet, fmt::Write};

// ── Types ────────────────────────────────────────────────────────────────────

/// Channel context for batch serialization.
pub struct BatchContext {
    /// The channel ID where the messages were sent.
    pub channel_id: ChannelId,
    /// If the messages are in a thread, the parent channel ID.
    pub thread_id: Option<ChannelId>,
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
    #[error("event channel {event_channel} does not match batch channel {batch_channel}")]
    ChannelMismatch {
        event_channel: ChannelId,
        batch_channel: ChannelId,
    },
    /// Structurally required by the `From<std::fmt::Error>` derive but
    /// unreachable in practice — all `write!` / `writeln!` calls target a
    /// `String`, whose `fmt::Write` impl is infallible.
    #[error("format error: {0}")]
    Fmt(#[from] std::fmt::Error),
}

// ── Trait ───────────────────────────────────────────────────────────────────

/// Visible participant roster. A single effective identity may legitimately
/// have multiple message display names (for example PluralKit members), so the
/// pair—not only the ID—is the deduplication key.
type Roster<'a> = BTreeSet<(UserId, &'a str)>;

/// Trait for writing a single entry in the compact batch wire format.
pub trait BatchSerialize {
    /// Write this item as a batch entry into `out`, using `roster` for
    /// user-id-to-name resolution and `tz` for timestamp localization.
    fn write_batch_entry(
        &self,
        out: &mut String,
        roster: &Roster<'_>,
        tz: Option<Tz>,
    ) -> Result<(), BatchError>;
}

// ── Public API ───────────────────────────────────────────────────────────────

/// Serialize a batch of notification events into the compact batch format.
///
/// Only [`NotificationEvent::Message`] variants are accepted; other variants
/// return [`BatchError::NotAMessage`]. Every event's `chat_id` must match
/// `ctx.channel_id`; a mismatch returns [`BatchError::ChannelMismatch`].
pub fn serialize_batch(
    events: &[NotificationEvent],
    ctx: &BatchContext,
) -> Result<String, BatchError> {
    if events.is_empty() {
        return Err(BatchError::Empty);
    }

    let messages = extract_messages(events, ctx.channel_id)?;
    let roster = build_roster(&messages);
    let latest_id = messages
        .last()
        .map(|m| m.message_id)
        .expect("checked non-empty above");

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
        msg.write_batch_entry(&mut out, &roster, ctx.tz)?;
    }

    Ok(out)
}

// ── Extraction ───────────────────────────────────────────────────────────────

fn extract_messages(
    events: &[NotificationEvent],
    expected_channel: ChannelId,
) -> Result<Vec<&MessageEvent>, BatchError> {
    events
        .iter()
        .map(|e| extract_one(e, expected_channel))
        .collect()
}

fn extract_one(
    event: &NotificationEvent,
    expected_channel: ChannelId,
) -> Result<&MessageEvent, BatchError> {
    match event {
        NotificationEvent::Message(msg) => {
            if msg.chat_id != expected_channel {
                return Err(BatchError::ChannelMismatch {
                    event_channel: msg.chat_id,
                    batch_channel: expected_channel,
                });
            }
            Ok(msg)
        }
        _ => Err(BatchError::NotAMessage),
    }
}

// ── Roster ───────────────────────────────────────────────────────────────────

fn build_roster<'a>(messages: &[&'a MessageEvent]) -> Roster<'a> {
    let mut roster = Roster::new();
    for msg in messages {
        roster.insert((msg.user_id, msg.user.as_str()));
    }
    roster
}

// ── Timestamp formatting ─────────────────────────────────────────────────────

/// Format a [`Timestamp`] for batch output.
///
/// Delegates to [`crate::timestamp::format_compact`] for the shared compact
/// `HH:MM` / `HH:MM:SS` logic.
fn format_timestamp(ts: &Timestamp, tz: Option<Tz>) -> String {
    format_compact(ts, tz)
}

// ── Writers ──────────────────────────────────────────────────────────────────

fn write_header(
    out: &mut String,
    ctx: &BatchContext,
    count: usize,
    latest_id: serenity::model::id::MessageId,
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

fn write_roster(out: &mut String, roster: &Roster<'_>) -> Result<(), BatchError> {
    write!(out, "[users")?;
    for (id, name) in roster {
        write!(out, " {id}={name}")?;
    }
    writeln!(out, "]")?;
    Ok(())
}

// ── MessageEvent helpers ────────────────────────────────────────────────────

impl MessageEvent {
    /// Content with trailing newlines stripped, for line-count-accurate
    /// serialization.
    ///
    /// Without this, `"text\n".lines()` returns 1 but `writeln!("{}", "text\n")`
    /// emits 2 lines (off-by-one), and `"".lines()` returns 0 but
    /// `writeln!("{}", "")` emits a line (desync).
    pub(crate) fn normalized_content(&self) -> &str {
        self.content.trim_end_matches('\n')
    }

    /// Number of lines in the normalized content. Returns 0 for empty content.
    pub(crate) fn content_line_count(&self) -> usize {
        let content = self.normalized_content();
        if content.is_empty() {
            0
        } else {
            content.lines().count()
        }
    }
}

// ── BatchSerialize impl ────────────────────────────────────────────────────

impl BatchSerialize for MessageEvent {
    /// Write this message as a single batch entry.
    ///
    /// Produces the compact wire format:
    /// ```text
    /// MSG_ID|LOCAL_TS|SHORT_NAME|L=LINECOUNT[|>REPLY_TO][|+ATTACHMENTS]
    /// content lines...
    /// ```
    fn write_batch_entry(
        &self,
        out: &mut String,
        _roster: &Roster<'_>,
        tz: Option<Tz>,
    ) -> Result<(), BatchError> {
        let ts = format_timestamp(&self.timestamp, tz);

        let content = self.normalized_content();
        let line_count = self.content_line_count();

        // Header: MSG_ID|LOCAL_TS|SHORT_NAME|L=LINECOUNT[|suffix].
        // IDs use Display at the serialization boundary — no early .get().
        write!(
            out,
            "{}|{}|{}|L={}",
            self.message_id, ts, self.user, line_count
        )?;

        if let Some(reply_to) = self.reply_to_message_id {
            write!(out, "|>{reply_to}")?;
        }

        let attachment_count = self.attachments.len();
        if attachment_count > 0 {
            write!(out, "|+{attachment_count}")?;
        }

        writeln!(out)?;

        // Content body — only emit if non-empty. For L=0 (empty content), the
        // parser expects zero content lines between the header and the next
        // blank separator (or EOF).
        if !content.is_empty() {
            writeln!(out, "{content}")?;
        }

        Ok(())
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
