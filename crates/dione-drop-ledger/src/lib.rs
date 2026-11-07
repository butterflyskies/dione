//! Recently dropped message retention and direct-reply inheritance.
//!
//! Native Dione admission records configured mute, mention, bot and non-identity
//! gate drops under an effective gate scope (thread parent or channel). Native
//! identity-ignore policy remains stateless. These policies live in the native
//! application's guild_message_preflight and admit_direct_guild_message functions,
//! not in this ledger. A ChannelId or record call is not authorization.
//!
//! A direct reply inherits only when its caller supplies the scope containing
//! the recorded parent. Callers record each inherited hop to extend a chain.
//! Oldest ids are evicted within each scope; activity in another scope cannot
//! evict them. There is no TTL, persistence or bound on the number of scopes.
//! Restart forgets retention and native callers apply their ordinary gating.
//!
//! Reads and writes take one short mutex critical section. A poisoned mutex
//! panics, preserving the original behavior. The process-wide singleton belongs
//! here and is shared by native ingress and canonical egress fallback.
#![warn(missing_docs)]

use serenity::model::id::{ChannelId, MessageId};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Mutex, OnceLock},
};

/// Maximum dropped-message ids retained per gate scope. Oldest are evicted
/// first, within their own scope only.
pub const SCOPE_CAPACITY: usize = 1024;

struct ScopeRing {
    order: VecDeque<MessageId>,
    set: HashSet<MessageId>,
}

impl ScopeRing {
    fn new() -> Self {
        Self {
            order: VecDeque::new(),
            set: HashSet::new(),
        }
    }

    fn record(&mut self, message_id: MessageId) {
        if self.set.insert(message_id) {
            self.order.push_back(message_id);
            if self.order.len() > SCOPE_CAPACITY
                && let Some(evicted) = self.order.pop_front()
            {
                self.set.remove(&evicted);
            }
        }
    }
}

/// Scope-partitioned retention of dropped inbound message ids.
///
/// Each scope retains at most [`SCOPE_CAPACITY`] ids. The number of scopes is
/// unbounded here; callers must derive scopes from their admitted configuration.
pub struct DropLedger {
    scopes: Mutex<HashMap<ChannelId, ScopeRing>>,
}

impl DropLedger {
    /// Create an empty ledger. Restarting or replacing it forgets all drops.
    pub fn new() -> Self {
        Self {
            scopes: Mutex::new(HashMap::new()),
        }
    }

    /// Record a dropped message id under its gate scope. Idempotent within
    /// the scope; evicts oldest-first within the scope only.
    pub fn record(&self, scope: ChannelId, message_id: MessageId) {
        self.scopes
            .lock()
            .expect("drop ledger poisoned")
            .entry(scope)
            .or_insert_with(ScopeRing::new)
            .record(message_id);
    }

    /// Was `message_id` dropped recently in `scope`?
    pub fn contains(&self, scope: ChannelId, message_id: MessageId) -> bool {
        self.scopes
            .lock()
            .expect("drop ledger poisoned")
            .get(&scope)
            .is_some_and(|ring| ring.set.contains(&message_id))
    }

    /// The reply-inheritance decision: does a message directly replying to
    /// `parent_id` inherit a drop? `None` (not a reply) never inherits.
    /// Replies are same-channel in Discord, so the reply's own gate scope is
    /// the scope its parent was recorded under.
    pub fn reply_inherits_drop(&self, scope: ChannelId, parent_id: Option<MessageId>) -> bool {
        parent_id.is_some_and(|id| self.contains(scope, id))
    }

    #[cfg(test)]
    fn scope_len(&self, scope: ChannelId) -> usize {
        self.scopes
            .lock()
            .expect("drop ledger poisoned")
            .get(&scope)
            .map_or(0, |ring| ring.order.len())
    }
}

impl Default for DropLedger {
    fn default() -> Self {
        Self::new()
    }
}

static DROP_LEDGER: OnceLock<DropLedger> = OnceLock::new();

/// The process-wide ledger. Self-initializing: no state directory, no
/// startup wiring — first touch creates it empty.
pub fn global() -> &'static DropLedger {
    DROP_LEDGER.get_or_init(DropLedger::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope(raw: u64) -> ChannelId {
        ChannelId::new(raw)
    }

    fn id(raw: u64) -> MessageId {
        MessageId::new(raw)
    }

    const SCOPE: u64 = 20;

    #[test]
    fn records_and_answers_membership() {
        let ledger = DropLedger::new();
        assert!(!ledger.contains(scope(SCOPE), id(42)));
        ledger.record(scope(SCOPE), id(42));
        assert!(ledger.contains(scope(SCOPE), id(42)));
    }

    #[test]
    fn a_direct_reply_to_a_dropped_message_inherits() {
        let ledger = DropLedger::new();
        ledger.record(scope(SCOPE), id(100));
        assert!(ledger.reply_inherits_drop(scope(SCOPE), Some(id(100))));
    }

    #[test]
    fn a_non_reply_never_inherits() {
        let ledger = DropLedger::new();
        ledger.record(scope(SCOPE), id(100));
        assert!(!ledger.reply_inherits_drop(scope(SCOPE), None));
    }

    #[test]
    fn a_reply_to_an_undropped_message_does_not_inherit() {
        let ledger = DropLedger::new();
        ledger.record(scope(SCOPE), id(100));
        assert!(!ledger.reply_inherits_drop(scope(SCOPE), Some(id(101))));
    }

    #[test]
    fn a_reply_chain_inherits_transitively_once_each_hop_is_recorded() {
        let ledger = DropLedger::new();
        ledger.record(scope(SCOPE), id(1));
        // hop 2 replies to 1: inherits, and the caller records it
        assert!(ledger.reply_inherits_drop(scope(SCOPE), Some(id(1))));
        ledger.record(scope(SCOPE), id(2));
        // hop 3 replies to 2: inherits through the recorded hop
        assert!(ledger.reply_inherits_drop(scope(SCOPE), Some(id(2))));
    }

    #[test]
    fn record_is_idempotent_and_capacity_evicts_oldest_first_within_scope() {
        let ledger = DropLedger::new();
        ledger.record(scope(SCOPE), id(7));
        ledger.record(scope(SCOPE), id(7));
        for raw in 0..(SCOPE_CAPACITY as u64 + 8) {
            ledger.record(scope(SCOPE), id(1_000_000 + raw));
        }
        // the double-recorded id was oldest and must be gone exactly once over
        assert!(!ledger.contains(scope(SCOPE), id(7)));
        // newest survives
        assert!(ledger.contains(scope(SCOPE), id(1_000_000 + SCOPE_CAPACITY as u64 + 7)));
        // bound holds
        assert!(ledger.scope_len(scope(SCOPE)) <= SCOPE_CAPACITY);
    }

    /// The adversarial case from the #362 round-2 review: traffic spread
    /// across well over 64 distinct scopes must not evict any other scope's
    /// suppression history. The victim scope is touched once at the start
    /// and never again.
    #[test]
    fn sixty_five_plus_scopes_cannot_evict_an_untouched_scope() {
        let ledger = DropLedger::new();
        ledger.record(scope(SCOPE), id(500));
        for extra in 0..200u64 {
            ledger.record(scope(10_000 + extra), id(2_000_000 + extra));
        }
        // untouched throughout the churn, still intact
        assert!(ledger.contains(scope(SCOPE), id(500)));
        assert!(ledger.reply_inherits_drop(scope(SCOPE), Some(id(500))));
        // and every churned scope retained independently too
        assert!(ledger.contains(scope(10_000), id(2_000_000)));
        assert!(ledger.contains(scope(10_199), id(2_000_199)));
    }

    #[test]
    fn scopes_are_isolated_for_lookup() {
        let ledger = DropLedger::new();
        ledger.record(scope(SCOPE), id(300));
        assert!(!ledger.contains(scope(99), id(300)));
        assert!(!ledger.reply_inherits_drop(scope(99), Some(id(300))));
    }
}
