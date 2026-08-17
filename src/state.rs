use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serenity::{
    http::Http,
    model::id::{ChannelId, MessageId, WebhookId},
};
use tokio::sync::RwLock;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A pending permission relay request waiting for admin response.
pub struct PendingPermission {
    pub request_id: String,
    pub channel_id: ChannelId,
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
    /// Cache of webhook ID → observed creator facts (not a classification verdict).
    /// Stores the creator bot's user ID so provider semantics can be re-derived
    /// at use time under current policy (P3 — webhook cache stores facts).
    ///
    /// Uses HashMap + VecDeque for FIFO eviction (oldest insertion evicted
    /// first), avoiding the BTreeMap bias of evicting by smallest key.
    proxy_webhooks: HashMap<u64, WebhookCreatorInfo>,
    proxy_webhook_order: std::collections::VecDeque<u64>,
}

/// Observed facts about a webhook's creator.
///
/// Stores the creator's bot user ID (if available) rather than a derived
/// classification verdict, so provider semantics can be re-evaluated under
/// current policy without cache invalidation.
#[derive(Debug, Clone)]
struct WebhookCreatorInfo {
    /// The Discord user ID of the bot that created the webhook, if available.
    creator_bot_id: u64,
    observed_at: Instant,
}

/// Thread-safe shared state handle.
pub type State = Arc<RwLock<SharedState>>;

/// Maximum recent-sent-ID entries kept in memory.
const SENT_IDS_CAP: usize = 200;

const THREAD_CACHE_CAP: usize = 200;
const WEBHOOK_CACHE_CAP: usize = 200;
const WEBHOOK_CREATOR_TTL: Duration = Duration::from_secs(300);

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
            proxy_webhooks: HashMap::new(),
            proxy_webhook_order: std::collections::VecDeque::new(),
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

    /// Caches observed webhook creator facts (not a classification verdict).
    /// FIFO eviction: oldest insertion is evicted first, regardless of key value.
    fn record_proxy_webhook(&mut self, webhook_id: u64, creator_bot_id: u64) {
        self.proxy_webhook_order
            .retain(|queued| *queued != webhook_id);
        self.proxy_webhook_order.push_back(webhook_id);
        self.proxy_webhooks.insert(
            webhook_id,
            WebhookCreatorInfo {
                creator_bot_id,
                observed_at: Instant::now(),
            },
        );
        while self.proxy_webhooks.len() > WEBHOOK_CACHE_CAP {
            if let Some(oldest) = self.proxy_webhook_order.pop_front() {
                self.proxy_webhooks.remove(&oldest);
            } else {
                break;
            }
        }
    }

    /// Returns a fresh observed creator fact, physically removing stale facts.
    fn proxy_webhook_creator(&mut self, webhook_id: u64) -> Option<u64> {
        let now = Instant::now();
        self.proxy_webhooks.retain(|_, info| {
            now.saturating_duration_since(info.observed_at) < WEBHOOK_CREATOR_TTL
        });
        self.proxy_webhook_order
            .retain(|queued| self.proxy_webhooks.contains_key(queued));
        self.proxy_webhooks
            .get(&webhook_id)
            .map(|info| info.creator_bot_id)
    }

    /// Removes all pending permission entries matching a `request_id` and
    /// returns the `(ChannelId, MessageId)` pairs of removed siblings so
    /// callers can clean up the corresponding Discord messages.
    ///
    /// The removed message IDs are also re-recorded as bot-sent: callers
    /// delete these messages immediately after removal, and marking them here
    /// guarantees the resulting gateway `message_delete` events are suppressed
    /// even if the original send-time entry was evicted from the capped
    /// `recent_sent_ids` set in the meantime.
    pub fn remove_permissions_by_request_id(
        &mut self,
        request_id: &str,
    ) -> Vec<(ChannelId, MessageId)> {
        let mut removed = Vec::new();
        self.pending_permissions.retain(|&msg_id, p| {
            if p.request_id == request_id {
                removed.push((p.channel_id, MessageId::new(msg_id)));
                false
            } else {
                true
            }
        });
        for &(_, msg_id) in &removed {
            self.note_sent(msg_id.get());
        }
        removed
    }
}

/// Returns a fresh Discord-observed webhook creator fact.
///
/// Callers can request an observation but cannot seed creator facts. Only a
/// successful Discord lookup publishes a positive fact into the bounded cache.
pub(crate) async fn observe_webhook_creator(
    http: &Http,
    state: &State,
    webhook_id: WebhookId,
) -> Option<u64> {
    if let Some(creator_bot_id) = state.write().await.proxy_webhook_creator(webhook_id.get()) {
        return Some(creator_bot_id);
    }

    let webhook = match http.get_webhook(webhook_id).await {
        Ok(webhook) => webhook,
        Err(error) => {
            tracing::debug!(
                webhook_id = webhook_id.get(),
                %error,
                "failed to obtain Discord webhook creator facts"
            );
            return None;
        }
    };
    let creator_bot_id = webhook.user.as_ref().map(|user| user.id.get())?;
    state
        .write()
        .await
        .record_proxy_webhook(webhook_id.get(), creator_bot_id);
    Some(creator_bot_id)
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
    fn test_remove_permissions_by_request_id() {
        let mut state = SharedState::new();

        let now = Utc::now();
        // Two messages for the same request_id (multi-admin scenario).
        state.pending_permissions.insert(
            5001,
            PendingPermission {
                request_id: "shared-req".to_string(),
                channel_id: ChannelId::new(9001),
                created_at: now,
            },
        );
        state.pending_permissions.insert(
            5002,
            PendingPermission {
                request_id: "shared-req".to_string(),
                channel_id: ChannelId::new(9002),
                created_at: now,
            },
        );
        // Unrelated request should survive.
        state.pending_permissions.insert(
            5003,
            PendingPermission {
                request_id: "other-req".to_string(),
                channel_id: ChannelId::new(9003),
                created_at: now,
            },
        );

        let removed = state.remove_permissions_by_request_id("shared-req");

        assert_eq!(
            removed.len(),
            2,
            "both entries for shared-req should be removed"
        );
        assert_eq!(
            state.pending_permissions.len(),
            1,
            "unrelated entry should survive"
        );
        assert!(state.pending_permissions.contains_key(&5003));
    }

    /// Removing pending permissions marks the removed message IDs as
    /// bot-sent, so the cleanup deletions that follow are suppressed by the
    /// message_delete handler instead of being delivered via MCP.
    #[test]
    fn test_remove_permissions_marks_removed_ids_as_sent() {
        let mut state = SharedState::new();

        let now = Utc::now();
        for (msg_id, chan_id) in [(5001u64, 9001u64), (5002, 9002)] {
            state.pending_permissions.insert(
                msg_id,
                PendingPermission {
                    request_id: "shared-req".to_string(),
                    channel_id: ChannelId::new(chan_id),
                    created_at: now,
                },
            );
        }
        state.pending_permissions.insert(
            5003,
            PendingPermission {
                request_id: "other-req".to_string(),
                channel_id: ChannelId::new(9003),
                created_at: now,
            },
        );

        state.remove_permissions_by_request_id("shared-req");

        assert!(
            state.recent_sent_ids.contains(&5001),
            "removed prompt 5001 should be marked sent for delete suppression"
        );
        assert!(
            state.recent_sent_ids.contains(&5002),
            "removed sibling 5002 should be marked sent for delete suppression"
        );
        assert!(
            !state.recent_sent_ids.contains(&5003),
            "unrelated pending entry must not be marked sent"
        );
    }

    #[test]
    fn test_record_proxy_webhook() {
        let mut state = SharedState::new();
        state.record_proxy_webhook(100, 466378653216014359);
        assert_eq!(state.proxy_webhook_creator(100), Some(466378653216014359));
        assert_eq!(state.proxy_webhook_creator(300), None);
    }

    #[test]
    fn test_proxy_webhook_cache_prunes() {
        let mut state = SharedState::new();
        for i in 0u64..210 {
            state.record_proxy_webhook(i, i + 1);
        }
        assert!(
            state.proxy_webhooks.len() <= 200,
            "proxy_webhooks exceeded cap: {}",
            state.proxy_webhooks.len()
        );
    }

    #[test]
    fn proxy_webhook_cache_physically_removes_expired_facts() {
        let mut state = SharedState::new();
        state.record_proxy_webhook(100, 200);
        state.proxy_webhooks.get_mut(&100).unwrap().observed_at =
            Instant::now() - WEBHOOK_CREATOR_TTL - Duration::from_secs(1);

        assert_eq!(state.proxy_webhook_creator(100), None);
        assert!(!state.proxy_webhooks.contains_key(&100));
        assert!(!state.proxy_webhook_order.contains(&100));
    }
}
