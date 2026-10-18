//! Discord scheduling classification and scoped payload ownership.

use crate::{
    core::{ConversationId, ConversationRef, ModuleInstanceId},
    delivery_buffer::{BufferResult, BufferedEvent, DeliveryBuffer as Scheduler},
    discord::events::NotificationEvent,
};
use std::time::Duration;
use tokio::time::Instant;

struct ScopedEvent {
    event: NotificationEvent,
    conversation: Option<ConversationRef>,
    immediate_eligible: bool,
}

impl ScopedEvent {
    fn new(module: &ModuleInstanceId, event: NotificationEvent, evidence_enabled: bool) -> Self {
        let channel = match &event {
            NotificationEvent::Message(message) => Some(message.chat_id),
            NotificationEvent::MessageEdit { chat_id, .. }
            | NotificationEvent::MessageDelete { chat_id, .. }
            | NotificationEvent::Reaction { chat_id, .. } => Some(*chat_id),
            NotificationEvent::PermissionResponse { .. }
            | NotificationEvent::Trace { .. }
            | NotificationEvent::ConfigError { .. }
            | NotificationEvent::Coordination(_) => None,
        };
        let conversation = channel.map(|channel| {
            ConversationRef::new(module.clone(), ConversationId::numeric(channel.into()))
        });
        let immediate_eligible = evidence_enabled && event.has_offered_evidence();
        Self {
            event,
            conversation,
            immediate_eligible,
        }
    }
}

impl BufferedEvent for ScopedEvent {
    fn conversation(&self) -> Option<&ConversationRef> {
        self.conversation.as_ref()
    }
    fn immediate_eligible(&self) -> bool {
        self.immediate_eligible
    }
}

/// Schedules Discord notifications within an explicitly configured module slot.
pub struct DeliveryBuffer {
    module: ModuleInstanceId,
    scheduler: Scheduler<ScopedEvent>,
}

impl DeliveryBuffer {
    pub(crate) fn module(&self) -> &ModuleInstanceId {
        &self.module
    }

    /// Creates the adapter for the module slot supplied by composition.
    pub fn new(module: ModuleInstanceId) -> Self {
        Self {
            module,
            scheduler: Scheduler::new(),
        }
    }

    /// Offers an event with evidence markers enabled.
    pub fn buffer_event(
        &mut self,
        event: NotificationEvent,
        delay: Duration,
    ) -> BufferResult<NotificationEvent> {
        self.buffer_event_with_evidence(event, delay, true)
    }

    /// Classifies evidence according to the current Discord delivery setting.
    pub fn buffer_event_with_evidence(
        &mut self,
        event: NotificationEvent,
        delay: Duration,
        evidence_enabled: bool,
    ) -> BufferResult<NotificationEvent> {
        match self.scheduler.buffer_event(
            ScopedEvent::new(&self.module, event, evidence_enabled),
            delay,
        ) {
            BufferResult::Immediate(event) => BufferResult::Immediate(Box::new(event.event)),
            BufferResult::FlushThenImmediate { preceding, event } => {
                BufferResult::FlushThenImmediate {
                    preceding: preceding.into_iter().map(|item| item.event).collect(),
                    event: Box::new(event.event),
                }
            }
            BufferResult::Buffered => BufferResult::Buffered,
            BufferResult::Rejected { event, error } => BufferResult::Rejected {
                event: Box::new(event.event),
                error,
            },
        }
    }

    /// Restores conversation-associated events from the failed and unattempted tail.
    /// Control events without a conversation are discarded.
    pub fn requeue(&mut self, events: Vec<NotificationEvent>) {
        self.scheduler.requeue(
            events
                .into_iter()
                .map(|event| ScopedEvent::new(&self.module, event, false))
                .collect(),
        );
    }

    /// Returns the earliest retained deadline.
    pub fn next_flush_deadline(&self) -> Option<Instant> {
        self.scheduler.next_flush_deadline()
    }

    /// Drains all retained notifications in numeric channel order and channel FIFO.
    pub fn flush_all(&mut self) -> Vec<NotificationEvent> {
        self.scheduler
            .flush_all()
            .into_iter()
            .map(|item| item.event)
            .collect()
    }

    /// Drains conversations whose deadline has passed.
    pub fn flush_ready(&mut self, now: Instant) -> Vec<NotificationEvent> {
        self.scheduler
            .flush_ready(now)
            .into_iter()
            .map(|item| item.event)
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        core::{ModuleSlotId, ProviderId},
        delivery_buffer::IMMEDIATE_BURST_LIMIT,
        discord::events::MessageEvent,
        timestamp::Timestamp,
    };
    use serenity::model::id::{ChannelId, MessageId, UserId};

    fn msg_event(chat_id: ChannelId) -> NotificationEvent {
        NotificationEvent::Message(MessageEvent {
            chat_id,
            message_id: MessageId::new(1),
            user: "alice".to_string(),
            user_id: UserId::new(100),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
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
            bells_status: None,
        })
    }

    fn reaction_event(chat_id: ChannelId) -> NotificationEvent {
        NotificationEvent::Reaction {
            chat_id,
            message_id: MessageId::new(1),
            user: "bob".to_string(),
            user_id: UserId::new(200),
            emoji: "👍".to_string(),
            self_react: false,
        }
    }

    fn evidence_event(chat_id: ChannelId, label: &str) -> NotificationEvent {
        let mut event = msg_event(chat_id);
        let NotificationEvent::Message(message) = &mut event else {
            panic!("message fixture");
        };
        message.content = format!("{label} [🔍=v1:AAAAAAAAAAw]");
        event
    }

    fn trace_event() -> NotificationEvent {
        NotificationEvent::Trace {
            level: tracing::Level::INFO,
            target: "test".to_string(),
            message: "trace msg".to_string(),
            fields: vec![],
        }
    }

    #[test]
    fn discord_items_keep_the_module_scope_attached_to_their_channel() {
        let first = ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("first").unwrap(),
        );
        let second = ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("second").unwrap(),
        );
        let mut scheduler = Scheduler::new();
        assert!(matches!(
            scheduler.buffer_event(
                ScopedEvent::new(&first, msg_event(ChannelId::new(42)), true),
                Duration::from_secs(5)
            ),
            BufferResult::Buffered
        ));
        assert!(matches!(
            scheduler.buffer_event(
                ScopedEvent::new(&second, msg_event(ChannelId::new(42)), true),
                Duration::ZERO
            ),
            BufferResult::Immediate(_)
        ));
        let drained = scheduler.flush_all();
        assert_eq!(drained.len(), 1);
        assert_eq!(
            drained[0].conversation(),
            Some(&ConversationRef::new(
                first,
                ConversationId::numeric(ChannelId::new(42).into())
            ))
        );
    }

    #[test]
    fn rejected_deadline_returns_the_original_discord_event() {
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        let BufferResult::Rejected { event, error } =
            buffer.buffer_event(msg_event(ChannelId::new(42)), Duration::MAX)
        else {
            panic!("an unrepresentable deadline must be rejected");
        };
        assert_eq!(error.delay, Duration::MAX);
        let NotificationEvent::Message(message) = *event else {
            panic!("the original message must be retained");
        };
        assert_eq!(message.chat_id, ChannelId::new(42));
        assert_eq!(message.message_id, MessageId::new(1));
        assert_eq!(message.content, "hello");
        assert!(buffer.next_flush_deadline().is_none());
        assert!(buffer.flush_all().is_empty());
        assert!(matches!(
            buffer.buffer_event(NotificationEvent::Message(message), Duration::from_secs(1)),
            BufferResult::Buffered
        ));
    }

    #[test]
    fn evidence_bearing_message_bypasses_delayed_coalescing() {
        let mut event = msg_event(ChannelId::new(42));
        let NotificationEvent::Message(message) = &mut event else {
            panic!("message fixture");
        };
        message.content = "claim [🔍=v1:AAAAAAAAAAw]".to_string();
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        assert!(matches!(
            buffer.buffer_event(event, Duration::from_millis(500)),
            BufferResult::Immediate(_)
        ));
        assert!(buffer.flush_all().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn evidence_immediate_budget_buffers_overflow_in_fifo_order() {
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        for index in 0..IMMEDIATE_BURST_LIMIT {
            assert!(matches!(
                buffer.buffer_event(
                    evidence_event(ChannelId::new(42), &format!("burst-{index}")),
                    Duration::from_millis(500)
                ),
                BufferResult::Immediate(_)
            ));
        }

        assert!(matches!(
            buffer.buffer_event(
                evidence_event(ChannelId::new(42), "overflow-1"),
                Duration::from_millis(500)
            ),
            BufferResult::Buffered
        ));
        assert!(matches!(
            buffer.buffer_event(
                evidence_event(ChannelId::new(42), "overflow-2"),
                Duration::from_millis(500)
            ),
            BufferResult::Buffered
        ));

        let flushed = buffer.flush_all();
        let contents: Vec<&str> = flushed
            .iter()
            .map(|event| match event {
                NotificationEvent::Message(message) => message.content.as_str(),
                _ => panic!("message fixture"),
            })
            .collect();
        assert_eq!(
            contents,
            [
                "overflow-1 [🔍=v1:AAAAAAAAAAw]",
                "overflow-2 [🔍=v1:AAAAAAAAAAw]",
            ]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn evidence_immediate_budget_recovers_and_flushes_buffered_predecessor() {
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        for index in 0..IMMEDIATE_BURST_LIMIT {
            assert!(matches!(
                buffer.buffer_event(
                    evidence_event(ChannelId::new(42), &format!("burst-{index}")),
                    Duration::from_millis(500)
                ),
                BufferResult::Immediate(_)
            ));
        }
        assert!(matches!(
            buffer.buffer_event(
                evidence_event(ChannelId::new(42), "overflow"),
                Duration::from_millis(500)
            ),
            BufferResult::Buffered
        ));

        tokio::time::advance(Duration::from_secs(1)).await;
        let BufferResult::FlushThenImmediate { preceding, event } = buffer.buffer_event(
            evidence_event(ChannelId::new(42), "recovered"),
            Duration::from_millis(500),
        ) else {
            panic!("recovered budget must preserve FIFO before immediate delivery");
        };
        assert_eq!(preceding.len(), 1);
        let NotificationEvent::Message(preceding) = &preceding[0] else {
            panic!("message fixture");
        };
        assert!(preceding.content.starts_with("overflow "));
        let NotificationEvent::Message(current) = *event else {
            panic!("message fixture");
        };
        assert!(current.content.starts_with("recovered "));
    }

    #[test]
    fn evidence_message_flushes_older_same_channel_events_first() {
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        let mut older = msg_event(ChannelId::new(42));
        let NotificationEvent::Message(message) = &mut older else {
            panic!("message fixture");
        };
        message.content = "older".to_string();
        assert!(matches!(
            buffer.buffer_event(older, Duration::from_millis(500)),
            BufferResult::Buffered
        ));

        let mut evidence = msg_event(ChannelId::new(42));
        let NotificationEvent::Message(message) = &mut evidence else {
            panic!("message fixture");
        };
        message.content = "claim [🔍=v1:AAAAAAAAAAw]".to_string();
        let BufferResult::FlushThenImmediate { preceding, event } =
            buffer.buffer_event(evidence, Duration::from_millis(500))
        else {
            panic!("evidence event must flush its predecessors");
        };

        assert_eq!(preceding.len(), 1);
        let NotificationEvent::Message(older) = &preceding[0] else {
            panic!("older message");
        };
        assert_eq!(older.content, "older");
        assert!(event.has_offered_evidence());
        assert!(buffer.flush_all().is_empty());
    }

    #[test]
    fn disabled_evidence_marker_uses_ordinary_buffering() {
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        let mut event = msg_event(ChannelId::new(42));
        let NotificationEvent::Message(message) = &mut event else {
            panic!("message fixture");
        };
        message.content = "claim [🔍=v1:AAAAAAAAAAw]".to_string();

        assert!(matches!(
            buffer.buffer_event_with_evidence(event, Duration::from_millis(500), false),
            BufferResult::Buffered
        ));
    }

    #[test]
    fn evidence_message_flushes_predecessors_after_delay_changes_to_zero() {
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        assert!(matches!(
            buffer.buffer_event(msg_event(ChannelId::new(42)), Duration::from_millis(500)),
            BufferResult::Buffered
        ));
        let mut evidence = msg_event(ChannelId::new(42));
        let NotificationEvent::Message(message) = &mut evidence else {
            panic!("message fixture");
        };
        message.content = "claim [🔍=v1:AAAAAAAAAAw]".to_string();

        let BufferResult::FlushThenImmediate { preceding, .. } =
            buffer.buffer_event(evidence, Duration::from_millis(0))
        else {
            panic!("configuration change must not reorder the channel");
        };
        assert_eq!(preceding.len(), 1);
    }

    #[test]
    fn ordinary_message_flushes_predecessors_after_delay_changes_to_zero() {
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        let mut older = msg_event(ChannelId::new(42));
        let NotificationEvent::Message(message) = &mut older else {
            panic!("message fixture");
        };
        message.content = "older".to_string();
        assert!(matches!(
            buffer.buffer_event(older, Duration::from_millis(500)),
            BufferResult::Buffered
        ));

        let mut current = msg_event(ChannelId::new(42));
        let NotificationEvent::Message(message) = &mut current else {
            panic!("message fixture");
        };
        message.content = "current".to_string();
        let BufferResult::FlushThenImmediate { preceding, event } =
            buffer.buffer_event(current, Duration::from_millis(0))
        else {
            panic!("configuration change must preserve ordinary message FIFO");
        };

        assert_eq!(preceding.len(), 1);
        let NotificationEvent::Message(older) = &preceding[0] else {
            panic!("older message");
        };
        let NotificationEvent::Message(current) = *event else {
            panic!("current message");
        };
        assert_eq!(older.content, "older");
        assert_eq!(current.content, "current");
        assert!(buffer.flush_all().is_empty());
    }

    #[test]
    fn ordinary_edit_flushes_predecessors_after_delay_changes_to_zero() {
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        assert!(matches!(
            buffer.buffer_event(msg_event(ChannelId::new(42)), Duration::from_millis(500)),
            BufferResult::Buffered
        ));
        let edit = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(42),
            message_id: MessageId::new(1),
            user: "alice".to_string(),
            user_id: UserId::new(100),
            new_content: "edited".to_string(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };

        let BufferResult::FlushThenImmediate { preceding, event } =
            buffer.buffer_event(edit, Duration::from_millis(0))
        else {
            panic!("configuration change must preserve ordinary edit FIFO");
        };

        assert_eq!(preceding.len(), 1);
        assert!(matches!(preceding[0], NotificationEvent::Message(_)));
        assert!(matches!(*event, NotificationEvent::MessageEdit { .. }));
        assert!(buffer.flush_all().is_empty());
    }

    #[test]
    fn malformed_marker_remains_eligible_for_coalescing() {
        let mut event = msg_event(ChannelId::new(42));
        let NotificationEvent::Message(message) = &mut event else {
            panic!("message fixture");
        };
        message.content = "claim [🔍=v1:short]".to_string();
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        assert!(matches!(
            buffer.buffer_event(event, Duration::from_millis(500)),
            BufferResult::Buffered
        ));
    }

    #[test]
    fn edit_coalescing_tracks_only_the_marker_in_current_content() {
        let edit = |new_content: &str| NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(42),
            message_id: MessageId::new(1),
            user: "alice".to_string(),
            user_id: UserId::new(100),
            new_content: new_content.to_string(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        assert!(matches!(
            buffer.buffer_event(
                edit("edited [🔍=v1:AAAAAAAAAAw]"),
                Duration::from_millis(500)
            ),
            BufferResult::Immediate(_)
        ));
        assert!(matches!(
            buffer.buffer_event(edit("edited"), Duration::from_millis(500)),
            BufferResult::Buffered
        ));
    }

    #[test]
    fn evidence_edit_flushes_older_same_channel_events_first() {
        let mut buffer = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        assert!(matches!(
            buffer.buffer_event(msg_event(ChannelId::new(42)), Duration::from_millis(500)),
            BufferResult::Buffered
        ));
        let evidence_edit = NotificationEvent::MessageEdit {
            chat_id: ChannelId::new(42),
            message_id: MessageId::new(1),
            user: "alice".to_string(),
            user_id: UserId::new(100),
            new_content: "edited [🔍=v1:AAAAAAAAAAw]".to_string(),
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            thread_parent_id: None,
            reply_to_message_id: None,
        };

        let BufferResult::FlushThenImmediate { preceding, event } =
            buffer.buffer_event(evidence_edit, Duration::from_millis(500))
        else {
            panic!("evidence edit must flush its predecessors");
        };
        assert_eq!(preceding.len(), 1);
        assert!(matches!(preceding[0], NotificationEvent::Message(_)));
        assert!(matches!(*event, NotificationEvent::MessageEdit { .. }));
    }

    #[test]
    fn immediate_passthrough_when_delay_zero() {
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        let event = msg_event(ChannelId::new(1));
        let result = buf.buffer_event(event, Duration::from_millis(0));
        assert!(matches!(result, BufferResult::Immediate(_)));
        assert!(buf.next_flush_deadline().is_none());
    }

    #[test]
    fn non_channel_events_always_immediate() {
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        // Trace event with a delay — should still pass through (not a channel event).
        let result = buf.buffer_event(trace_event(), Duration::from_millis(1000));
        assert!(matches!(result, BufferResult::Immediate(_)));
    }

    #[test]
    fn reaction_buffered_when_delay_positive() {
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        // Reaction with a delay — should be buffered (it's a channel event).
        let result = buf.buffer_event(
            reaction_event(ChannelId::new(1)),
            Duration::from_millis(1000),
        );
        assert!(matches!(result, BufferResult::Buffered));
        assert!(buf.next_flush_deadline().is_some());
    }

    #[test]
    fn reaction_immediate_when_no_delay() {
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        // Reaction with no delay — passes through immediately.
        let result = buf.buffer_event(reaction_event(ChannelId::new(1)), Duration::from_millis(0));
        assert!(matches!(result, BufferResult::Immediate(_)));
    }

    #[test]
    fn message_events_buffered_when_delay_positive() {
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        let result = buf.buffer_event(msg_event(ChannelId::new(1)), Duration::from_millis(500));
        assert!(matches!(result, BufferResult::Buffered));
        assert!(buf.next_flush_deadline().is_some());
    }

    #[test]
    fn flush_ready_drains_expired_channels() {
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        // Buffer two messages for the same channel.
        assert!(matches!(
            buf.buffer_event(msg_event(ChannelId::new(1)), Duration::from_millis(100)),
            BufferResult::Buffered
        ));
        assert!(matches!(
            buf.buffer_event(msg_event(ChannelId::new(1)), Duration::from_millis(100)),
            BufferResult::Buffered
        ));

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
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        // Channel 1 has a short delay.
        assert!(matches!(
            buf.buffer_event(msg_event(ChannelId::new(1)), Duration::from_millis(50)),
            BufferResult::Buffered
        ));
        // Channel 2 has a longer delay.
        let ch2_event = NotificationEvent::Message(MessageEvent {
            chat_id: ChannelId::new(2),
            message_id: MessageId::new(2),
            user: "carol".to_string(),
            user_id: UserId::new(300),
            author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
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
            bells_status: None,
        });
        assert!(matches!(
            buf.buffer_event(ch2_event, Duration::from_millis(500)),
            BufferResult::Buffered
        ));

        // After 100ms: ch1 should flush, ch2 should not.
        let now = Instant::now() + tokio::time::Duration::from_millis(100);
        let flushed = buf.flush_ready(now);
        assert_eq!(flushed.len(), 1);
        // ch2 still pending.
        assert!(buf.next_flush_deadline().is_some());
    }

    #[test]
    fn empty_buffer_has_no_deadline() {
        let buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));
        assert!(buf.next_flush_deadline().is_none());
    }

    #[test]
    fn flush_resets_for_new_events() {
        let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("primary").unwrap(),
        ));

        // Buffer and flush.
        assert!(matches!(
            buf.buffer_event(msg_event(ChannelId::new(1)), Duration::from_millis(50)),
            BufferResult::Buffered
        ));
        let later = Instant::now() + tokio::time::Duration::from_millis(100);
        let flushed = buf.flush_ready(later);
        assert_eq!(flushed.len(), 1);

        // Buffer again — new deadline should be set.
        assert!(matches!(
            buf.buffer_event(msg_event(ChannelId::new(1)), Duration::from_millis(50)),
            BufferResult::Buffered
        ));
        assert!(buf.next_flush_deadline().is_some());
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use crate::{
        core::{ModuleSlotId, ProviderId},
        discord::events::MessageEvent,
        timestamp::Timestamp,
    };
    use proptest::prelude::*;
    use serenity::model::id::{ChannelId, MessageId, UserId};

    /// Strategy: pick a channel ID from a small set.
    fn channel_id_strategy() -> impl Strategy<Value = ChannelId> {
        prop_oneof![Just(1u64), Just(2u64), Just(3u64)].prop_map(ChannelId::new)
    }

    /// Strategy: pick a delay value.
    fn delay_strategy() -> impl Strategy<Value = Duration> {
        prop_oneof![Just(0u64), Just(50), Just(100), Just(500)].prop_map(Duration::from_millis)
    }

    /// Strategy: optional reply-to message ID.
    fn reply_to_strategy() -> impl Strategy<Value = Option<MessageId>> {
        prop_oneof![Just(None), Just(Some(MessageId::new(42)))]
    }

    /// Operation in a random buffer/flush sequence.
    #[derive(Debug, Clone)]
    enum Op {
        /// Buffer a channel event with the given delay.
        Buffer { event_idx: usize, delay: Duration },
        /// Flush ready events (advance time past all current deadlines).
        FlushReady,
    }

    /// Strategy: generate a sequence of operations.
    fn ops_strategy() -> impl Strategy<Value = Vec<Op>> {
        let op = prop_oneof![
            (0..20usize, delay_strategy())
                .prop_map(|(event_idx, delay)| Op::Buffer { event_idx, delay }),
            Just(Op::FlushReady),
        ];
        proptest::collection::vec(op, 1..50)
    }

    proptest! {
        /// No silent drops: every buffered event eventually comes out of
        /// flush_ready or flush_all.
        #[test]
        fn no_silent_drops(ops in ops_strategy()) {
            let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(ProviderId::new("discord").unwrap(), ModuleSlotId::new("primary").unwrap()));
            let far_future = Instant::now() + tokio::time::Duration::from_secs(3600);

            // Pre-generate events (indexed by event_idx % pool_size).
            let pool_size = 20;
            let events: Vec<NotificationEvent> = (0..pool_size)
                .map(|i| NotificationEvent::Message(MessageEvent {
                    chat_id: ChannelId::new(u64::try_from(i % 3 + 1).unwrap()),
                    message_id: MessageId::new(1),
                    user: "user".to_string(),
                    user_id: UserId::new(100),
                    author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
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
            bells_status: None,
                }))
                .collect();

            let mut total_buffered = 0usize;
            let mut total_immediate = 0usize;
            let mut total_flushed = 0usize;

            for op in &ops {
                match op {
                    Op::Buffer { event_idx, delay } => {
                        let event = events[event_idx % pool_size].clone();
                        match buf.buffer_event(event, *delay) {
                            BufferResult::Immediate(_) => total_immediate += 1,
                            BufferResult::FlushThenImmediate { preceding, .. } => {
                                total_flushed += preceding.len();
                                total_immediate += 1;
                            }
                            BufferResult::Buffered => total_buffered += 1,
                            BufferResult::Rejected { error, .. } => panic!("valid fixture delay rejected: {error}"),
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
            delay in delay_strategy().prop_filter("need positive delay", |d| !d.is_zero()),
            reply_to in reply_to_strategy(),
        ) {
            let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(ProviderId::new("discord").unwrap(), ModuleSlotId::new("primary").unwrap()));

            // Buffer `count` events for the same channel.
            for i in 0..count {
                let event = NotificationEvent::Message(MessageEvent {
                    chat_id: channel,
                    message_id: MessageId::new(1),
                    user: "user".to_string(),
                    user_id: UserId::new(100),
                    author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
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
            bells_status: None,
                });
                let result = buf.buffer_event(event, delay);
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
            let mut buf = DeliveryBuffer::new(ModuleInstanceId::new(ProviderId::new("discord").unwrap(), ModuleSlotId::new("primary").unwrap()));
            let far_future = Instant::now() + tokio::time::Duration::from_secs(3600);

            let pool_size = 20;
            let events: Vec<NotificationEvent> = (0..pool_size)
                .map(|i| NotificationEvent::Message(MessageEvent {
                    chat_id: ChannelId::new(u64::try_from(i % 3 + 1).unwrap()),
                    message_id: MessageId::new(1),
                    user: "user".to_string(),
                    user_id: UserId::new(100),
                    author_kind: crate::attention::types::SourceAuthorKind::DirectHuman,
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
            bells_status: None,
                }))
                .collect();

            let mut total_buffered = 0usize;
            let mut total_flushed_ready = 0usize;

            for op in &ops {
                match op {
                    Op::Buffer { event_idx, delay } => {
                        let event = events[event_idx % pool_size].clone();
                        match buf.buffer_event(event, *delay) {
                            BufferResult::Immediate(_) => {} // not tracked
                            BufferResult::FlushThenImmediate { preceding, .. } => {
                                total_flushed_ready += preceding.len();
                            }
                            BufferResult::Buffered => total_buffered += 1,
                            BufferResult::Rejected { error, .. } => panic!("valid fixture delay rejected: {error}"),
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
