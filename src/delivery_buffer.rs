//! Per-channel coalescing buffer for notification events.
//!
//! When a channel has `delivery_delay_ms > 0`, incoming message events are
//! buffered and flushed together after the delay expires. Non-message events
//! (reactions, traces, permission responses, config errors) pass through
//! immediately.

use std::collections::{HashMap, VecDeque};

use tokio::time::Instant;

use crate::discord::events::NotificationEvent;

/// Per-channel coalescing buffer.
#[derive(Default)]
pub struct DeliveryBuffer {
    /// Buffered events per channel ID, with the flush deadline.
    channels: HashMap<String, ChannelBuffer>,
}

struct ChannelBuffer {
    events: VecDeque<NotificationEvent>,
    flush_at: Instant,
}

/// Result of offering an event to the buffer.
pub enum BufferResult {
    /// Event should be forwarded immediately (not buffered).
    Immediate(NotificationEvent),
    /// Event was buffered; will be flushed at the channel's deadline.
    Buffered,
}

impl DeliveryBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer an event to the buffer. Returns `Immediate` for events that
    /// should not be buffered (non-message events, or channels with delay=0).
    pub fn buffer_event(&mut self, event: NotificationEvent, delay_ms: u64) -> BufferResult {
        // No delay or non-message event → immediate passthrough.
        if delay_ms == 0 || !is_message_event(&event) {
            return BufferResult::Immediate(event);
        }

        let channel_id = extract_channel_id(&event);
        let delay = tokio::time::Duration::from_millis(delay_ms);

        let buf = self
            .channels
            .entry(channel_id)
            .or_insert_with(|| ChannelBuffer {
                events: VecDeque::new(),
                flush_at: Instant::now() + delay,
            });

        // If this is the first event after a flush (buffer was empty),
        // reset the deadline.
        if buf.events.is_empty() {
            buf.flush_at = Instant::now() + delay;
        }

        buf.events.push_back(event);
        BufferResult::Buffered
    }

    /// Returns the earliest flush deadline across all non-empty channel
    /// buffers, or `None` if no events are buffered.
    pub fn next_flush_deadline(&self) -> Option<Instant> {
        self.channels
            .values()
            .filter(|b| !b.events.is_empty())
            .map(|b| b.flush_at)
            .min()
    }

    /// Drain all events from channels whose deadline has passed.
    pub fn flush_ready(&mut self, now: Instant) -> Vec<NotificationEvent> {
        let mut flushed = Vec::new();

        // Collect channel IDs that are ready to flush to avoid borrow issues.
        let ready_channels: Vec<String> = self
            .channels
            .iter()
            .filter(|(_, b)| !b.events.is_empty() && now >= b.flush_at)
            .map(|(k, _)| k.clone())
            .collect();

        for channel_id in ready_channels {
            if let Some(buf) = self.channels.get_mut(&channel_id) {
                flushed.extend(buf.events.drain(..));
            }
        }

        // Remove empty channel entries to avoid unbounded growth.
        self.channels.retain(|_, b| !b.events.is_empty());

        flushed
    }
}

/// Returns true if the event is a message-type event (should be buffered).
fn is_message_event(event: &NotificationEvent) -> bool {
    matches!(
        event,
        NotificationEvent::Message { .. }
            | NotificationEvent::MessageEdit { .. }
            | NotificationEvent::MessageDelete { .. }
    )
}

/// Extract the channel ID string from a notification event.
fn extract_channel_id(event: &NotificationEvent) -> String {
    match event {
        NotificationEvent::Message { chat_id, .. }
        | NotificationEvent::MessageEdit { chat_id, .. }
        | NotificationEvent::MessageDelete { chat_id, .. } => chat_id.clone(),
        NotificationEvent::Reaction { chat_id, .. } => chat_id.clone(),
        // Events without a channel ID get a synthetic key (shouldn't reach
        // the buffer path, but be safe).
        _ => "__no_channel__".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg_event(chat_id: &str) -> NotificationEvent {
        NotificationEvent::Message {
            chat_id: chat_id.to_string(),
            message_id: "1".to_string(),
            user: "alice".to_string(),
            user_id: "100".to_string(),
            content: "hello".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
        }
    }

    fn reaction_event(chat_id: &str) -> NotificationEvent {
        NotificationEvent::Reaction {
            chat_id: chat_id.to_string(),
            message_id: "1".to_string(),
            user: "bob".to_string(),
            user_id: "200".to_string(),
            emoji: "👍".to_string(),
        }
    }

    fn trace_event() -> NotificationEvent {
        NotificationEvent::Trace {
            level: "info".to_string(),
            target: "test".to_string(),
            message: "trace msg".to_string(),
            fields: vec![],
        }
    }

    #[test]
    fn immediate_passthrough_when_delay_zero() {
        let mut buf = DeliveryBuffer::new();
        let event = msg_event("ch1");
        let result = buf.buffer_event(event, 0);
        assert!(matches!(result, BufferResult::Immediate(_)));
        assert!(buf.next_flush_deadline().is_none());
    }

    #[test]
    fn non_message_events_always_immediate() {
        let mut buf = DeliveryBuffer::new();

        // Reaction with a delay — should still pass through.
        let result = buf.buffer_event(reaction_event("ch1"), 1000);
        assert!(matches!(result, BufferResult::Immediate(_)));

        // Trace event.
        let result = buf.buffer_event(trace_event(), 1000);
        assert!(matches!(result, BufferResult::Immediate(_)));
    }

    #[test]
    fn message_events_buffered_when_delay_positive() {
        let mut buf = DeliveryBuffer::new();
        let result = buf.buffer_event(msg_event("ch1"), 500);
        assert!(matches!(result, BufferResult::Buffered));
        assert!(buf.next_flush_deadline().is_some());
    }

    #[test]
    fn flush_ready_drains_expired_channels() {
        let mut buf = DeliveryBuffer::new();

        // Buffer two messages for the same channel.
        buf.buffer_event(msg_event("ch1"), 100);
        buf.buffer_event(msg_event("ch1"), 100);

        // Before deadline: nothing flushed.
        let now = Instant::now();
        let flushed = buf.flush_ready(now);
        assert!(flushed.is_empty(), "should not flush before deadline");

        // After deadline: both messages flushed.
        let later = now + tokio::time::Duration::from_millis(200);
        let flushed = buf.flush_ready(later);
        assert_eq!(flushed.len(), 2, "should flush both buffered messages");
        assert!(buf.next_flush_deadline().is_none());
    }

    #[test]
    fn multiple_channels_flush_independently() {
        let mut buf = DeliveryBuffer::new();

        // Channel 1 has a short delay.
        buf.buffer_event(msg_event("ch1"), 50);
        // Channel 2 has a longer delay.
        let ch2_event = NotificationEvent::Message {
            chat_id: "ch2".to_string(),
            message_id: "2".to_string(),
            user: "carol".to_string(),
            user_id: "300".to_string(),
            content: "world".to_string(),
            timestamp: "2026-01-01T00:00:00Z".to_string(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
        };
        buf.buffer_event(ch2_event, 500);

        // After 100ms: ch1 should flush, ch2 should not.
        let now = Instant::now() + tokio::time::Duration::from_millis(100);
        let flushed = buf.flush_ready(now);
        assert_eq!(flushed.len(), 1);
        // ch2 still pending.
        assert!(buf.next_flush_deadline().is_some());
    }

    #[test]
    fn empty_buffer_has_no_deadline() {
        let buf = DeliveryBuffer::new();
        assert!(buf.next_flush_deadline().is_none());
    }

    #[test]
    fn flush_resets_for_new_events() {
        let mut buf = DeliveryBuffer::new();

        // Buffer and flush.
        buf.buffer_event(msg_event("ch1"), 50);
        let later = Instant::now() + tokio::time::Duration::from_millis(100);
        let flushed = buf.flush_ready(later);
        assert_eq!(flushed.len(), 1);

        // Buffer again — new deadline should be set.
        buf.buffer_event(msg_event("ch1"), 50);
        assert!(buf.next_flush_deadline().is_some());
    }
}
