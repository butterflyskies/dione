//! Event coalescing: merge buffered events into a single delivery envelope.
//!
//! When [`crate::delivery_buffer::DeliveryBuffer`] flushes multiple events at once (because the
//! delivery delay window expired), this module combines them into a single
//! MCP notification. This means the LLM receives one prompt injection per
//! batch window instead of N separate injections for N events.
//!
//! # Format
//!
//! The coalesced envelope uses a compact text format:
//!
//! ```text
//! [events ch=CHANNEL_ID n=COUNT tz=TZ]
//! [users USER_ID1=name1 USER_ID2=name2]
//!
//! MSG_ID|HH:MM|name|L=LINES
//! message content
//!
//! !edit|MSG_ID|HH:MM|name|L=LINES
//! new content after edit
//!
//! !delete|MSG_ID
//!
//! !react|MSG_ID|name|:emoji:
//! ```
//!
//! For single-event flushes, events pass through as individual notifications
//! (no coalescing overhead). Coalescing only activates for 2+ events.

use crate::batch::{BatchContext, serialize_batch};
use crate::discord::events::{MessageEvent, NotificationEvent};
use crate::timestamp::{Timestamp, format_compact};
use chrono_tz::Tz;
use serde_json::{Value, json};
use serenity::model::id::{ChannelId, UserId};
use std::collections::BTreeMap;
use std::fmt::Write;

// ── Public API ──────────────────────────────────────────────────────────────

/// Result of attempting to coalesce a batch of flushed events.
#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
pub enum CoalesceResult {
    /// Only one event — return it as an individual notification (no batching).
    Single(NotificationEvent),
    /// Multiple events coalesced into a single MCP notification payload.
    Coalesced(Value),
}

/// Coalesce a vector of flushed events into a single delivery envelope.
///
/// - Empty input: returns `None`
/// - Single event: returns `CoalesceResult::Single` (caller handles as normal)
/// - Multiple events, all messages from same channel: uses the existing compact
///   batch format from `batch.rs`
/// - Multiple events with mixed types: uses the heterogeneous envelope format
pub fn coalesce(events: Vec<NotificationEvent>, tz: Option<Tz>) -> Option<CoalesceResult> {
    if events.is_empty() {
        return None;
    }

    if events.len() == 1 {
        return Some(CoalesceResult::Single(
            events.into_iter().next().expect("checked len == 1"),
        ));
    }

    // Group events by channel. Events from different channels get separate
    // envelopes. Non-channel events (Trace, ConfigError, PermissionResponse)
    // get their own group keyed by channel_id = 0.
    let groups = group_by_channel(events);

    // For each channel group, produce a coalesced notification.
    let mut notifications = Vec::new();
    for (_channel_id, channel_events) in groups {
        let notification = coalesce_channel_group(channel_events, tz);
        notifications.push(notification);
    }

    // If we ended up with exactly one notification (all events were same channel),
    // return it directly.
    if notifications.len() == 1 {
        return Some(CoalesceResult::Coalesced(
            notifications.into_iter().next().unwrap(),
        ));
    }

    // Multiple channel groups: concatenate all content sections into a single
    // standard notification. Each per-channel content block is already self-
    // describing (has its own [events]/[batch] header with channel ID), so they
    // can be concatenated with a newline separator. (Each content block already
    // ends with a trailing newline, so a single `\n` produces a blank line
    // between blocks.)
    let mut combined_content = String::new();
    for (i, notification) in notifications.iter().enumerate() {
        if i > 0 {
            combined_content.push('\n');
        }
        if let Some(content) = notification["params"]["content"].as_str() {
            combined_content.push_str(content);
        }
    }

    // Use the first channel's ID as the primary; meta signals multi-channel.
    let first_channel_id = notifications
        .first()
        .and_then(|n| n["params"]["meta"]["chat_id"].as_str())
        .unwrap_or("0");

    let combined = json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": combined_content,
            "meta": {
                "chat_id": first_channel_id,
                "type": "batch",
                "format": "multi_channel",
            },
        }
    });
    Some(CoalesceResult::Coalesced(combined))
}

// ── Channel grouping ────────────────────────────────────────────────────────

fn event_channel_id(event: &NotificationEvent) -> u64 {
    match event {
        NotificationEvent::Message(msg) => msg.chat_id.get(),
        NotificationEvent::MessageEdit { chat_id, .. }
        | NotificationEvent::MessageDelete { chat_id, .. }
        | NotificationEvent::Reaction { chat_id, .. } => chat_id.get(),
        // Non-channel events get grouped under sentinel 0.
        _ => 0,
    }
}

/// Group events by channel ID, preserving insertion order within each group.
/// Uses BTreeMap for deterministic cross-channel ordering.
fn group_by_channel(events: Vec<NotificationEvent>) -> BTreeMap<u64, Vec<NotificationEvent>> {
    let mut groups: BTreeMap<u64, Vec<NotificationEvent>> = BTreeMap::new();
    for event in events {
        let ch = event_channel_id(&event);
        groups.entry(ch).or_default().push(event);
    }
    groups
}

// ── Single-channel coalescing ───────────────────────────────────────────────

/// Check if all events in a group are Message variants.
fn all_messages(events: &[NotificationEvent]) -> bool {
    events
        .iter()
        .all(|e| matches!(e, NotificationEvent::Message(_)))
}

/// Extract thread_parent_id from the first event that has one.
fn find_thread_parent(events: &[NotificationEvent]) -> Option<ChannelId> {
    for event in events {
        match event {
            NotificationEvent::Message(msg) if msg.thread_parent_id.is_some() => {
                return msg.thread_parent_id;
            }
            NotificationEvent::MessageEdit {
                thread_parent_id, ..
            } if thread_parent_id.is_some() => {
                return *thread_parent_id;
            }
            NotificationEvent::MessageDelete {
                thread_parent_id, ..
            } if thread_parent_id.is_some() => {
                return *thread_parent_id;
            }
            _ => {}
        }
    }
    None
}

/// Coalesce events from a single channel into one notification.
fn coalesce_channel_group(events: Vec<NotificationEvent>, tz: Option<Tz>) -> Value {
    debug_assert!(!events.is_empty());

    let channel_id = event_channel_id(&events[0]);

    // Non-channel events (sentinel 0): serialize individually.
    if channel_id == 0 {
        return coalesce_non_channel_events(events);
    }

    let ch = ChannelId::new(channel_id);
    let thread_id = find_thread_parent(&events);

    // Fast path: if all events are messages, use the existing compact
    // batch format which is already optimized for message batches.
    if all_messages(&events) {
        let ctx = BatchContext {
            channel_id: ch,
            thread_id,
            tz,
        };
        match serialize_batch(&events, &ctx) {
            Ok(batch_text) => {
                return json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/claude/channel",
                    "params": {
                        "content": batch_text,
                        "meta": {
                            "chat_id": channel_id.to_string(),
                            "type": "batch",
                            "format": "batch_v1",
                        },
                    }
                });
            }
            Err(e) => {
                tracing::warn!(error = %e, "batch serialization failed, falling back to heterogeneous format");
                // Fall through to heterogeneous format.
            }
        }
    }

    // Heterogeneous format: mix of messages, reactions, edits, deletes.
    let content = serialize_heterogeneous(&events, ch, thread_id, tz);

    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": content,
            "meta": {
                "chat_id": channel_id.to_string(),
                "type": "batch",
                "format": "events_v1",
            },
        }
    })
}

// ── Heterogeneous serialization ─────────────────────────────────────────────

/// Normalize content for serialization: strip trailing newlines.
///
/// Mirrors [`MessageEvent::normalized_content`] for non-message content strings.
fn normalize_content(s: &str) -> &str {
    s.trim_end_matches('\n')
}

/// Count lines in normalized content. Returns 0 for empty strings.
///
/// Mirrors [`MessageEvent::content_line_count`] for non-message content strings.
fn count_lines(s: &str) -> usize {
    if s.is_empty() { 0 } else { s.lines().count() }
}

/// User roster for heterogeneous batches.
type Roster = BTreeMap<UserId, String>;

fn build_heterogeneous_roster(events: &[NotificationEvent]) -> Roster {
    let mut roster = Roster::new();
    for event in events {
        match event {
            NotificationEvent::Message(msg) => {
                roster
                    .entry(msg.user_id)
                    .or_insert_with(|| msg.user.clone());
            }
            NotificationEvent::Reaction { user_id, user, .. } => {
                roster.entry(*user_id).or_insert_with(|| user.clone());
            }
            NotificationEvent::MessageEdit { user_id, user, .. } => {
                roster.entry(*user_id).or_insert_with(|| user.clone());
            }
            _ => {}
        }
    }
    roster
}

fn serialize_heterogeneous(
    events: &[NotificationEvent],
    channel_id: ChannelId,
    thread_id: Option<ChannelId>,
    tz: Option<Tz>,
) -> String {
    let roster = build_heterogeneous_roster(events);
    let mut out = String::new();

    // Header.
    write!(out, "[events ch={channel_id}").unwrap();
    if let Some(tid) = thread_id {
        write!(out, " thread={tid}").unwrap();
    }
    write!(out, " n={}", events.len()).unwrap();
    if let Some(tz) = tz {
        write!(out, " tz={tz}").unwrap();
    }
    writeln!(out, "]").unwrap();

    // User roster.
    if !roster.is_empty() {
        write!(out, "[users").unwrap();
        for (id, name) in &roster {
            write!(out, " {id}={name}").unwrap();
        }
        writeln!(out, "]").unwrap();
    }

    // Blank separator.
    out.push('\n');

    // Events, separated by blank lines.
    for (i, event) in events.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        write_event_entry(&mut out, event, &roster, tz);
    }

    out
}

fn write_event_entry(out: &mut String, event: &NotificationEvent, roster: &Roster, tz: Option<Tz>) {
    match event {
        NotificationEvent::Message(msg) => {
            write_message_entry(out, msg, roster, tz);
        }
        NotificationEvent::Reaction {
            message_id,
            user_id,
            emoji,
            ..
        } => {
            let name = roster.get(user_id).map(|s| s.as_str()).unwrap_or("?");
            writeln!(out, "!react|{message_id}|{name}|{emoji}").unwrap();
        }
        NotificationEvent::MessageEdit {
            message_id,
            user_id,
            new_content,
            timestamp,
            ..
        } => {
            let name = roster.get(user_id).map(|s| s.as_str()).unwrap_or("?");
            let ts = format_timestamp(timestamp, tz);
            let content = normalize_content(new_content);
            let line_count = count_lines(content);
            writeln!(out, "!edit|{message_id}|{ts}|{name}|L={line_count}").unwrap();
            if !content.is_empty() {
                writeln!(out, "{content}").unwrap();
            }
        }
        NotificationEvent::MessageDelete { message_id, .. } => {
            writeln!(out, "!delete|{message_id}").unwrap();
        }
        // Non-channel events shouldn't reach here, but handle gracefully.
        NotificationEvent::Trace { message, .. } => {
            writeln!(out, "!trace|{message}").unwrap();
        }
        NotificationEvent::PermissionResponse {
            request_id,
            granted,
        } => {
            let label = if *granted { "allowed" } else { "denied" };
            writeln!(out, "!perm|{request_id}|{label}").unwrap();
        }
        NotificationEvent::ConfigError { error } => {
            writeln!(out, "!config_error|{error}").unwrap();
        }
    }
}

fn write_message_entry(out: &mut String, msg: &MessageEvent, roster: &Roster, tz: Option<Tz>) {
    let ts = format_timestamp(&msg.timestamp, tz);
    let name = roster
        .get(&msg.user_id)
        .map(|s| s.as_str())
        .unwrap_or(msg.user.as_str());
    let content = msg.normalized_content();
    let line_count = msg.content_line_count();

    // Header: MSG_ID|TS|NAME|L=LINES[|>REPLY_TO][|+ATTACHMENTS]
    write!(out, "{}|{}|{}|L={}", msg.message_id, ts, name, line_count).unwrap();

    if let Some(reply_to) = msg.reply_to_message_id {
        write!(out, "|>{reply_to}").unwrap();
    }

    let attachment_count = msg.attachments.len();
    if attachment_count > 0 {
        write!(out, "|+{attachment_count}").unwrap();
    }

    writeln!(out).unwrap();

    if !content.is_empty() {
        writeln!(out, "{content}").unwrap();
    }
}

// ── Non-channel events ──────────────────────────────────────────────────────

/// Coalesce non-channel events (Trace, ConfigError, PermissionResponse).
/// These are rare in practice; wrap them as individual notifications in a batch.
fn coalesce_non_channel_events(events: Vec<NotificationEvent>) -> Value {
    use crate::mcp::notifications::IntoNotification;

    let notifications: Vec<Value> = events.into_iter().map(|e| e.into_notification()).collect();

    if notifications.len() == 1 {
        return notifications.into_iter().next().unwrap();
    }

    // Pack non-channel events into a single standard notification.
    // Extract `content` from each notification. Events without a `content`
    // field (e.g. PermissionResponse, which uses `request_id`/`behavior`)
    // get a compact JSON fallback so their data isn't silently dropped.
    let mut combined_content = String::new();
    for (i, n) in notifications.iter().enumerate() {
        if i > 0 {
            combined_content.push('\n');
        }
        if let Some(content) = n["params"]["content"].as_str() {
            combined_content.push_str(content);
        } else {
            // Fallback: serialize the params object as compact JSON so
            // no event data is silently lost during coalescing.
            let params = &n["params"];
            combined_content.push_str(&serde_json::to_string(params).unwrap_or_default());
        }
    }

    json!({
        "jsonrpc": "2.0",
        "method": "notifications/claude/channel",
        "params": {
            "content": combined_content,
            "meta": {
                "type": "batch",
                "format": "multi",
            },
        }
    })
}

// ── Timestamp formatting ────────────────────────────────────────────────────

/// Format a [`Timestamp`] for events_v1 output.
///
/// Delegates to [`crate::timestamp::format_compact`] for the shared compact
/// `HH:MM` / `HH:MM:SS` logic.
fn format_timestamp(ts: &Timestamp, tz: Option<Tz>) -> String {
    format_compact(ts, tz)
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::events::MessageEvent;
    use serenity::model::id::{ChannelId, MessageId, UserId};

    fn msg_event(channel: u64, msg_id: u64, user: &str, content: &str) -> NotificationEvent {
        NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(channel),
            message_id: MessageId::new(msg_id),
            user: user.to_string(),
            user_id: UserId::new(100),
            content: content.to_string(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        })
    }

    fn reaction_event(channel: u64, msg_id: u64) -> NotificationEvent {
        NotificationEvent::Reaction {
            chat_id: ChannelId::new(channel),
            message_id: MessageId::new(msg_id),
            user: "bob".to_string(),
            user_id: UserId::new(200),
            emoji: "\u{1f44d}".to_string(),
        }
    }

    fn edit_event(channel: u64, msg_id: u64) -> NotificationEvent {
        NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(channel),
            message_id: MessageId::new(msg_id),
            user: "alice".to_string(),
            user_id: UserId::new(100),
            new_content: "edited content".to_string(),
            timestamp: Timestamp::parse("2026-06-19T15:31:00+00:00").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        }
    }

    fn delete_event(channel: u64, msg_id: u64) -> NotificationEvent {
        NotificationEvent::MessageDelete {
            chat_id: ChannelId::new(channel),
            message_id: MessageId::new(msg_id),
            thread_parent_id: None,
        }
    }

    #[test]
    fn empty_returns_none() {
        assert!(coalesce(vec![], None).is_none());
    }

    #[test]
    fn single_event_returns_single() {
        let events = vec![msg_event(1, 100, "alice", "hello")];
        let result = coalesce(events, None);
        assert!(matches!(result, Some(CoalesceResult::Single(_))));
    }

    #[test]
    fn two_messages_same_channel_coalesced() {
        let events = vec![
            msg_event(1, 100, "alice", "hello"),
            msg_event(1, 101, "bob", "world"),
        ];
        let result = coalesce(events, None);
        match result {
            Some(CoalesceResult::Coalesced(v)) => {
                assert_eq!(v["method"], "notifications/claude/channel");
                assert_eq!(v["params"]["meta"]["chat_id"], "1");
                assert_eq!(v["params"]["meta"]["format"], "batch_v1");
                assert_eq!(v["params"]["meta"]["type"], "batch");
                // Content should contain the batch format.
                let content = v["params"]["content"].as_str().unwrap();
                assert!(content.contains("[batch ch=1"));
                assert!(content.contains("alice"));
                assert!(content.contains("hello"));
                assert!(content.contains("world"));
            }
            other => panic!("expected Coalesced, got {other:?}"),
        }
    }

    #[test]
    fn mixed_events_use_heterogeneous_format() {
        let events = vec![msg_event(1, 100, "alice", "hello"), reaction_event(1, 100)];
        let result = coalesce(events, None);
        match result {
            Some(CoalesceResult::Coalesced(v)) => {
                assert_eq!(v["method"], "notifications/claude/channel");
                assert_eq!(v["params"]["meta"]["format"], "events_v1");
                assert_eq!(v["params"]["meta"]["type"], "batch");
                let content = v["params"]["content"].as_str().unwrap();
                assert!(content.contains("[events ch=1"));
                assert!(content.contains("hello"));
                assert!(content.contains("!react|100"));
            }
            other => panic!("expected Coalesced, got {other:?}"),
        }
    }

    #[test]
    fn heterogeneous_with_edit_and_delete() {
        let events = vec![
            msg_event(1, 100, "alice", "original"),
            edit_event(1, 100),
            delete_event(1, 101),
        ];
        let result = coalesce(events, None);
        match result {
            Some(CoalesceResult::Coalesced(v)) => {
                let content = v["params"]["content"].as_str().unwrap();
                assert!(content.contains("[events ch=1"));
                assert!(content.contains("n=3"));
                assert!(content.contains("!edit|100"));
                assert!(content.contains("edited content"));
                assert!(content.contains("!delete|101"));
            }
            other => panic!("expected Coalesced, got {other:?}"),
        }
    }

    #[test]
    fn different_channels_produce_multi_channel_batch() {
        let events = vec![
            msg_event(1, 100, "alice", "in channel 1"),
            msg_event(2, 200, "bob", "in channel 2"),
        ];
        let result = coalesce(events, None);
        match result {
            Some(CoalesceResult::Coalesced(v)) => {
                // Multiple channels packed into a single standard notification.
                assert_eq!(v["method"], "notifications/claude/channel");
                assert_eq!(v["params"]["meta"]["type"], "batch");
                assert_eq!(v["params"]["meta"]["format"], "multi_channel");
                // Content should contain both channels' batch text.
                let content = v["params"]["content"].as_str().unwrap();
                assert!(content.contains("ch=1"));
                assert!(content.contains("ch=2"));
                assert!(content.contains("in channel 1"));
                assert!(content.contains("in channel 2"));
            }
            other => panic!("expected Coalesced, got {other:?}"),
        }
    }

    #[test]
    fn roster_includes_all_participants() {
        let events = vec![
            msg_event(1, 100, "alice", "hello"),
            reaction_event(1, 100),
            edit_event(1, 100),
        ];
        let result = coalesce(events, None);
        match result {
            Some(CoalesceResult::Coalesced(v)) => {
                let content = v["params"]["content"].as_str().unwrap();
                // alice (user_id=100) and bob (user_id=200) should be in roster.
                assert!(content.contains("alice"));
                assert!(content.contains("bob"));
            }
            other => panic!("expected Coalesced, got {other:?}"),
        }
    }

    #[test]
    fn timestamp_formatting() {
        let ts1 = Timestamp::parse("2026-06-19T15:30:00+00:00").unwrap();
        assert_eq!(format_timestamp(&ts1, None), "15:30");
        let ts2 = Timestamp::parse("2026-06-19T15:30:45+00:00").unwrap();
        assert_eq!(format_timestamp(&ts2, None), "15:30:45");
    }

    #[test]
    fn non_channel_events_coalesced_individually() {
        let events = vec![
            NotificationEvent::Trace {
                level: "info".to_string(),
                target: "test".to_string(),
                message: "trace1".to_string(),
                fields: vec![],
            },
            NotificationEvent::Trace {
                level: "warn".to_string(),
                target: "test".to_string(),
                message: "trace2".to_string(),
                fields: vec![],
            },
        ];
        let result = coalesce(events, None);
        match result {
            Some(CoalesceResult::Coalesced(v)) => {
                // Non-channel events use standard method with multi format.
                assert_eq!(v["method"], "notifications/claude/channel");
                assert_eq!(v["params"]["meta"]["format"], "multi");
                assert_eq!(v["params"]["meta"]["type"], "batch");
                // Content should contain both trace messages.
                let content = v["params"]["content"].as_str().unwrap();
                assert!(content.contains("trace1"));
                assert!(content.contains("trace2"));
            }
            other => panic!("expected Coalesced, got {other:?}"),
        }
    }

    // ── Wire-contract snapshots ─────────────────────────────────────────

    #[test]
    fn wire_format_snapshot_events_v1() {
        // Pin the events_v1 heterogeneous envelope format.
        // A message, a reaction, an edit, and a delete — the four event types
        // that appear in a mixed-event batch for a single channel.
        let tz: chrono_tz::Tz = "US/Eastern".parse().expect("valid tz");

        let events = vec![
            NotificationEvent::Message(MessageEvent {
                chat_id: ChannelId::new(555),
                message_id: MessageId::new(3001),
                user: "lina".to_string(),
                user_id: UserId::new(10),
                content: "hey everyone".to_string(),
                targeting: crate::discord::events::MessageTargeting::Ambient,
                timestamp: Timestamp::parse("2026-06-19T20:00:00+00:00").unwrap(),
                attachments: vec![],
                is_voice_message: false,
                thread_parent_id: Some(ChannelId::new(666)),
                reply_to_message_id: None,
                reply_to_user_id: None,
                reply_to_user: None,
                reply_to_content_preview: None,
                bells: None,
                bells_status: None,
            }),
            NotificationEvent::Reaction {
                chat_id: ChannelId::new(555),
                message_id: MessageId::new(3001),
                user: "ros".to_string(),
                user_id: UserId::new(20),
                emoji: "\u{2764}\u{fe0f}".to_string(),
            },
            NotificationEvent::MessageEdit {
                chat_id: ChannelId::new(555),
                message_id: MessageId::new(3001),
                user: "lina".to_string(),
                user_id: UserId::new(10),
                new_content: "hey everyone!".to_string(),
                timestamp: Timestamp::parse("2026-06-19T20:01:00+00:00").unwrap(),
                thread_parent_id: Some(ChannelId::new(666)),
                reply_to_message_id: None,
            },
            NotificationEvent::MessageDelete {
                chat_id: ChannelId::new(555),
                message_id: MessageId::new(2999),
                thread_parent_id: Some(ChannelId::new(666)),
            },
        ];

        let result = coalesce(events, Some(tz));
        let v = match result {
            Some(CoalesceResult::Coalesced(v)) => v,
            other => panic!("expected Coalesced, got {other:?}"),
        };

        assert_eq!(v["method"], "notifications/claude/channel");
        assert_eq!(v["params"]["meta"]["chat_id"], "555");
        assert_eq!(v["params"]["meta"]["format"], "events_v1");
        assert_eq!(v["params"]["meta"]["type"], "batch");

        let content = v["params"]["content"].as_str().unwrap();
        let expected = "\
[events ch=555 thread=666 n=4 tz=US/Eastern]
[users 10=lina 20=ros]

3001|16:00|lina|L=1
hey everyone

!react|3001|ros|\u{2764}\u{fe0f}

!edit|3001|16:01|lina|L=1
hey everyone!

!delete|2999
";
        assert_eq!(content, expected);
    }

    #[test]
    fn wire_format_snapshot_multi() {
        // Pin the multi-envelope format: two trace events wrapped as
        // individual notifications inside a "multi" batch.
        let events = vec![
            NotificationEvent::Trace {
                level: "info".to_string(),
                target: "dione::gate".to_string(),
                message: "channel 555 passed gate".to_string(),
                fields: vec![],
            },
            NotificationEvent::PermissionResponse {
                request_id: "req-42".to_string(),
                granted: true,
            },
        ];

        let result = coalesce(events, None);
        let v = match result {
            Some(CoalesceResult::Coalesced(v)) => v,
            other => panic!("expected Coalesced, got {other:?}"),
        };

        assert_eq!(v["method"], "notifications/claude/channel");
        assert_eq!(v["params"]["meta"]["format"], "multi");
        assert_eq!(v["params"]["meta"]["type"], "batch");

        // Content should contain both non-channel event texts.
        let content = v["params"]["content"].as_str().unwrap();
        assert!(content.contains("channel 555 passed gate"));
        // PermissionResponse has no `content` field in its notification format
        // (it uses `request_id`/`behavior`), so the coalescer falls back to
        // compact JSON serialization of the params to avoid data loss.
        assert!(
            content.contains("req-42"),
            "PermissionResponse request_id must survive coalescing"
        );
        assert!(
            content.contains("allow"),
            "PermissionResponse behavior must survive coalescing"
        );
    }
}
