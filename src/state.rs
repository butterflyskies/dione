use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A pending permission relay request waiting for admin response.
pub struct PendingPermission {
    pub request_id: String,
    pub channel_id: u64,
    pub created_at: DateTime<Utc>,
}

/// Shared runtime state accessed by both MCP and Discord tasks.
pub struct SharedState {
    /// Recently sent Discord message IDs used for reply-to-bot detection.
    pub recent_sent_ids: BTreeSet<u64>,
    /// Maps Discord user IDs to their DM channel IDs.
    pub dm_channel_map: HashMap<u64, u64>,
    /// Reverse index: set of all known DM channel IDs for O(1) outbound gate checks.
    pub dm_channel_ids: HashSet<u64>,
    /// Pending permission relay requests keyed by Discord message ID.
    pub pending_permissions: BTreeMap<u64, PendingPermission>,
    /// Message IDs confirmed not authored by the bot (negative cache for reaction lookups).
    pub non_bot_message_ids: BTreeSet<u64>,
    /// Cache of user ID → username, populated from message events.
    pub user_names: BTreeMap<u64, String>,
    /// Cache of thread channel ID → parent channel ID. Populated when we
    /// encounter messages in threads and look up the parent via the Discord API.
    /// `None` means the channel was looked up and confirmed **not** to be a thread
    /// (negative cache), avoiding repeated HTTP calls.
    pub thread_parents: BTreeMap<u64, Option<u64>>,
}

/// Thread-safe shared state handle.
pub type State = Arc<RwLock<SharedState>>;

/// Maximum recent-sent-ID entries kept in memory.
const SENT_IDS_CAP: usize = 200;

/// Maximum thread-parent cache entries.
const THREAD_CACHE_CAP: usize = 200;

/// Stale threshold for pending permissions (5 minutes).
const PERMISSION_STALE_SECS: i64 = 300;

// ── Implementation ────────────────────────────────────────────────────────────

impl SharedState {
    /// Creates a new, empty shared state.
    pub fn new() -> Self {
        Self {
            recent_sent_ids: BTreeSet::new(),
            dm_channel_map: HashMap::new(),
            dm_channel_ids: HashSet::new(),
            pending_permissions: BTreeMap::new(),
            non_bot_message_ids: BTreeSet::new(),
            user_names: BTreeMap::new(),
            thread_parents: BTreeMap::new(),
        }
    }

    /// Records a DM channel mapping and updates the reverse index.
    pub fn record_dm_channel(&mut self, user_id: u64, channel_id: u64) {
        self.dm_channel_map.insert(user_id, channel_id);
        self.dm_channel_ids.insert(channel_id);
    }

    /// Records a sent message ID and auto-prunes if the set exceeds `cap`.
    pub fn note_sent(&mut self, id: u64) {
        self.recent_sent_ids.insert(id);
        self.prune_sent_ids(SENT_IDS_CAP);
    }

    /// Records a message ID confirmed not authored by the bot.
    pub fn note_non_bot(&mut self, id: u64) {
        self.non_bot_message_ids.insert(id);
        prune_oldest(&mut self.non_bot_message_ids, SENT_IDS_CAP);
    }

    /// Keeps only the most recent `cap` entries by ID value.
    ///
    /// Snowflake IDs are monotonically increasing, so higher values are newer.
    pub fn prune_sent_ids(&mut self, cap: usize) {
        prune_oldest(&mut self.recent_sent_ids, cap);
    }

    /// Records a thread → parent channel mapping for gate and notification lookups.
    ///
    /// Pass `Some(parent_id)` for confirmed threads, or `None` to negatively
    /// cache a channel that is not a thread (avoids repeated HTTP lookups).
    pub fn record_thread_parent(&mut self, thread_id: u64, parent_id: Option<u64>) {
        self.thread_parents.insert(thread_id, parent_id);
        // Prune if over cap — BTreeMap with snowflake keys evicts oldest first.
        while self.thread_parents.len() > THREAD_CACHE_CAP {
            if let Some(&oldest) = self.thread_parents.keys().next() {
                self.thread_parents.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Caches a user ID → username mapping, pruning if over cap.
    ///
    /// BTreeMap with snowflake keys evicts oldest (smallest) ID first.
    pub fn cache_username(&mut self, user_id: u64, name: String) {
        self.user_names.insert(user_id, name);
        while self.user_names.len() > SENT_IDS_CAP {
            if let Some(&oldest) = self.user_names.keys().next() {
                self.user_names.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Removes pending permission entries older than 5 minutes and returns
    /// the pruned `(channel_id, message_id)` pairs so callers can clean up
    /// the corresponding Discord messages.
    pub fn prune_stale_permissions(&mut self) -> Vec<(u64, u64)> {
        let cutoff = Utc::now() - chrono::Duration::seconds(PERMISSION_STALE_SECS);
        let mut stale = Vec::new();
        self.pending_permissions.retain(|&msg_id, p| {
            if p.created_at > cutoff {
                true
            } else {
                stale.push((p.channel_id, msg_id));
                false
            }
        });
        stale
    }
}

fn prune_oldest(set: &mut BTreeSet<u64>, cap: usize) {
    while set.len() > cap {
        if let Some(&oldest) = set.iter().next() {
            set.remove(&oldest);
        } else {
            break;
        }
    }
}

impl Default for SharedState {
    fn default() -> Self {
        Self::new()
    }
}

/// Creates a new `State` handle wrapping a fresh `SharedState`.
pub fn new_state() -> State {
    Arc::new(RwLock::new(SharedState::new()))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_prune_sent_ids() {
        let mut state = SharedState::new();
        for i in 0u64..10 {
            state.recent_sent_ids.insert(i);
        }
        state.prune_sent_ids(5);
        assert_eq!(state.recent_sent_ids.len(), 5);
        // Should keep the 5 largest (newest) IDs: 5,6,7,8,9.
        assert_eq!(
            state.recent_sent_ids.iter().copied().collect::<Vec<_>>(),
            vec![5, 6, 7, 8, 9]
        );
    }

    #[test]
    fn test_note_sent_auto_prunes() {
        let mut state = SharedState::new();
        for i in 0u64..=(SENT_IDS_CAP as u64 + 10) {
            state.note_sent(i);
        }
        assert!(
            state.recent_sent_ids.len() <= SENT_IDS_CAP,
            "sent IDs exceeded cap: {}",
            state.recent_sent_ids.len()
        );
    }

    #[test]
    fn test_record_thread_parent_prunes_at_cap() {
        let mut state = SharedState::new();
        // Insert 210 entries — all positive (Some).
        for i in 0u64..210 {
            state.record_thread_parent(i, Some(i + 1000));
        }
        assert!(
            state.thread_parents.len() <= THREAD_CACHE_CAP,
            "thread_parents exceeded cap: {}",
            state.thread_parents.len()
        );
        // The oldest (lowest) IDs should have been evicted.
        assert!(
            !state.thread_parents.contains_key(&0),
            "oldest entry (0) should be evicted"
        );
        assert!(
            !state.thread_parents.contains_key(&9),
            "oldest entry (9) should be evicted"
        );
        // Newest entries should still be present.
        assert!(
            state.thread_parents.contains_key(&209),
            "newest entry (209) should be retained"
        );
    }

    #[test]
    fn test_record_thread_parent_prunes_negative_cache_entries() {
        let mut state = SharedState::new();
        // Insert 210 entries — all negative (None), simulating channels confirmed not-threads.
        for i in 0u64..210 {
            state.record_thread_parent(i, None);
        }
        assert!(
            state.thread_parents.len() <= THREAD_CACHE_CAP,
            "thread_parents exceeded cap with negative entries: {}",
            state.thread_parents.len()
        );
        // Oldest negative entries should be evicted just like positive ones.
        assert!(
            !state.thread_parents.contains_key(&0),
            "oldest negative entry (0) should be evicted"
        );
        // Newest entries should still be present.
        assert!(
            state.thread_parents.contains_key(&209),
            "newest negative entry (209) should be retained"
        );
    }

    #[test]
    fn test_prune_stale_permissions() {
        let mut state = SharedState::new();

        let old_time = Utc::now() - chrono::Duration::seconds(PERMISSION_STALE_SECS + 10);
        let fresh_time = Utc::now() - chrono::Duration::seconds(10);

        state.pending_permissions.insert(
            1001,
            PendingPermission {
                request_id: "old-request".to_string(),
                channel_id: 9001,
                created_at: old_time,
            },
        );
        state.pending_permissions.insert(
            1002,
            PendingPermission {
                request_id: "fresh-request".to_string(),
                channel_id: 9002,
                created_at: fresh_time,
            },
        );

        let stale = state.prune_stale_permissions();

        assert!(
            !state.pending_permissions.contains_key(&1001),
            "stale permission should be pruned"
        );
        assert!(
            state.pending_permissions.contains_key(&1002),
            "fresh permission should be retained"
        );
        assert_eq!(stale.len(), 1, "should return one stale entry");
        assert_eq!(
            stale[0],
            (9001, 1001),
            "stale entry should contain channel_id and message_id"
        );
    }
}
