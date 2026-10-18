//! Provider-independent scheduling with per-conversation FIFO and burst limits.

use crate::core::ConversationRef;
use std::{
    collections::{BTreeMap, VecDeque, btree_map::Entry},
    time::Duration,
};
use thiserror::Error;
use tokio::time::Instant;

pub(crate) const IMMEDIATE_BURST_LIMIT: u8 = 4;
const IMMEDIATE_BURST_WINDOW: tokio::time::Duration = tokio::time::Duration::from_secs(1);

/// An adapter-owned payload with complete scheduling identity.
pub trait BufferedEvent {
    /// Returns the complete conversation key, or none for a bypassing control.
    fn conversation(&self) -> Option<&ConversationRef>;
    /// Whether the item is eligible for bounded immediate delivery.
    fn immediate_eligible(&self) -> bool;
}

/// Per-conversation scheduling buffer.
pub struct DeliveryBuffer<E> {
    conversations: BTreeMap<ConversationRef, ConversationBuffer<E>>,
    immediate_budget: BTreeMap<ConversationRef, ImmediateBudget>,
}

struct ConversationBuffer<E> {
    events: VecDeque<E>,
    flush_at: Instant,
}

struct ImmediateBudget {
    window_started: Instant,
    used: u8,
}

/// Result of offering an event to the buffer.
#[must_use]
pub enum BufferResult<E> {
    /// Event should be forwarded immediately (not buffered).
    Immediate(Box<E>),
    /// Older same-conversation events must be delivered before this immediate event.
    FlushThenImmediate { preceding: Vec<E>, event: Box<E> },
    /// Event was buffered; will be flushed at the conversation's deadline.
    Buffered,
    /// The required deadline was unrepresentable; the offered event is returned.
    Rejected {
        /// The event that remains owned by the caller.
        event: Box<E>,
        /// The delay that could not produce a deadline.
        error: DeadlineError,
    },
}

/// A delay that cannot produce a scheduling deadline.
#[derive(Debug, Error)]
#[error("invalid scheduling delay {delay:?}")]
pub struct DeadlineError {
    /// The offered delay.
    pub delay: Duration,
    /// The deadline construction failure.
    #[source]
    pub kind: DeadlineErrorKind,
}

/// Deadline construction failures.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum DeadlineErrorKind {
    /// The deadline is outside the clock's representable range.
    #[error("delay exceeds the clock range")]
    OutOfRange,
}

impl<E> Default for DeliveryBuffer<E> {
    fn default() -> Self {
        Self {
            conversations: BTreeMap::new(),
            immediate_budget: BTreeMap::new(),
        }
    }
}

impl<E: BufferedEvent> DeliveryBuffer<E> {
    /// Creates an empty scheduler.
    pub fn new() -> Self {
        Self::default()
    }

    /// Offers an adapter-owned item using its complete conversation identity.
    /// Returns the event if a required new deadline exceeds the clock's range.
    /// Existing queues retain their deadline, and immediate events bypass delay.
    pub fn buffer_event(&mut self, event: E, delay: Duration) -> BufferResult<E> {
        let Some(conversation) = event.conversation().cloned() else {
            return BufferResult::Immediate(Box::new(event));
        };
        if delay.is_zero()
            || (event.immediate_eligible()
                && self.claim_immediate_budget(conversation.clone(), Instant::now()))
        {
            return self.immediate_in_conversation_order(event, &conversation);
        }
        let buf = match self.conversations.entry(conversation) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let Some(flush_at) = Instant::now().checked_add(delay) else {
                    return BufferResult::Rejected {
                        event: Box::new(event),
                        error: DeadlineError {
                            delay,
                            kind: DeadlineErrorKind::OutOfRange,
                        },
                    };
                };
                entry.insert(ConversationBuffer {
                    events: VecDeque::new(),
                    flush_at,
                })
            }
        };
        buf.events.push_back(event);
        BufferResult::Buffered
    }

    fn claim_immediate_budget(&mut self, conversation: ConversationRef, now: Instant) -> bool {
        self.immediate_budget.retain(|_, budget| {
            now.saturating_duration_since(budget.window_started) < IMMEDIATE_BURST_WINDOW
        });
        let budget = self
            .immediate_budget
            .entry(conversation)
            .or_insert(ImmediateBudget {
                window_started: now,
                used: 0,
            });
        if budget.used >= IMMEDIATE_BURST_LIMIT {
            return false;
        }
        budget.used += 1;
        true
    }

    fn immediate_in_conversation_order(
        &mut self,
        event: E,
        conversation: &ConversationRef,
    ) -> BufferResult<E> {
        let preceding = self
            .conversations
            .remove(conversation)
            .map_or_else(Vec::new, |buffer| buffer.events.into_iter().collect());
        if preceding.is_empty() {
            BufferResult::Immediate(Box::new(event))
        } else {
            BufferResult::FlushThenImmediate {
                preceding,
                event: Box::new(event),
            }
        }
    }

    /// Re-insert events that could not be delivered. Events are prepended
    /// to their conversation's buffer in original order so `flush_all` will
    /// include them. New queues are due immediately; existing queues retain
    /// their deadline.
    ///
    /// Events with a conversation bypass the delay and immediate-delivery policy.
    /// Control events without a conversation are discarded.
    pub fn requeue(&mut self, events: Vec<E>) {
        for event in events.into_iter().rev() {
            let Some(conversation) = event.conversation().cloned() else {
                continue;
            };
            let buf =
                self.conversations
                    .entry(conversation)
                    .or_insert_with(|| ConversationBuffer {
                        events: VecDeque::new(),
                        flush_at: Instant::now(),
                    });
            buf.events.push_front(event);
        }
    }

    /// Returns the earliest flush deadline across all non-empty conversation
    /// buffers, or `None` if no events are buffered.
    pub fn next_flush_deadline(&self) -> Option<Instant> {
        self.conversations
            .values()
            .filter(|b| !b.events.is_empty())
            .map(|b| b.flush_at)
            .min()
    }

    /// Drain all buffered events regardless of deadline (used on shutdown).
    ///
    /// Events are sorted by provider, module slot, and local conversation identity,
    /// with FIFO preserved within each conversation. Numeric local identities sort
    /// numerically before opaque identities, which sort lexicographically.
    pub fn flush_all(&mut self) -> Vec<E> {
        let mut flushed = Vec::new();
        let conversations = std::mem::take(&mut self.conversations);
        for (_, buf) in conversations {
            flushed.extend(buf.events);
        }
        flushed
    }

    /// Drains due conversations in the same complete-key order as `flush_all`.
    pub fn flush_ready(&mut self, now: Instant) -> Vec<E> {
        let mut flushed = Vec::new();

        let ready_conversations: Vec<ConversationRef> = self
            .conversations
            .iter()
            .filter(|(_, b)| !b.events.is_empty() && now >= b.flush_at)
            .map(|(k, _)| k.clone())
            .collect();

        for conversation in ready_conversations {
            if let Some(buf) = self.conversations.get_mut(&conversation) {
                flushed.extend(buf.events.drain(..));
            }
        }

        self.conversations.retain(|_, b| !b.events.is_empty());

        flushed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ConversationId, ModuleInstanceId, ModuleSlotId, ProviderId};
    use std::num::NonZeroU64;

    #[derive(Debug)]
    struct FakeEvent {
        conversation: Option<ConversationRef>,
        immediate: bool,
        text: &'static str,
    }

    impl BufferedEvent for FakeEvent {
        fn conversation(&self) -> Option<&ConversationRef> {
            self.conversation.as_ref()
        }
        fn immediate_eligible(&self) -> bool {
            self.immediate
        }
    }

    fn room(instance: &str, local: ConversationId) -> ConversationRef {
        ConversationRef::new(
            ModuleInstanceId::new(
                ProviderId::new("fake").unwrap(),
                ModuleSlotId::new(instance).unwrap(),
            ),
            local,
        )
    }

    fn numeric(value: u64) -> ConversationId {
        ConversationId::numeric(NonZeroU64::new(value).unwrap())
    }

    fn event(room: &ConversationRef, text: &'static str, immediate: bool) -> FakeEvent {
        FakeEvent {
            conversation: Some(room.clone()),
            text,
            immediate,
        }
    }

    fn texts(events: Vec<FakeEvent>) -> Vec<&'static str> {
        events.into_iter().map(|event| event.text).collect()
    }

    #[tokio::test(start_paused = true)]
    async fn unrepresentable_deadline_returns_payload_without_changing_queues() {
        let existing = room("one", numeric(42));
        let rejected = room("two", numeric(42));
        let mut buffer = DeliveryBuffer::new();
        assert!(matches!(
            buffer.buffer_event(event(&existing, "first", false), Duration::from_secs(5)),
            BufferResult::Buffered
        ));
        let deadline = buffer.next_flush_deadline();
        let BufferResult::Rejected {
            event: returned,
            error,
        } = buffer.buffer_event(event(&rejected, "rejected", false), Duration::MAX)
        else {
            panic!("an unrepresentable deadline must return the offered payload");
        };
        assert_eq!(error.kind, DeadlineErrorKind::OutOfRange);
        assert_eq!(error.delay, Duration::MAX);
        assert_eq!(returned.text, "rejected");
        assert_eq!(returned.conversation(), Some(&rejected));
        assert_eq!(buffer.next_flush_deadline(), deadline);
        assert!(buffer.flush_ready(Instant::now()).is_empty());
        assert!(matches!(
            buffer.buffer_event(*returned, Duration::from_secs(2)),
            BufferResult::Buffered
        ));
        tokio::time::advance(Duration::from_secs(2)).await;
        assert_eq!(texts(buffer.flush_ready(Instant::now())), ["rejected"]);
        assert_eq!(buffer.next_flush_deadline(), deadline);
        tokio::time::advance(Duration::from_secs(3)).await;
        assert_eq!(texts(buffer.flush_ready(Instant::now())), ["first"]);
        assert!(buffer.next_flush_deadline().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn queued_events_keep_the_original_deadline_when_later_delay_is_unrepresentable() {
        let conversation = room("one", numeric(42));
        let mut buffer = DeliveryBuffer::new();
        assert!(matches!(
            buffer.buffer_event(event(&conversation, "first", false), Duration::from_secs(5)),
            BufferResult::Buffered
        ));
        let deadline = buffer.next_flush_deadline();
        assert!(matches!(
            buffer.buffer_event(event(&conversation, "second", false), Duration::MAX),
            BufferResult::Buffered
        ));
        assert_eq!(buffer.next_flush_deadline(), deadline);
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            texts(buffer.flush_ready(Instant::now())),
            ["first", "second"]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_events_ignore_delays_that_do_not_need_deadlines() {
        let conversation = room("one", numeric(42));
        let mut buffer = DeliveryBuffer::new();
        let control = FakeEvent {
            conversation: None,
            text: "control",
            immediate: false,
        };
        let BufferResult::Immediate(control) = buffer.buffer_event(control, Duration::MAX) else {
            panic!("controls do not need deadlines");
        };
        assert_eq!(control.text, "control");
        for _ in 0..IMMEDIATE_BURST_LIMIT {
            let BufferResult::Immediate(immediate) =
                buffer.buffer_event(event(&conversation, "immediate", true), Duration::MAX)
            else {
                panic!("admitted immediate events do not need deadlines");
            };
            assert_eq!(immediate.text, "immediate");
        }
        let BufferResult::Rejected {
            event: returned, ..
        } = buffer.buffer_event(event(&conversation, "overflow", true), Duration::MAX)
        else {
            panic!("burst overflow needs a representable deadline");
        };
        assert_eq!(returned.text, "overflow");
        assert!(buffer.next_flush_deadline().is_none());
        assert!(buffer.flush_all().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn same_local_id_in_separate_instances_has_independent_deadlines() {
        let first = room("one", numeric(42));
        let second = room("two", numeric(42));
        let mut buffer = DeliveryBuffer::new();
        assert!(matches!(
            buffer.buffer_event(event(&first, "first", false), Duration::from_secs(1)),
            BufferResult::Buffered
        ));
        assert!(matches!(
            buffer.buffer_event(event(&second, "second", false), Duration::from_secs(2)),
            BufferResult::Buffered
        ));
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(texts(buffer.flush_ready(Instant::now())), ["first"]);
        assert_eq!(texts(buffer.flush_all()), ["second"]);
    }

    #[tokio::test(start_paused = true)]
    async fn immediate_budgets_are_scoped_and_controls_do_not_consume_them() {
        let first = room("one", numeric(42));
        let second = room("two", numeric(42));
        let mut buffer = DeliveryBuffer::new();
        for _ in 0..8 {
            let control = FakeEvent {
                conversation: None,
                text: "control",
                immediate: true,
            };
            let BufferResult::Immediate(item) =
                buffer.buffer_event(control, Duration::from_secs(5))
            else {
                panic!("control must bypass");
            };
            assert_eq!(item.text, "control");
        }
        for _ in 0..4 {
            assert!(matches!(
                buffer.buffer_event(event(&first, "burst", true), Duration::from_secs(5)),
                BufferResult::Immediate(_)
            ));
        }
        assert!(matches!(
            buffer.buffer_event(event(&first, "overflow", true), Duration::from_secs(5)),
            BufferResult::Buffered
        ));
        assert!(matches!(
            buffer.buffer_event(event(&second, "independent", true), Duration::from_secs(5)),
            BufferResult::Immediate(_)
        ));
        assert_eq!(texts(buffer.flush_all()), ["overflow"]);
        assert!(buffer.next_flush_deadline().is_none());
    }

    #[test]
    fn numeric_order_and_opaque_identity_survive_shutdown_drain() {
        let mut buffer = DeliveryBuffer::new();
        for (local, text) in [
            (numeric(10), "ten"),
            (ConversationId::opaque("2").unwrap(), "opaque two"),
            (numeric(2), "two"),
            (numeric(2), "two again"),
        ] {
            assert!(matches!(
                buffer.buffer_event(
                    event(&room("one", local), text, false),
                    Duration::from_secs(5)
                ),
                BufferResult::Buffered
            ));
        }
        assert_eq!(
            texts(buffer.flush_all()),
            ["two", "two again", "ten", "opaque two"]
        );
        assert!(buffer.flush_all().is_empty());
        assert!(buffer.next_flush_deadline().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn requeue_preserves_failed_tail_fifo_and_existing_deadline() {
        let conversation = room("one", numeric(42));
        let mut buffer = DeliveryBuffer::new();
        assert!(matches!(
            buffer.buffer_event(event(&conversation, "later", false), Duration::from_secs(5)),
            BufferResult::Buffered
        ));
        let deadline = buffer.next_flush_deadline();
        buffer.requeue(vec![
            event(&conversation, "failed", false),
            event(&conversation, "unattempted", false),
        ]);
        assert_eq!(buffer.next_flush_deadline(), deadline);
        assert!(buffer.flush_ready(Instant::now()).is_empty());
        assert_eq!(
            texts(buffer.flush_all()),
            ["failed", "unattempted", "later"]
        );
        buffer.requeue(vec![event(&conversation, "retry", false)]);
        assert_eq!(texts(buffer.flush_ready(Instant::now())), ["retry"]);
    }

    #[test]
    fn immediate_event_drains_only_its_own_predecessors() {
        let first = room("one", numeric(42));
        let second = room("two", numeric(42));
        let mut buffer = DeliveryBuffer::new();
        for (conversation, text) in [(&first, "first"), (&first, "second"), (&second, "other")] {
            assert!(matches!(
                buffer.buffer_event(event(conversation, text, false), Duration::from_secs(5)),
                BufferResult::Buffered
            ));
        }
        let BufferResult::FlushThenImmediate { preceding, event } =
            buffer.buffer_event(event(&first, "immediate", true), Duration::from_secs(5))
        else {
            panic!("predecessors must flush first");
        };
        assert_eq!(texts(preceding), ["first", "second"]);
        assert_eq!(event.text, "immediate");
        assert_eq!(texts(buffer.flush_all()), ["other"]);
    }
}
