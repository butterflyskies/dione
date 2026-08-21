use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, io};

/// Maximum preview length stored per access request.
const PREVIEW_MAX: usize = 100;

// ── Types ─────────────────────────────────────────────────────────────────────

/// A pending access request from an unknown Discord user.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccessRequest {
    pub user_id: u64,
    pub username: String,
    pub message_preview: String,
    pub timestamp: DateTime<Utc>,
}

/// In-memory access request queue backed by `queue.json`.
pub struct AccessQueue {
    entries: BTreeMap<u64, AccessRequest>,
    state_dir: Utf8PathBuf,
    last_admin_notify: Option<DateTime<Utc>>,
}

// ── Implementation ────────────────────────────────────────────────────────────

impl AccessQueue {
    /// Loads the queue from `{state_dir}/queue.json`, or starts empty.
    pub fn load(state_dir: &Utf8Path) -> Self {
        let queue_path = state_dir.join("queue.json");
        let entries = try_load_queue(&queue_path).unwrap_or_else(|e| {
            tracing::warn!(path = %queue_path, error = %e, "failed to load queue, starting empty");
            BTreeMap::new()
        });

        Self {
            entries,
            state_dir: state_dir.to_owned(),
            last_admin_notify: None,
        }
    }

    /// Enqueues an access request.
    ///
    /// - Deduplicates by `user_id`.
    /// - Enforces `max_pending` cap (silently drops excess).
    /// - Truncates `message_preview` to 100 chars.
    /// - Persists on change.
    ///
    /// Returns `true` if the request was actually added (new entry).
    pub fn enqueue(&mut self, mut request: AccessRequest, max_pending: usize) -> bool {
        // Dedup: if the user already has a pending request, skip.
        if self.entries.contains_key(&request.user_id) {
            return false;
        }

        // Cap: if we're at the limit, silently drop.
        if self.entries.len() >= max_pending {
            tracing::debug!(
                user_id = request.user_id,
                "access queue full, dropping request"
            );
            return false;
        }

        // Truncate preview.
        if request.message_preview.len() > PREVIEW_MAX {
            let boundary = request.message_preview.floor_char_boundary(PREVIEW_MAX);
            request.message_preview.truncate(boundary);
        }

        self.entries.insert(request.user_id, request);

        if let Err(e) = self.persist() {
            tracing::warn!(error = %e, "failed to persist access queue after enqueue");
        }

        true
    }

    /// Removes entries older than `expiry`.
    ///
    /// Persists if any entries were removed.
    pub fn prune_expired(&mut self, expiry: std::time::Duration) {
        let cutoff = Utc::now()
            - chrono::Duration::from_std(expiry).unwrap_or(chrono::Duration::seconds(86400));

        let before = self.entries.len();
        self.entries.retain(|_id, req| req.timestamp > cutoff);
        let after = self.entries.len();

        if before != after {
            tracing::debug!(removed = before - after, "pruned expired access requests");
            if let Err(e) = self.persist() {
                tracing::warn!(error = %e, "failed to persist queue after pruning");
            }
        }
    }

    /// Approves an access request, removing it from the queue.
    ///
    /// Returns the removed request if it existed.
    pub fn approve(&mut self, user_id: u64) -> Option<AccessRequest> {
        let entry = self.entries.remove(&user_id);
        if entry.is_some()
            && let Err(e) = self.persist()
        {
            tracing::warn!(error = %e, "failed to persist queue after approve");
        }
        entry
    }

    /// Denies an access request, removing it from the queue.
    ///
    /// Returns the removed request if it existed.
    pub fn deny(&mut self, user_id: u64) -> Option<AccessRequest> {
        let entry = self.entries.remove(&user_id);
        if entry.is_some()
            && let Err(e) = self.persist()
        {
            tracing::warn!(error = %e, "failed to persist queue after deny");
        }
        entry
    }

    /// Returns a reference to the pending request for `user_id`, if any.
    pub fn peek(&self, user_id: u64) -> Option<&AccessRequest> {
        self.entries.get(&user_id)
    }

    /// Returns all pending requests in chronological order (oldest first).
    pub fn list(&self) -> Vec<&AccessRequest> {
        let mut entries: Vec<_> = self.entries.values().collect();
        entries.sort_by_key(|r| r.timestamp);
        entries
    }

    /// Returns `true` if an admin notification should be sent now.
    ///
    /// Rate-limited by `cooldown`.
    pub fn should_notify_admin(&self, cooldown: std::time::Duration) -> bool {
        if self.entries.is_empty() {
            return false;
        }
        match self.last_admin_notify {
            None => true,
            Some(last) => {
                let elapsed = Utc::now().signed_duration_since(last);
                let cooldown_duration =
                    chrono::Duration::from_std(cooldown).unwrap_or(chrono::Duration::seconds(60));
                elapsed >= cooldown_duration
            }
        }
    }

    /// Records that an admin notification was sent now.
    pub fn mark_notified(&mut self) {
        self.last_admin_notify = Some(Utc::now());
    }

    /// Atomically writes the queue to `{state_dir}/queue.json`.
    ///
    /// Uses write-to-tmp then rename for atomicity.
    pub fn persist(&self) -> io::Result<()> {
        let queue_path = self.state_dir.join("queue.json");
        let tmp_path = self.state_dir.join("queue.json.tmp");

        // Ensure state dir exists.
        std::fs::create_dir_all(self.state_dir.as_std_path())?;

        let json = serde_json::to_vec_pretty(&self.entries)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        std::fs::write(tmp_path.as_std_path(), &json)?;
        std::fs::rename(tmp_path.as_std_path(), queue_path.as_std_path())?;

        Ok(())
    }
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn try_load_queue(path: &Utf8Path) -> io::Result<BTreeMap<u64, AccessRequest>> {
    let bytes = match std::fs::read(path.as_std_path()) {
        Ok(b) => b,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(BTreeMap::new()),
        Err(e) => return Err(e),
    };
    serde_json::from_slice(&bytes).map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn temp_state() -> (TempDir, Utf8PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(dir.path().to_path_buf()).unwrap();
        (dir, path)
    }

    fn make_request(user_id: u64, preview: &str) -> AccessRequest {
        AccessRequest {
            user_id,
            username: format!("user_{user_id}"),
            message_preview: preview.to_string(),
            timestamp: Utc::now(),
        }
    }

    #[test]
    fn test_enqueue_dedup_by_user_id() {
        let (_dir, state_dir) = temp_state();
        let mut q = AccessQueue::load(&state_dir);

        let added1 = q.enqueue(make_request(42, "first"), 50);
        let added2 = q.enqueue(make_request(42, "second"), 50);

        assert!(added1, "first enqueue should succeed");
        assert!(!added2, "duplicate enqueue should return false");
        assert_eq!(q.list().len(), 1);
    }

    #[test]
    fn test_enqueue_cap_enforced() {
        let (_dir, state_dir) = temp_state();
        let mut q = AccessQueue::load(&state_dir);

        for i in 0u64..5 {
            q.enqueue(make_request(i, "hello"), 5);
        }

        // Should be at cap.
        assert_eq!(q.list().len(), 5);

        // Adding one more should be silently dropped.
        let added = q.enqueue(make_request(99, "overflow"), 5);
        assert!(!added);
        assert_eq!(q.list().len(), 5);
    }

    #[test]
    fn test_preview_truncated_to_100() {
        let (_dir, state_dir) = temp_state();
        let mut q = AccessQueue::load(&state_dir);
        let long_preview = "a".repeat(200);
        q.enqueue(make_request(1, &long_preview), 50);

        let entries = q.list();
        assert_eq!(entries[0].message_preview.len(), 100);
    }

    #[test]
    fn test_prune_expired() {
        let (_dir, state_dir) = temp_state();
        let mut q = AccessQueue::load(&state_dir);

        // Add an old request.
        let old_req = AccessRequest {
            user_id: 1,
            username: "old_user".to_string(),
            message_preview: "hello".to_string(),
            timestamp: Utc::now() - chrono::Duration::hours(25),
        };
        q.entries.insert(1, old_req);

        // Add a fresh request.
        q.enqueue(make_request(2, "fresh"), 50);

        q.prune_expired(std::time::Duration::from_secs(86400));

        let entries = q.list();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].user_id, 2);
    }

    #[test]
    fn test_should_notify_admin_cooldown() {
        let (_dir, state_dir) = temp_state();
        let mut q = AccessQueue::load(&state_dir);
        q.enqueue(make_request(1, "hello"), 50);

        let cooldown = std::time::Duration::from_secs(60);

        // First notify — should be allowed.
        assert!(q.should_notify_admin(cooldown));
        q.mark_notified();

        // Immediately after — should be rate-limited.
        assert!(!q.should_notify_admin(cooldown));
    }

    #[test]
    fn test_approve_removes_from_queue() {
        let (_dir, state_dir) = temp_state();
        let mut q = AccessQueue::load(&state_dir);
        q.enqueue(make_request(5, "please let me in"), 50);

        let removed = q.approve(5);
        assert!(removed.is_some());
        assert_eq!(q.list().len(), 0);

        // Approving again returns None.
        assert!(q.approve(5).is_none());
    }

    #[test]
    fn test_deny_removes_from_queue() {
        let (_dir, state_dir) = temp_state();
        let mut q = AccessQueue::load(&state_dir);
        q.enqueue(make_request(7, "let me in"), 50);

        let removed = q.deny(7);
        assert!(removed.is_some());
        assert_eq!(q.list().len(), 0);
    }

    #[test]
    fn test_persist_atomic_write() {
        let (_dir, state_dir) = temp_state();
        let mut q = AccessQueue::load(&state_dir);
        q.enqueue(make_request(99, "test persistence"), 50);

        // Persist should succeed and queue.json should exist.
        let queue_path = state_dir.join("queue.json");
        assert!(queue_path.as_std_path().exists(), "queue.json should exist");

        // .tmp file should be cleaned up.
        let tmp_path = state_dir.join("queue.json.tmp");
        assert!(
            !tmp_path.as_std_path().exists(),
            ".tmp should be cleaned up"
        );

        // Loading it back should restore the entry.
        let q2 = AccessQueue::load(&state_dir);
        assert_eq!(q2.list().len(), 1);
        assert_eq!(q2.list()[0].user_id, 99);
    }
}
