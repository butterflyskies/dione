//! Per-channel coalescing buffer for notification events.
//!
//! When a channel has `delivery_delay_ms > 0`, incoming channel events
//! (Message, MessageEdit, MessageDelete, Reaction) are buffered and flushed
//! together after the delay expires. Non-channel events (Trace,
//! PermissionResponse, ConfigError) pass through immediately.

use crate::discord::events::NotificationEvent;
use std::collections::{BTreeMap, VecDeque};
use tokio::time::Instant;

/// Per-channel coalescing buffer.
#[derive(Default)]
pub struct DeliveryBuffer {
    /// Buffered events per channel ID, with the flush deadline.
    /// BTreeMap gives deterministic cross-channel ordering in flush_all/flush_ready.
    channels: BTreeMap<u64, ChannelBuffer>,
}

struct ChannelBuffer {
    events: VecDeque<NotificationEvent>,
    flush_at: Instant,
}

/// Result of offering an event to the buffer.
pub enum BufferResult {
    /// Event should be forwarded immediately (not buffered).
    Immediate(Box<NotificationEvent>),
    /// Event was buffered; will be flushed at the channel's deadline.
    Buffered,
}

impl DeliveryBuffer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer an event to the buffer. Returns `Immediate` for events that
    /// should not be buffered (non-channel events, or channels with delay=0).
    pub fn buffer_event(&mut self, event: NotificationEvent, delay_ms: u64) -> BufferResult {
        // No delay → immediate passthrough regardless of event type.
        if delay_ms == 0 {
            return BufferResult::Immediate(Box::new(event));
        }

        // Non-channel events (Trace, PermissionResponse, ConfigError) always
        // pass through immediately — they have no channel association.
        if !is_channel_event(&event) {
            return BufferResult::Immediate(Box::new(event));
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

    /// Drain all buffered events regardless of deadline (used on shutdown).
    ///
    /// Events are returned in deterministic order: sorted by channel ID,
    /// with intra-channel FIFO preserved.
    pub fn flush_all(&mut self) -> Vec<NotificationEvent> {
        let mut flushed = Vec::new();
        let channels = std::mem::take(&mut self.channels);
        for (_, buf) in channels {
            flushed.extend(buf.events);
        }
        flushed
    }

    /// Drain all events from channels whose deadline has passed.
    pub fn flush_ready(&mut self, now: Instant) -> Vec<NotificationEvent> {
        let mut flushed = Vec::new();

        // Collect channel IDs that are ready to flush to avoid borrow issues.
        let ready_channels: Vec<u64> = self
            .channels
            .iter()
            .filter(|(_, b)| !b.events.is_empty() && now >= b.flush_at)
            .map(|(k, _)| *k)
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

/// Returns true if the event is associated with a channel and should be
/// buffered when the channel has a delivery delay.
fn is_channel_event(event: &NotificationEvent) -> bool {
    matches!(
        event,
        NotificationEvent::Message(_)
            | NotificationEvent::MessageEdit { .. }
            | NotificationEvent::MessageDelete { .. }
            | NotificationEvent::Reaction { .. }
    )
}

/// Extract the channel ID from a notification event.
fn extract_channel_id(event: &NotificationEvent) -> u64 {
    match event {
        NotificationEvent::Message(msg) => msg.chat_id.get(),
        NotificationEvent::MessageEdit { chat_id, .. }
        | NotificationEvent::MessageDelete { chat_id, .. }
        | NotificationEvent::Reaction { chat_id, .. } => chat_id.get(),
        // Unreachable in practice (non-channel events bypass the buffer path),
        // but kept for exhaustiveness.
        // 0 is an impossible Discord snowflake (snowflakes start at epoch
        // 2015-01-01, so the minimum valid value is >> 0), making it safe as a
        // sentinel that will never collide with a real channel.
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::discord::events::MessageEvent;
    use crate::timestamp::Timestamp;
    use serenity::model::id::{ChannelId, MessageId, UserId};

    fn msg_event(chat_id: u64) -> NotificationEvent {
        NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(chat_id),
            message_id: MessageId::new(1),
            user: "alice".to_string(),
            user_id: UserId::new(100),
            content: "hello".to_string(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
        })
    }

    fn reaction_event(chat_id: u64) -> NotificationEvent {
        NotificationEvent::Reaction {
            chat_id: ChannelId::new(chat_id),
            message_id: MessageId::new(1),
            user: "bob".to_string(),
            user_id: UserId::new(200),
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
        let event = msg_event(1);
        let result = buf.buffer_event(event, 0);
        assert!(matches!(result, BufferResult::Immediate(_)));
        assert!(buf.next_flush_deadline().is_none());
    }

    #[test]
    fn non_channel_events_always_immediate() {
        let mut buf = DeliveryBuffer::new();

        // Trace event with a delay — should still pass through (not a channel event).
        let result = buf.buffer_event(trace_event(), 1000);
        assert!(matches!(result, BufferResult::Immediate(_)));
    }

    #[test]
    fn reaction_buffered_when_delay_positive() {
        let mut buf = DeliveryBuffer::new();

        // Reaction with a delay — should be buffered (it's a channel event).
        let result = buf.buffer_event(reaction_event(1), 1000);
        assert!(matches!(result, BufferResult::Buffered));
        assert!(buf.next_flush_deadline().is_some());
    }

    #[test]
    fn reaction_immediate_when_no_delay() {
        let mut buf = DeliveryBuffer::new();

        // Reaction with no delay — passes through immediately.
        let result = buf.buffer_event(reaction_event(1), 0);
        assert!(matches!(result, BufferResult::Immediate(_)));
    }

    #[test]
    fn message_events_buffered_when_delay_positive() {
        let mut buf = DeliveryBuffer::new();
        let result = buf.buffer_event(msg_event(1), 500);
        assert!(matches!(result, BufferResult::Buffered));
        assert!(buf.next_flush_deadline().is_some());
    }

    #[test]
    fn flush_ready_drains_expired_channels() {
        let mut buf = DeliveryBuffer::new();

        // Buffer two messages for the same channel.
        buf.buffer_event(msg_event(1), 100);
        buf.buffer_event(msg_event(1), 100);

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
        buf.buffer_event(msg_event(1), 50);
        // Channel 2 has a longer delay.
        let ch2_event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(2),
            message_id: MessageId::new(2),
            user: "carol".to_string(),
            user_id: UserId::new(300),
            content: "world".to_string(),
            targeting: crate::discord::events::MessageTargeting::Ambient,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
        });
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
        buf.buffer_event(msg_event(1), 50);
        let later = Instant::now() + tokio::time::Duration::from_millis(100);
        let flushed = buf.flush_ready(later);
        assert_eq!(flushed.len(), 1);

        // Buffer again — new deadline should be set.
        buf.buffer_event(msg_event(1), 50);
        assert!(buf.next_flush_deadline().is_some());
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::discord::events::MessageEvent;
    use crate::timestamp::Timestamp;
    use proptest::prelude::*;
    use serenity::model::id::{ChannelId, MessageId, UserId};

    /// Strategy: pick a channel ID from a small set.
    fn channel_id_strategy() -> impl Strategy<Value = u64> {
        prop_oneof![Just(1u64), Just(2u64), Just(3u64)]
    }

    /// Strategy: pick a delay value.
    fn delay_strategy() -> impl Strategy<Value = u64> {
        prop_oneof![Just(0u64), Just(50), Just(100), Just(500)]
    }

    /// Strategy: optional reply-to message ID.
    fn reply_to_strategy() -> impl Strategy<Value = Option<MessageId>> {
        prop_oneof![Just(None), Just(Some(MessageId::new(42)))]
    }

    /// Operation in a random buffer/flush sequence.
    #[derive(Debug, Clone)]
    enum Op {
        /// Buffer a channel event with the given delay.
        Buffer { event_idx: usize, delay_ms: u64 },
        /// Flush ready events (advance time past all current deadlines).
        FlushReady,
    }

    /// Strategy: generate a sequence of operations.
    fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
        let op = prop_oneof![
            (0..20usize, delay_strategy()).prop_map(|(event_idx, delay_ms)| Op::Buffer {
                event_idx,
                delay_ms,
            }),
            Just(Op::FlushReady),
        ];
        proptest::collection::vec(op, 1..50)
    }

    proptest! {
        /// No silent drops: every buffered event eventually comes out of
        /// flush_ready or flush_all.
        #[test]
        fn no_silent_drops(ops in ops_strategy()) {
            let mut buf = DeliveryBuffer::new();
            let far_future = Instant::now() + tokio::time::Duration::from_secs(3600);

            // Pre-generate events (indexed by event_idx % pool_size).
            let pool_size = 20;
            let events: Vec<NotificationEvent> = (0..pool_size)
                .map(|i| NotificationEvent::Message(MessageEvent {
                    chat_id: ChannelId::new((i % 3 + 1) as u64),
                    message_id: MessageId::new(1),
                    user: "user".to_string(),
                    user_id: UserId::new(100),
                    content: format!("evt-{i}"),
                    targeting: crate::discord::events::MessageTargeting::Ambient,
                    timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
                    attachments: vec![],
                    is_voice_message: false,
                    thread_parent_id: None,
                    reply_to_message_id: if i % 2 == 0 {
                        None
                    } else {
                        Some(MessageId::new(42))
                    },
                    reply_to_user_id: None,
                    reply_to_user: None,
                    reply_to_content_preview: None,
            bells: None,
                }))
                .collect();

            let mut total_buffered = 0usize;
            let mut total_immediate = 0usize;
            let mut total_flushed = 0usize;

            for op in &ops {
                match op {
                    Op::Buffer { event_idx, delay_ms } => {
                        let event = events[event_idx % pool_size].clone();
                        match buf.buffer_event(event, *delay_ms) {
                            BufferResult::Immediate(_) => total_immediate += 1,
                            BufferResult::Buffered => total_buffered += 1,
                        }
                    }
                    Op::FlushReady => {
                        let flushed = buf.flush_ready(far_future);
                        total_flushed += flushed.len();
                    }
                }
            }

            // Drain remaining.
            let remaining = buf.flush_all();
            total_flushed += remaining.len();

            // Every event that went in must come out.
            let total_in = total_buffered + total_immediate;
            let total_out = total_flushed + total_immediate;
            prop_assert_eq!(
                total_in, total_out,
                "total_in ({}) != total_out ({}): \
                 buffered={}, flushed={}, immediate={}",
                total_in, total_out, total_buffered, total_flushed, total_immediate
            );
        }

        /// Intra-channel FIFO: events buffered for the same channel come out
        /// in the order they went in.
        #[test]
        fn intra_channel_fifo(
            channel in channel_id_strategy(),
            count in 2..20usize,
            delay_ms in delay_strategy().prop_filter("need positive delay", |d| *d > 0),
            reply_to in reply_to_strategy(),
        ) {
            let mut buf = DeliveryBuffer::new();

            // Buffer `count` events for the same channel.
            for i in 0..count {
                let event = NotificationEvent::Message(MessageEvent {
                    chat_id: ChannelId::new(channel),
                    message_id: MessageId::new(1),
                    user: "user".to_string(),
                    user_id: UserId::new(100),
                    content: format!("order-{i}"),
                    targeting: crate::discord::events::MessageTargeting::Ambient,
                    timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
                    attachments: vec![],
                    is_voice_message: false,
                    thread_parent_id: None,
                    reply_to_message_id: reply_to,
                    reply_to_user_id: None,
                    reply_to_user: None,
                    reply_to_content_preview: None,
            bells: None,
                });
                let result = buf.buffer_event(event, delay_ms);
                prop_assert!(matches!(result, BufferResult::Buffered));
            }

            // Flush everything.
            let far_future = Instant::now() + tokio::time::Duration::from_secs(3600);
            let flushed = buf.flush_ready(far_future);

            prop_assert_eq!(flushed.len(), count);

            // Verify order.
            for (i, event) in flushed.iter().enumerate() {
                let content = match event {
                    NotificationEvent::Message(msg) => msg.content.as_str(),
                    _ => panic!("unexpected event type"),
                };
                prop_assert_eq!(content, format!("order-{i}"));
            }
        }

        /// flush_all drains everything: after any sequence of operations,
        /// flush_all returns all remaining events and leaves the buffer empty.
        #[test]
        fn flush_all_drains_everything(ops in ops_strategy()) {
            let mut buf = DeliveryBuffer::new();
            let far_future = Instant::now() + tokio::time::Duration::from_secs(3600);

            let pool_size = 20;
            let events: Vec<NotificationEvent> = (0..pool_size)
                .map(|i| NotificationEvent::Message(MessageEvent {
                    chat_id: ChannelId::new((i % 3 + 1) as u64),
                    message_id: MessageId::new(1),
                    user: "user".to_string(),
                    user_id: UserId::new(100),
                    content: format!("evt-{i}"),
                    targeting: crate::discord::events::MessageTargeting::Ambient,
                    timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
                    attachments: vec![],
                    is_voice_message: false,
                    thread_parent_id: None,
                    reply_to_message_id: if i % 2 == 0 {
                        None
                    } else {
                        Some(MessageId::new(42))
                    },
                    reply_to_user_id: None,
                    reply_to_user: None,
                    reply_to_content_preview: None,
            bells: None,
                }))
                .collect();

            let mut total_buffered = 0usize;
            let mut total_flushed_ready = 0usize;

            for op in &ops {
                match op {
                    Op::Buffer { event_idx, delay_ms } => {
                        let event = events[event_idx % pool_size].clone();
                        match buf.buffer_event(event, *delay_ms) {
                            BufferResult::Immediate(_) => {} // not tracked
                            BufferResult::Buffered => total_buffered += 1,
                        }
                    }
                    Op::FlushReady => {
                        let flushed = buf.flush_ready(far_future);
                        total_flushed_ready += flushed.len();
                    }
                }
            }

            // flush_all must return exactly the remaining buffered events.
            let remaining = buf.flush_all();
            let total_out = total_flushed_ready + remaining.len();
            prop_assert_eq!(total_buffered, total_out);

            // Buffer must be completely empty after flush_all.
            prop_assert!(buf.next_flush_deadline().is_none(), "buffer must be empty after flush_all");
            prop_assert!(buf.flush_all().is_empty(), "second flush_all must return nothing");
        }
    }
}
