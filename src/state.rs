use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use tokio::sync::RwLock;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A pending permission relay request waiting for admin response.
pub struct PendingPermission {
    pub request_id: String,
    pub created_at: DateTime<Utc>,
}

/// Shared runtime state accessed by both MCP and Discord tasks.
pub struct SharedState {
    /// Recently sent Discord message IDs used for reply-to-bot detection.
    pub recent_sent_ids: BTreeSet<u64>,
    /// Maps Discord user IDs to their DM channel IDs.
    pub dm_channel_map: HashMap<u64, u64>,
    /// Pending permission relay requests keyed by Discord message ID.
    pub pending_permissions: BTreeMap<u64, PendingPermission>,
}

/// Thread-safe shared state handle.
pub type State = Arc<RwLock<SharedState>>;

/// Maximum recent-sent-ID entries kept in memory.
const SENT_IDS_CAP: usize = 200;

/// Stale threshold for pending permissions (5 minutes).
const PERMISSION_STALE_SECS: i64 = 300;

// ── Implementation ────────────────────────────────────────────────────────────

impl SharedState {
    /// Creates a new, empty shared state.
    pub fn new() -> Self {
        Self {
            recent_sent_ids: BTreeSet::new(),
            dm_channel_map: HashMap::new(),
            pending_permissions: BTreeMap::new(),
        }
    }

    /// Records a sent message ID and auto-prunes if the set exceeds `cap`.
    pub fn note_sent(&mut self, id: u64) {
        self.recent_sent_ids.insert(id);
        self.prune_sent_ids(SENT_IDS_CAP);
    }

    /// Keeps only the most recent `cap` entries by ID value.
    ///
    /// Snowflake IDs are monotonically increasing, so higher values are newer.
    pub fn prune_sent_ids(&mut self, cap: usize) {
        while self.recent_sent_ids.len() > cap {
            // Remove the smallest (oldest) ID.
            if let Some(&oldest) = self.recent_sent_ids.iter().next() {
                self.recent_sent_ids.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Removes pending permission entries older than 5 minutes.
    pub fn prune_stale_permissions(&mut self) {
        let cutoff = Utc::now() - chrono::Duration::seconds(PERMISSION_STALE_SECS);
        self.pending_permissions
            .retain(|_msg_id, p| p.created_at > cutoff);
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
    fn test_prune_stale_permissions() {
        let mut state = SharedState::new();

        let old_time = Utc::now() - chrono::Duration::seconds(PERMISSION_STALE_SECS + 10);
        let fresh_time = Utc::now() - chrono::Duration::seconds(10);

        state.pending_permissions.insert(
            1001,
            PendingPermission {
                request_id: "old-request".to_string(),
                created_at: old_time,
            },
        );
        state.pending_permissions.insert(
            1002,
            PendingPermission {
                request_id: "fresh-request".to_string(),
                created_at: fresh_time,
            },
        );

        state.prune_stale_permissions();

        assert!(
            !state.pending_permissions.contains_key(&1001),
            "stale permission should be pruned"
        );
        assert!(
            state.pending_permissions.contains_key(&1002),
            "fresh permission should be retained"
        );
    }
}
