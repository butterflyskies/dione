//! Guild-level push mute store.
//!
//! The append-only JSONL receipt log (`guild_mute_receipts.jsonl`) is the
//! canonical source of truth for mute state. On startup the log is replayed
//! to rebuild the in-memory `MuteState` projection — there is no cache file;
//! the receipt log is small enough that replay is always fast.
//!
//! All mutations (mute, unmute, extend) are serialized through a
//! `tokio::sync::Mutex`. The receipt is appended to the log *first* — this
//! is the commit point. Only after a successful append is the in-memory
//! projection updated. If the receipt write fails, the operation fails and
//! no state changes.
//!
//! The read-only `is_guild_muted()` path is pure: it checks the projection
//! and returns `false` for expired entries without emitting receipts.
//! Explicit `reconcile_expiries()` runs under the writer lock to emit
//! `Expire` receipts for entries whose TTL has elapsed.
//!
//! A background expiry task (`spawn_expiry_task`) sleeps until the nearest
//! `muted_until` deadline and then calls `reconcile_expiries` to emit
//! `Expire` receipts promptly. When no active mutes exist it parks on a
//! `Notify` that `mute_guild` triggers.

use arc_swap::ArcSwap;
use camino::Utf8Path;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::sync::{Mutex, Notify};

// ── Types ────────────────────────────────────────────────────────────────────

/// Maximum allowed TTL in minutes (30 days).
pub const MAX_TTL_MINUTES: u64 = 43200;

/// A single guild mute entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuildMute {
    pub guild_id: u64,
    pub muted_until: DateTime<Utc>,
    pub muted_by: String,
    pub reason: Option<String>,
    pub muted_at: DateTime<Utc>,
    /// Deterministic identity for classifying pre-cutoff stragglers.
    #[serde(default)]
    pub cutoff_event_id: String,
}

impl GuildMute {
    /// Returns `true` if this mute is still active (not expired).
    pub fn is_active(&self) -> bool {
        Utc::now() < self.muted_until
    }

    /// Returns the remaining duration in seconds, or 0 if expired.
    pub fn remaining_seconds(&self) -> i64 {
        (self.muted_until - Utc::now()).num_seconds().max(0)
    }
}

/// Snapshot of all guild mutes.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MuteState {
    pub mutes: HashMap<u64, GuildMute>,
}

impl MuteState {
    /// Returns `true` if push delivery for the given guild is currently muted.
    ///
    /// Pure read — expired entries return `false` but are not removed.
    /// Use `reconcile_expiries()` to emit `Expire` receipts and clean up.
    pub fn is_guild_muted(&self, guild_id: u64) -> bool {
        self.mutes
            .get(&guild_id)
            .is_some_and(|mute| mute.is_active())
    }

    /// Returns all currently active mutes.
    pub fn active_mutes(&self) -> Vec<&GuildMute> {
        self.mutes.values().filter(|m| m.is_active()).collect()
    }
}

// ── Receipt types ───────────────────────────────────────────────────────────

/// The type of mute lifecycle operation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MuteOperation {
    Mute,
    Unmute,
    Extend,
    Expire,
}

/// A durable, append-only lifecycle receipt for audit history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MuteReceipt {
    pub timestamp: DateTime<Utc>,
    pub guild_id: u64,
    pub operation: MuteOperation,
    pub actor: String,
    pub reason: Option<String>,
    /// For Mute/Extend: the new expiry time.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub muted_until: Option<DateTime<Utc>>,
    /// Opaque event identity for classifying pre-cutoff stragglers.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cutoff_event_id: Option<String>,
}

// ── Helpers ─────────────────────────────────────────────────────────────────

/// Generate a deterministic cutoff event identity from timestamp + guild_id.
fn generate_cutoff_event_id(timestamp: &DateTime<Utc>, guild_id: u64) -> String {
    format!(
        "{}-{}",
        timestamp.timestamp_nanos_opt().unwrap_or(0),
        guild_id
    )
}

/// Apply a single receipt to the mute state projection.
fn apply_receipt(state: &mut MuteState, receipt: &MuteReceipt) {
    match receipt.operation {
        MuteOperation::Mute | MuteOperation::Extend => {
            if let Some(muted_until) = receipt.muted_until {
                state.mutes.insert(
                    receipt.guild_id,
                    GuildMute {
                        guild_id: receipt.guild_id,
                        muted_until,
                        muted_by: receipt.actor.clone(),
                        reason: receipt.reason.clone(),
                        muted_at: receipt.timestamp,
                        cutoff_event_id: receipt.cutoff_event_id.clone().unwrap_or_default(),
                    },
                );
            }
        }
        MuteOperation::Unmute | MuteOperation::Expire => {
            state.mutes.remove(&receipt.guild_id);
        }
    }
}

/// Replay every receipt line in the log to rebuild the projection.
fn replay_receipt_log(
    contents: &str,
) -> Result<MuteState, Box<dyn std::error::Error + Send + Sync>> {
    let mut state = MuteState::default();
    for (i, line) in contents.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let receipt: MuteReceipt = serde_json::from_str(trimmed)
            .map_err(|e| format!("corrupt receipt at line {}: {e}", i + 1))?;
        apply_receipt(&mut state, &receipt);
    }
    Ok(state)
}

// ── Store ────────────────────────────────────────────────────────────────────

/// Thread-safe mute store with lock-free reads and receipt-log-canonical persistence.
///
/// All mutations go through `write_lock` to serialize operations. The receipt
/// log append is the commit point — the ArcSwap is updated only after a
/// successful write.
pub struct MuteStore {
    state: ArcSwap<MuteState>,
    receipt_path: camino::Utf8PathBuf,
    /// Serializes all write operations (mute, unmute, extend, reconcile).
    write_lock: Mutex<()>,
    /// Wakes the background expiry task when a new mute is created.
    expiry_notify: Notify,
    /// Test-only: fires when the scheduler has armed its select! branch.
    #[cfg(test)]
    scheduler_ready: Notify,
}

impl MuteStore {
    /// Load mute state by replaying the receipt log.
    ///
    /// The full log is always replayed — there is no cache.
    ///
    /// After loading, `reconcile_expiries` is called to emit `Expire`
    /// receipts for any entries whose TTL has elapsed while the process
    /// was down.
    pub async fn load(
        state_dir: &Utf8Path,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let receipt_path = state_dir.join("guild_mute_receipts.jsonl");

        let state = Self::replay_log(&receipt_path).await?;

        let active = state.mutes.values().filter(|m| m.is_active()).count();
        let total = state.mutes.len();
        tracing::info!(active, total, "loaded guild mute state from receipt log");

        let store = Self {
            state: ArcSwap::from_pointee(state),
            receipt_path,
            write_lock: Mutex::new(()),
            expiry_notify: Notify::new(),
            #[cfg(test)]
            scheduler_ready: Notify::new(),
        };

        // Reconcile expired entries — emits Expire receipts for entries
        // that expired while we were offline.
        if let Err(e) = store.reconcile_expiries().await {
            tracing::warn!(error = %e, "failed to reconcile expired guild mutes at startup");
        }

        Ok(store)
    }

    /// Replay the receipt log to build the projection.
    async fn replay_log(
        receipt_path: &camino::Utf8Path,
    ) -> Result<MuteState, Box<dyn std::error::Error + Send + Sync>> {
        let log_contents = match tokio::fs::read_to_string(receipt_path).await {
            Ok(c) => c,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!("no receipt log found, starting with empty mute state");
                return Ok(MuteState::default());
            }
            Err(e) => return Err(e.into()),
        };

        if log_contents.trim().is_empty() {
            tracing::debug!("receipt log is empty, starting with empty mute state");
            return Ok(MuteState::default());
        }

        let line_count = log_contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .count();
        tracing::info!(lines = line_count, "replaying receipt log");
        replay_receipt_log(&log_contents)
    }

    /// Create a store from an initial state (for testing or wiring).
    pub fn from_state(state: MuteState, state_dir: &Utf8Path) -> Self {
        Self {
            state: ArcSwap::from_pointee(state),
            receipt_path: state_dir.join("guild_mute_receipts.jsonl"),
            write_lock: Mutex::new(()),
            expiry_notify: Notify::new(),
            #[cfg(test)]
            scheduler_ready: Notify::new(),
        }
    }

    /// Spawn a background task that reconciles expired mutes at their
    /// deadline rather than waiting for the next startup.
    ///
    /// The task sleeps until the nearest `muted_until` among active mutes,
    /// then calls `reconcile_expiries`. When no active mutes exist it parks
    /// on `expiry_notify` until `mute_guild` wakes it.
    ///
    /// Returns a `JoinHandle` that can be aborted on shutdown.
    pub fn spawn_expiry_task(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let store = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                let (next_deadline, has_overdue) = {
                    let state = store.load_state();
                    let has_overdue = state.mutes.values().any(|m| !m.is_active());
                    let next_active = state
                        .mutes
                        .values()
                        .filter(|m| m.is_active())
                        .map(|m| m.muted_until)
                        .min();
                    (next_active, has_overdue)
                };
                // Overdue entries (expired but unreceipted) need immediate
                // reconciliation — don't park on Notify when work is pending.
                if has_overdue {
                    match store.reconcile_expiries().await {
                        Ok(_) => {}
                        Err(e) => {
                            tracing::warn!("expiry reconciliation retry failed: {e}");
                            tokio::time::sleep(Duration::from_secs(5)).await;
                        }
                    }
                    continue;
                }
                match next_deadline {
                    Some(deadline) => {
                        let duration = (deadline - Utc::now())
                            .to_std()
                            .unwrap_or(Duration::from_secs(0));
                        // Register the notified future BEFORE the select to
                        // avoid a race where a notification fires between the
                        // deadline calculation and the sleep. Pin and enable
                        // so the waiter is registered before test readiness
                        // fires.
                        let notified = store.expiry_notify.notified();
                        tokio::pin!(notified);
                        #[cfg(test)]
                        {
                            notified.as_mut().enable();
                            store.scheduler_ready.notify_one();
                        }
                        tokio::select! {
                            _ = tokio::time::sleep(duration) => {
                                // Deadline reached — reconcile expired entries.
                                match store.reconcile_expiries().await {
                                    Ok(_) => {}
                                    Err(e) => {
                                        tracing::warn!("expiry reconciliation failed: {e}");
                                        // Retry after backoff so failed entries
                                        // aren't forgotten.
                                        tokio::time::sleep(Duration::from_secs(5)).await;
                                    }
                                }
                            }
                            _ = &mut notified => {
                                // New mute arrived — recalculate deadline.
                                continue;
                            }
                        }
                    }
                    None => {
                        // No active mutes — park until notified.
                        store.expiry_notify.notified().await;
                    }
                }
            }
        })
    }

    /// Lock-free read of current mute state.
    pub fn load_state(&self) -> Arc<MuteState> {
        self.state.load_full()
    }

    /// Returns `true` if the guild is currently muted.
    ///
    /// Pure read — does not emit receipts or modify state.
    pub fn is_guild_muted(&self, guild_id: u64) -> bool {
        self.state.load().is_guild_muted(guild_id)
    }

    /// Mute a guild for `ttl_minutes` from now.
    ///
    /// Re-issuing a mute that would shorten the existing one is rejected
    /// (requires explicit unmute-then-mute to shorten). If the guild is
    /// already muted and the new TTL extends it, this is recorded as an
    /// `Extend` operation.
    ///
    /// `ttl_minutes` must be in `1..=43200` (30 days).
    pub async fn mute_guild(
        &self,
        guild_id: u64,
        ttl_minutes: u64,
        muted_by: String,
        reason: Option<String>,
    ) -> Result<GuildMute, String> {
        if ttl_minutes > MAX_TTL_MINUTES {
            return Err(format!(
                "ttl_minutes ({ttl_minutes}) exceeds maximum ({MAX_TTL_MINUTES})"
            ));
        }

        let now = Utc::now();
        let duration = chrono::Duration::try_minutes(ttl_minutes as i64)
            .ok_or_else(|| format!("ttl_minutes ({ttl_minutes}) overflows duration"))?;
        let muted_until = now + duration;

        let _guard = self.write_lock.lock().await;

        // Re-read state under the lock to get a consistent snapshot.
        let current = self.state.load_full();
        let is_extend = if let Some(existing) = current.mutes.get(&guild_id) {
            if existing.is_active() && muted_until < existing.muted_until {
                return Err(format!(
                    "guild {} is already muted until {} ({} seconds remaining); \
                     new TTL would shorten the mute. Unmute first to replace.",
                    guild_id,
                    existing.muted_until.to_rfc3339(),
                    existing.remaining_seconds(),
                ));
            }
            existing.is_active()
        } else {
            false
        };

        let operation = if is_extend {
            MuteOperation::Extend
        } else {
            MuteOperation::Mute
        };

        let cutoff_event_id = generate_cutoff_event_id(&now, guild_id);

        // Build the receipt.
        let receipt = MuteReceipt {
            timestamp: now,
            guild_id,
            operation: operation.clone(),
            actor: muted_by.clone(),
            reason: reason.clone(),
            muted_until: Some(muted_until),
            cutoff_event_id: Some(cutoff_event_id.clone()),
        };

        // Append receipt FIRST — this is the commit point.
        self.append_receipt(&receipt)
            .await
            .map_err(|e| format!("failed to append mute receipt: {e}"))?;

        // Derive projection from the receipt.
        let mute = GuildMute {
            guild_id,
            muted_until,
            muted_by: muted_by.clone(),
            reason: reason.clone(),
            muted_at: now,
            cutoff_event_id,
        };

        let mut new_state = MuteState::clone(&current);
        new_state.mutes.insert(guild_id, mute.clone());

        // Update in-memory projection.
        self.state.store(Arc::new(new_state));

        tracing::info!(
            guild_id,
            muted_until = %muted_until.to_rfc3339(),
            muted_by = %muted_by,
            reason = reason.as_deref().unwrap_or("(none)"),
            ttl_minutes,
            ?operation,
            "guild muted"
        );

        // Wake the expiry task so it recalculates the next deadline.
        self.expiry_notify.notify_one();

        Ok(mute)
    }

    /// Manually unmute a guild. Returns the removed mute entry, or an error
    /// if the guild was not muted.
    pub async fn unmute_guild(&self, guild_id: u64) -> Result<GuildMute, String> {
        let _guard = self.write_lock.lock().await;

        // Re-read state under the lock.
        let current = self.state.load_full();
        let existing = current
            .mutes
            .get(&guild_id)
            .filter(|m| m.is_active())
            .cloned()
            .ok_or_else(|| format!("guild {guild_id} is not currently muted"))?;

        // Build the receipt.
        let receipt = MuteReceipt {
            timestamp: Utc::now(),
            guild_id,
            operation: MuteOperation::Unmute,
            actor: existing.muted_by.clone(),
            reason: None,
            muted_until: None,
            cutoff_event_id: None,
        };

        // Append receipt FIRST — this is the commit point.
        self.append_receipt(&receipt)
            .await
            .map_err(|e| format!("failed to append unmute receipt: {e}"))?;

        // Derive projection.
        let mut new_state = MuteState::clone(&current);
        new_state.mutes.remove(&guild_id);

        // Update in-memory projection.
        self.state.store(Arc::new(new_state));

        tracing::info!(
            guild_id,
            was_muted_until = %existing.muted_until.to_rfc3339(),
            "guild manually unmuted"
        );

        Ok(existing)
    }

    /// Reconcile expired mute entries by emitting `Expire` receipts and
    /// removing them from the projection.
    ///
    /// Called at startup (after replay) and periodically by the expiry task.
    /// Each `Expire` receipt carries the original `muted_until` and
    /// `cutoff_event_id` for audit continuity.
    pub async fn reconcile_expiries(
        &self,
    ) -> Result<usize, Box<dyn std::error::Error + Send + Sync>> {
        let _guard = self.write_lock.lock().await;

        let current = self.state.load_full();

        // Collect expired entries.
        let expired: Vec<GuildMute> = current
            .mutes
            .values()
            .filter(|m| !m.is_active())
            .cloned()
            .collect();

        if expired.is_empty() {
            return Ok(0);
        }

        // Process each expired entry individually: only remove entries whose
        // Expire receipt was successfully appended. Failed entries stay in the
        // projection so the next reconciliation pass can retry them.
        let mut succeeded: Vec<u64> = Vec::new();
        let mut last_error: Option<Box<dyn std::error::Error + Send + Sync>> = None;

        for mute in &expired {
            let receipt = MuteReceipt {
                timestamp: Utc::now(),
                guild_id: mute.guild_id,
                operation: MuteOperation::Expire,
                actor: "system".into(),
                reason: None,
                muted_until: Some(mute.muted_until),
                cutoff_event_id: if mute.cutoff_event_id.is_empty() {
                    None
                } else {
                    Some(mute.cutoff_event_id.clone())
                },
            };
            match self.append_receipt(&receipt).await {
                Ok(()) => succeeded.push(mute.guild_id),
                Err(e) => {
                    tracing::warn!(
                        guild_id = mute.guild_id,
                        error = %e,
                        "failed to append Expire receipt; entry will be retried"
                    );
                    last_error = Some(e);
                }
            }
        }

        // Only remove entries whose receipts were successfully written.
        if !succeeded.is_empty() {
            let mut new_state = MuteState::clone(&current);
            for guild_id in &succeeded {
                new_state.mutes.remove(guild_id);
            }
            self.state.store(Arc::new(new_state));
        }

        let ok_count = succeeded.len();
        if ok_count > 0 {
            tracing::info!(reconciled = ok_count, "reconciled expired guild mutes");
        }

        // If any entry failed, propagate the last error so the caller can
        // retry (with backoff).
        if let Some(e) = last_error {
            return Err(e);
        }

        Ok(ok_count)
    }

    /// List all currently active mutes.
    pub fn list_muted(&self) -> Vec<GuildMute> {
        let state = self.state.load();
        state
            .mutes
            .values()
            .filter(|m| m.is_active())
            .cloned()
            .collect()
    }

    /// Append a receipt to the JSONL receipt log.
    ///
    /// Flushes and fsyncs (`sync_data`) before returning so the receipt is
    /// durable on disk before the in-memory projection is updated.
    async fn append_receipt(
        &self,
        receipt: &MuteReceipt,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        use tokio::io::AsyncWriteExt;

        let line = serde_json::to_string(receipt)?;
        let mut with_newline = line;
        with_newline.push('\n');

        let mut file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.receipt_path)
            .await?;
        file.write_all(with_newline.as_bytes()).await?;
        file.flush().await?;
        file.sync_data().await?;

        Ok(())
    }
}

// ── Global accessor ─────────────────────────────────────────────────────────

/// Process-global mute store, initialized at startup.
static MUTE_STORE: std::sync::OnceLock<Arc<MuteStore>> = std::sync::OnceLock::new();

/// Initialize the global mute store. Called once at startup.
pub fn init_global(store: MuteStore) {
    MUTE_STORE.set(Arc::new(store)).ok();
}

/// Load the global mute store. Returns `None` if not yet initialized.
pub fn global() -> Option<Arc<MuteStore>> {
    MUTE_STORE.get().cloned()
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn empty_store(state_dir: &Utf8Path) -> MuteStore {
        MuteStore::from_state(MuteState::default(), state_dir)
    }

    /// Helper to write receipt lines directly to a log file for testing
    /// replay without going through the store's mutation methods.
    fn write_receipt_log(state_dir: &Utf8Path, receipts: &[MuteReceipt]) {
        let path = state_dir.join("guild_mute_receipts.jsonl");
        let mut contents = String::new();
        for r in receipts {
            contents.push_str(&serde_json::to_string(r).unwrap());
            contents.push('\n');
        }
        std::fs::write(path, contents).unwrap();
    }

    #[test]
    fn is_guild_muted_returns_false_for_unknown_guild() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);
        assert!(!store.is_guild_muted(12345));
    }

    #[test]
    fn expired_mute_is_not_active() {
        let mute = GuildMute {
            guild_id: 1,
            muted_until: Utc::now() - chrono::Duration::seconds(10),
            muted_by: "test".into(),
            reason: None,
            muted_at: Utc::now() - chrono::Duration::seconds(70),
            cutoff_event_id: String::new(),
        };
        assert!(!mute.is_active());
        assert_eq!(mute.remaining_seconds(), 0);
    }

    #[test]
    fn active_mute_is_active() {
        let mute = GuildMute {
            guild_id: 1,
            muted_until: Utc::now() + chrono::Duration::seconds(60),
            muted_by: "test".into(),
            reason: Some("testing".into()),
            muted_at: Utc::now(),
            cutoff_event_id: String::new(),
        };
        assert!(mute.is_active());
        assert!(mute.remaining_seconds() > 0);
    }

    #[test]
    fn apply_receipt_mute_inserts_entry() {
        let mut state = MuteState::default();
        let now = Utc::now();
        let receipt = MuteReceipt {
            timestamp: now,
            guild_id: 42,
            operation: MuteOperation::Mute,
            actor: "admin".into(),
            reason: Some("noisy".into()),
            muted_until: Some(now + chrono::Duration::minutes(60)),
            cutoff_event_id: Some("cutoff-42".into()),
        };
        apply_receipt(&mut state, &receipt);
        assert_eq!(state.mutes.len(), 1);
        let mute = state.mutes.get(&42).unwrap();
        assert_eq!(mute.guild_id, 42);
        assert_eq!(mute.cutoff_event_id, "cutoff-42");
        assert_eq!(mute.reason.as_deref(), Some("noisy"));
    }

    #[test]
    fn apply_receipt_unmute_removes_entry() {
        let mut state = MuteState::default();
        let now = Utc::now();
        // Insert a mute.
        apply_receipt(
            &mut state,
            &MuteReceipt {
                timestamp: now,
                guild_id: 42,
                operation: MuteOperation::Mute,
                actor: "admin".into(),
                reason: None,
                muted_until: Some(now + chrono::Duration::minutes(60)),
                cutoff_event_id: None,
            },
        );
        assert!(state.mutes.contains_key(&42));

        // Unmute.
        apply_receipt(
            &mut state,
            &MuteReceipt {
                timestamp: now,
                guild_id: 42,
                operation: MuteOperation::Unmute,
                actor: "admin".into(),
                reason: None,
                muted_until: None,
                cutoff_event_id: None,
            },
        );
        assert!(!state.mutes.contains_key(&42));
    }

    #[test]
    fn apply_receipt_expire_removes_entry() {
        let mut state = MuteState::default();
        let now = Utc::now();
        apply_receipt(
            &mut state,
            &MuteReceipt {
                timestamp: now,
                guild_id: 42,
                operation: MuteOperation::Mute,
                actor: "admin".into(),
                reason: None,
                muted_until: Some(now + chrono::Duration::minutes(60)),
                cutoff_event_id: None,
            },
        );
        apply_receipt(
            &mut state,
            &MuteReceipt {
                timestamp: now,
                guild_id: 42,
                operation: MuteOperation::Expire,
                actor: "system".into(),
                reason: None,
                muted_until: Some(now + chrono::Duration::minutes(60)),
                cutoff_event_id: None,
            },
        );
        assert!(!state.mutes.contains_key(&42));
    }

    #[test]
    fn replay_receipt_log_rebuilds_state() {
        let now = Utc::now();
        let muted_until = now + chrono::Duration::minutes(60);
        let mut log = String::new();

        // Mute guild 1.
        let r1 = serde_json::to_string(&MuteReceipt {
            timestamp: now,
            guild_id: 1,
            operation: MuteOperation::Mute,
            actor: "admin".into(),
            reason: Some("reason1".into()),
            muted_until: Some(muted_until),
            cutoff_event_id: Some("cutoff-1".into()),
        })
        .unwrap();
        log.push_str(&r1);
        log.push('\n');

        // Mute guild 2.
        let r2 = serde_json::to_string(&MuteReceipt {
            timestamp: now,
            guild_id: 2,
            operation: MuteOperation::Mute,
            actor: "admin".into(),
            reason: None,
            muted_until: Some(muted_until),
            cutoff_event_id: None,
        })
        .unwrap();
        log.push_str(&r2);
        log.push('\n');

        // Unmute guild 1.
        let r3 = serde_json::to_string(&MuteReceipt {
            timestamp: now,
            guild_id: 1,
            operation: MuteOperation::Unmute,
            actor: "admin".into(),
            reason: None,
            muted_until: None,
            cutoff_event_id: None,
        })
        .unwrap();
        log.push_str(&r3);
        log.push('\n');

        let state = replay_receipt_log(&log).unwrap();
        assert_eq!(state.mutes.len(), 1);
        assert!(!state.mutes.contains_key(&1));
        assert!(state.mutes.contains_key(&2));
    }

    #[tokio::test]
    async fn mute_and_unmute_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        // Mute guild 42.
        let mute = store
            .mute_guild(42, 60, "admin".into(), Some("noisy".into()))
            .await
            .expect("mute should succeed");
        assert_eq!(mute.guild_id, 42);
        assert!(store.is_guild_muted(42));
        assert!(
            !mute.cutoff_event_id.is_empty(),
            "cutoff_event_id must be set"
        );

        // Unmute guild 42.
        let removed = store.unmute_guild(42).await.expect("unmute should succeed");
        assert_eq!(removed.guild_id, 42);
        assert!(!store.is_guild_muted(42));
    }

    #[tokio::test]
    async fn mute_rejects_shorter_ttl() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        store
            .mute_guild(42, 120, "admin".into(), None)
            .await
            .expect("initial mute should succeed");

        // Attempt to shorten to 1 minute — should fail.
        let result = store.mute_guild(42, 1, "admin".into(), None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("shorten"));
    }

    #[tokio::test]
    async fn mute_allows_extension() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        store
            .mute_guild(42, 10, "admin".into(), None)
            .await
            .expect("initial mute should succeed");

        // Extend to 120 minutes — should succeed.
        let extended = store
            .mute_guild(42, 120, "admin".into(), Some("extended".into()))
            .await
            .expect("extension should succeed");
        assert!(extended.remaining_seconds() > 60 * 10);
    }

    #[tokio::test]
    async fn unmute_nonexistent_guild_errors() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        let result = store.unmute_guild(999).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("not currently muted"));
    }

    #[tokio::test]
    async fn list_muted_returns_only_active() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();

        let mut state = MuteState::default();
        // Active mute.
        state.mutes.insert(
            1,
            GuildMute {
                guild_id: 1,
                muted_until: Utc::now() + chrono::Duration::seconds(300),
                muted_by: "admin".into(),
                reason: None,
                muted_at: Utc::now(),
                cutoff_event_id: String::new(),
            },
        );
        // Expired mute.
        state.mutes.insert(
            2,
            GuildMute {
                guild_id: 2,
                muted_until: Utc::now() - chrono::Duration::seconds(10),
                muted_by: "admin".into(),
                reason: None,
                muted_at: Utc::now() - chrono::Duration::seconds(70),
                cutoff_event_id: String::new(),
            },
        );

        let store = MuteStore::from_state(state, path);
        let active = store.list_muted();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].guild_id, 1);
    }

    #[tokio::test]
    async fn persistence_roundtrip_via_receipt_log() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        store
            .mute_guild(42, 60, "admin".into(), Some("test".into()))
            .await
            .expect("mute should succeed");

        // Load from the same path — replays receipt log.
        let store2 = MuteStore::load(path).await.expect("load should succeed");
        assert!(store2.is_guild_muted(42));
        let mutes = store2.list_muted();
        assert_eq!(mutes.len(), 1);
        assert_eq!(mutes[0].guild_id, 42);
        assert_eq!(mutes[0].reason.as_deref(), Some("test"));
        assert!(
            !mutes[0].cutoff_event_id.is_empty(),
            "cutoff_event_id should survive reload"
        );
    }

    #[tokio::test]
    async fn load_always_replays_receipt_log() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();

        let now = Utc::now();
        let muted_until = now + chrono::Duration::minutes(60);

        // Write a receipt log.
        let receipt = MuteReceipt {
            timestamp: now,
            guild_id: 42,
            operation: MuteOperation::Mute,
            actor: "admin".into(),
            reason: Some("from-log".into()),
            muted_until: Some(muted_until),
            cutoff_event_id: Some("cutoff-42".into()),
        };
        write_receipt_log(path, &[receipt]);

        // Load — always replays the log.
        let store = MuteStore::load(path).await.expect("load should succeed");
        assert!(
            store.is_guild_muted(42),
            "guild 42 should be muted from log replay"
        );
        let listed = store.list_muted();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].cutoff_event_id, "cutoff-42");
    }

    #[tokio::test]
    async fn load_starts_empty_when_no_receipt_log() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();

        // No files at all.
        let store = MuteStore::load(path).await.expect("load should succeed");
        assert!(store.list_muted().is_empty());
    }

    #[tokio::test]
    async fn reconcile_expiries_emits_expire_receipts() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();

        let mut state = MuteState::default();
        // Expired entry.
        state.mutes.insert(
            1,
            GuildMute {
                guild_id: 1,
                muted_until: Utc::now() - chrono::Duration::seconds(10),
                muted_by: "admin".into(),
                reason: Some("expired".into()),
                muted_at: Utc::now() - chrono::Duration::seconds(70),
                cutoff_event_id: "cutoff-1".into(),
            },
        );
        // Active entry.
        state.mutes.insert(
            2,
            GuildMute {
                guild_id: 2,
                muted_until: Utc::now() + chrono::Duration::seconds(300),
                muted_by: "admin".into(),
                reason: None,
                muted_at: Utc::now(),
                cutoff_event_id: "cutoff-2".into(),
            },
        );

        let store = MuteStore::from_state(state, path);

        let reconciled = store
            .reconcile_expiries()
            .await
            .expect("reconcile should succeed");
        assert_eq!(reconciled, 1, "one entry should have been reconciled");

        // State should only have the active mute.
        assert!(!store.is_guild_muted(1));
        assert!(store.is_guild_muted(2));

        // Receipt log should contain the Expire receipt.
        let receipt_path = path.join("guild_mute_receipts.jsonl");
        let contents = std::fs::read_to_string(receipt_path).expect("receipt file should exist");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 1);
        let r: MuteReceipt = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(r.operation, MuteOperation::Expire);
        assert_eq!(r.guild_id, 1);
        assert_eq!(r.actor, "system");
        assert_eq!(r.cutoff_event_id.as_deref(), Some("cutoff-1"));
    }

    #[tokio::test]
    async fn reconcile_expiries_noop_when_all_active() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();

        let mut state = MuteState::default();
        state.mutes.insert(
            1,
            GuildMute {
                guild_id: 1,
                muted_until: Utc::now() + chrono::Duration::seconds(300),
                muted_by: "admin".into(),
                reason: None,
                muted_at: Utc::now(),
                cutoff_event_id: String::new(),
            },
        );

        let store = MuteStore::from_state(state, path);
        let reconciled = store
            .reconcile_expiries()
            .await
            .expect("reconcile should succeed");
        assert_eq!(reconciled, 0);

        // No receipt log should be created.
        let receipt_path = path.join("guild_mute_receipts.jsonl");
        assert!(!receipt_path.exists());
    }

    #[tokio::test]
    async fn mute_rejects_ttl_over_max() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        let result = store
            .mute_guild(42, MAX_TTL_MINUTES + 1, "admin".into(), None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("exceeds maximum"));
    }

    #[tokio::test]
    async fn receipt_log_records_mute_and_unmute() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        store
            .mute_guild(42, 60, "admin".into(), Some("test".into()))
            .await
            .expect("mute should succeed");

        store.unmute_guild(42).await.expect("unmute should succeed");

        // Read the receipt log.
        let receipt_path = path.join("guild_mute_receipts.jsonl");
        let contents = std::fs::read_to_string(&receipt_path).expect("receipt file should exist");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "should have exactly 2 receipts");

        let r1: MuteReceipt = serde_json::from_str(lines[0]).expect("valid receipt JSON");
        assert_eq!(r1.operation, MuteOperation::Mute);
        assert_eq!(r1.guild_id, 42);
        assert!(r1.muted_until.is_some());
        assert!(
            r1.cutoff_event_id.is_some(),
            "Mute receipt must carry cutoff_event_id"
        );

        let r2: MuteReceipt = serde_json::from_str(lines[1]).expect("valid receipt JSON");
        assert_eq!(r2.operation, MuteOperation::Unmute);
        assert_eq!(r2.guild_id, 42);
        assert!(r2.muted_until.is_none());
    }

    #[tokio::test]
    async fn receipt_log_records_extend() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        store
            .mute_guild(42, 10, "admin".into(), None)
            .await
            .expect("initial mute should succeed");

        store
            .mute_guild(42, 120, "admin".into(), Some("extended".into()))
            .await
            .expect("extension should succeed");

        let receipt_path = path.join("guild_mute_receipts.jsonl");
        let contents = std::fs::read_to_string(&receipt_path).expect("receipt file should exist");
        let lines: Vec<&str> = contents.lines().collect();
        assert_eq!(lines.len(), 2, "should have mute + extend receipts");

        let r1: MuteReceipt = serde_json::from_str(lines[0]).expect("valid receipt JSON");
        assert_eq!(r1.operation, MuteOperation::Mute);

        let r2: MuteReceipt = serde_json::from_str(lines[1]).expect("valid receipt JSON");
        assert_eq!(r2.operation, MuteOperation::Extend);
        assert_eq!(r2.guild_id, 42);
        assert!(r2.muted_until.is_some());
    }

    #[tokio::test]
    async fn receipt_append_failure_prevents_state_change() {
        // Verify that if append_receipt fails (unwritable dir),
        // the ArcSwap is NOT updated.
        let store = MuteStore {
            state: ArcSwap::from_pointee(MuteState::default()),
            receipt_path: camino::Utf8PathBuf::from("/nonexistent/dir/guild_mute_receipts.jsonl"),
            write_lock: Mutex::new(()),
            expiry_notify: Notify::new(),
            #[cfg(test)]
            scheduler_ready: Notify::new(),
        };

        let result = store.mute_guild(42, 60, "admin".into(), None).await;
        assert!(result.is_err(), "mute should fail when receipt write fails");
        assert!(
            !store.is_guild_muted(42),
            "ArcSwap must not be updated when receipt write fails"
        );
    }

    #[tokio::test]
    async fn cutoff_event_id_is_deterministic() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        let mute = store
            .mute_guild(42, 60, "admin".into(), None)
            .await
            .expect("mute should succeed");

        // cutoff_event_id should be timestamp_nanos-guild_id
        assert!(!mute.cutoff_event_id.is_empty());
        assert!(
            mute.cutoff_event_id.ends_with("-42"),
            "cutoff_event_id should end with '-guild_id', got: {}",
            mute.cutoff_event_id
        );
    }

    #[tokio::test]
    async fn startup_reconciles_expired_entries() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();

        let now = Utc::now();

        // Write a receipt log with an already-expired mute.
        let receipt = MuteReceipt {
            timestamp: now - chrono::Duration::minutes(120),
            guild_id: 42,
            operation: MuteOperation::Mute,
            actor: "admin".into(),
            reason: Some("will expire".into()),
            muted_until: Some(now - chrono::Duration::seconds(10)),
            cutoff_event_id: Some("cutoff-42".into()),
        };
        write_receipt_log(path, &[receipt]);

        // Load — reconcile_expiries runs at startup.
        let store = MuteStore::load(path).await.expect("load should succeed");
        assert!(
            !store.is_guild_muted(42),
            "expired mute should not be active after load"
        );
        assert!(
            store.load_state().mutes.is_empty(),
            "expired entry should be removed from projection after reconciliation"
        );

        // The receipt log should now have 2 entries: Mute + Expire.
        let receipt_path = path.join("guild_mute_receipts.jsonl");
        let contents = std::fs::read_to_string(receipt_path).unwrap();
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 2, "should have Mute + Expire receipts");
        let r2: MuteReceipt = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(r2.operation, MuteOperation::Expire);
        assert_eq!(r2.guild_id, 42);
        assert_eq!(r2.actor, "system");
    }

    // ── Scheduler invariant tests ──────────────────────────────────────────

    #[tokio::test]
    async fn earlier_deadline_mute_wakes_scheduler() {
        // When the scheduler is sleeping until a distant deadline and a new
        // overdue entry appears, expiry_notify must wake the scheduler so it
        // recalculates and reconciles the overdue entry — rather than
        // sleeping until the original deadline.
        //
        // Note: tokio::time::pause cannot control chrono::Utc::now() which
        // the store uses for is_active(). This test uses real wall-clock
        // time with short sleeps for synchronization.
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();

        let mut state = MuteState::default();
        // Guild 42: active mute expiring far in the future (10 minutes).
        // The scheduler will sleep for ~600 seconds.
        state.mutes.insert(
            42,
            GuildMute {
                guild_id: 42,
                muted_until: Utc::now() + chrono::Duration::seconds(600),
                muted_by: "admin".into(),
                reason: Some("long mute".into()),
                muted_at: Utc::now(),
                cutoff_event_id: "cutoff-42".into(),
            },
        );

        let store = Arc::new(MuteStore::from_state(state, path));
        let handle = store.spawn_expiry_task();

        // Wait for the spawned task to arm its select! branch
        // (sleeping until guild 42's distant deadline).
        store.scheduler_ready.notified().await;

        // Inject an already-expired entry directly into the projection.
        // This simulates a mute whose deadline was earlier and has already
        // passed — the scheduler must wake up and reconcile it rather than
        // continuing to sleep for the original 600s.
        {
            let current = store.load_state();
            let mut new_state = MuteState::clone(&current);
            new_state.mutes.insert(
                99,
                GuildMute {
                    guild_id: 99,
                    muted_until: Utc::now() - chrono::Duration::seconds(1),
                    muted_by: "admin".into(),
                    reason: Some("already expired".into()),
                    muted_at: Utc::now() - chrono::Duration::seconds(61),
                    cutoff_event_id: "cutoff-99".into(),
                },
            );
            store.state.store(Arc::new(new_state));
        }

        // Wake the scheduler — same mechanism mute_guild uses.
        store.expiry_notify.notify_one();

        // Poll until the scheduler has reconciled guild 99.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !store.load_state().mutes.contains_key(&99) { break; }
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for guild 99 reconciliation");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // Guild 99 (overdue) should be reconciled immediately upon wake.
        assert!(
            !store.load_state().mutes.contains_key(&99),
            "overdue guild 99 should be reconciled after scheduler wake"
        );

        // Guild 42 (still active, 10 min remaining) must NOT be touched.
        assert!(
            store.is_guild_muted(42),
            "guild 42 should still be active (not yet at deadline)"
        );

        // The scheduler should still be running — not deadlocked on the
        // old 600s sleep.
        assert!(!handle.is_finished(), "expiry task should still be running");

        // Verify the Expire receipt was written for guild 99.
        let receipt_path = path.join("guild_mute_receipts.jsonl");
        let contents = std::fs::read_to_string(receipt_path).expect("receipt file should exist");
        let receipts: Vec<MuteReceipt> = contents
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(receipts.len(), 1);
        assert_eq!(receipts[0].operation, MuteOperation::Expire);
        assert_eq!(receipts[0].guild_id, 99);

        handle.abort();
    }

    #[tokio::test]
    async fn failed_reconciliation_leaves_entry_for_retry() {
        // When append_receipt fails, reconcile_expiries must leave the
        // failed entry in the HashMap so the scheduler can retry it.
        let bad_path = camino::Utf8Path::new("/nonexistent/dir");

        let mut state = MuteState::default();
        // Expired entry — needs reconciliation.
        state.mutes.insert(
            1,
            GuildMute {
                guild_id: 1,
                muted_until: Utc::now() - chrono::Duration::seconds(10),
                muted_by: "admin".into(),
                reason: Some("expired".into()),
                muted_at: Utc::now() - chrono::Duration::seconds(70),
                cutoff_event_id: "cutoff-1".into(),
            },
        );

        let store = MuteStore::from_state(state, bad_path);

        // Reconciliation should fail (unwritable path).
        let result = store.reconcile_expiries().await;
        assert!(
            result.is_err(),
            "reconcile should fail when receipt write fails"
        );

        // The failed entry must remain in the projection.
        let post_state = store.load_state();
        assert!(
            post_state.mutes.contains_key(&1),
            "failed entry must stay in HashMap for retry"
        );

        // The has_overdue check (same logic the scheduler uses) should
        // find the entry and trigger immediate reconciliation.
        let has_overdue = post_state.mutes.values().any(|m| !m.is_active());
        assert!(
            has_overdue,
            "has_overdue must be true so scheduler retries rather than parking"
        );

        // Now verify that reconciliation succeeds with a writable path.
        let dir = tempfile::TempDir::new().unwrap();
        let writable_path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store2 = MuteStore::from_state(MuteState::clone(&post_state), writable_path);

        let reconciled = store2
            .reconcile_expiries()
            .await
            .expect("reconcile should succeed with writable path");
        assert_eq!(
            reconciled, 1,
            "the previously-failed entry should reconcile"
        );
        assert!(
            store2.load_state().mutes.is_empty(),
            "entry should be removed after successful reconciliation"
        );
    }

    #[tokio::test]
    async fn overdue_entries_trigger_immediate_reconciliation() {
        // When the projection contains expired-but-unreceipted entries
        // (e.g. from a previous failed reconciliation), spawn_expiry_task
        // must reconcile them immediately — not park on Notify (which is
        // the bug that was fixed in 7ced6a0).
        //
        // Uses real wall-clock time with a short sleep for synchronization,
        // since is_active() checks chrono::Utc::now().
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();

        let mut state = MuteState::default();
        // Pre-populate an expired entry (simulates a previous failure that
        // left the entry in the HashMap).
        state.mutes.insert(
            1,
            GuildMute {
                guild_id: 1,
                muted_until: Utc::now() - chrono::Duration::seconds(10),
                muted_by: "admin".into(),
                reason: Some("overdue".into()),
                muted_at: Utc::now() - chrono::Duration::seconds(70),
                cutoff_event_id: "cutoff-1".into(),
            },
        );

        let store = Arc::new(MuteStore::from_state(state, path));

        // Verify the overdue entry is present before spawning.
        assert!(
            store.load_state().mutes.contains_key(&1),
            "overdue entry should be present before spawn"
        );

        // Spawn the expiry task.
        let handle = store.spawn_expiry_task();
        tokio::task::yield_now().await;

        // Poll until the scheduler has reconciled the overdue entry.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if !store.load_state().mutes.contains_key(&1) { break; }
            assert!(tokio::time::Instant::now() < deadline, "timed out waiting for overdue reconciliation");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }

        // The overdue entry should be reconciled.
        assert!(
            !store.load_state().mutes.contains_key(&1),
            "overdue entry must be reconciled immediately, not parked on Notify"
        );

        // After reconciliation the store is empty — the scheduler should
        // park on Notify (not spin). Verify the task is still alive.
        assert!(
            !handle.is_finished(),
            "expiry task should be parked on Notify, not exited"
        );

        // Verify the Expire receipt was written.
        let receipt_path = path.join("guild_mute_receipts.jsonl");
        let contents = std::fs::read_to_string(receipt_path).expect("receipt file should exist");
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "should have exactly one Expire receipt");
        let r: MuteReceipt = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(r.operation, MuteOperation::Expire);
        assert_eq!(r.guild_id, 1);

        handle.abort();
    }

    #[tokio::test]
    async fn scheduler_retries_after_failure_and_commits_on_recovery() {
        // End-to-end: the scheduler's first reconciliation fails (bad path),
        // then the path becomes writable, and the scheduler retries through
        // the backoff and commits exactly one Expire receipt — no Notify
        // needed, the overdue branch drives the retry.
        //
        // This exercises the production code path:
        //   overdue detected → reconcile → fail → backoff → loop →
        //   overdue still detected → reconcile → succeed → receipt committed

        // Phase 1: create a store with an expired entry and a non-writable
        // receipt path.
        let dir = tempfile::TempDir::new().unwrap();
        let bad_path = dir.path().join("nonexistent_subdir");
        let bad_utf8 = camino::Utf8Path::from_path(&bad_path).unwrap();

        let mut state = MuteState::default();
        state.mutes.insert(
            77,
            GuildMute {
                guild_id: 77,
                muted_until: Utc::now() - chrono::Duration::seconds(10),
                muted_by: "admin".into(),
                reason: Some("will-fail".into()),
                muted_at: Utc::now() - chrono::Duration::seconds(70),
                cutoff_event_id: "cutoff-77".into(),
            },
        );

        let store = Arc::new(MuteStore::from_state(state, bad_utf8));

        // Spawn the scheduler — it will detect the overdue entry and try
        // to reconcile, but the receipt append will fail (directory doesn't
        // exist). It should backoff and retry.
        let handle = store.spawn_expiry_task();

        // Wait long enough for the first attempt + backoff start (the
        // backoff is 5 seconds, but the attempt itself is fast).
        tokio::time::sleep(Duration::from_millis(100)).await;

        // The entry should STILL be in the HashMap — the failed
        // reconciliation must not remove it.
        assert!(
            store.load_state().mutes.contains_key(&77),
            "failed reconciliation must not remove the entry"
        );

        // The task should still be alive (retrying, not parked).
        assert!(
            !handle.is_finished(),
            "scheduler should be alive and retrying"
        );

        // Phase 2: create the directory so the receipt path becomes writable.
        std::fs::create_dir_all(&bad_path).expect("should create directory");

        // Wait for the backoff (5s) to elapse and the retry to succeed.
        // Use 6s to give margin.
        tokio::time::sleep(Duration::from_secs(6)).await;

        // The entry should now be reconciled.
        assert!(
            !store.load_state().mutes.contains_key(&77),
            "entry should be reconciled after path becomes writable"
        );

        // Verify exactly one Expire receipt was committed.
        let receipt_file = bad_path.join("guild_mute_receipts.jsonl");
        let contents = std::fs::read_to_string(&receipt_file).expect("receipt file should exist");
        let lines: Vec<&str> = contents.lines().filter(|l| !l.trim().is_empty()).collect();
        assert_eq!(lines.len(), 1, "exactly one Expire receipt");
        let r: MuteReceipt = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(r.operation, MuteOperation::Expire);
        assert_eq!(r.guild_id, 77);

        // Task should still be alive (parked on Notify, no more work).
        assert!(
            !handle.is_finished(),
            "scheduler should be parked on Notify after successful reconciliation"
        );

        handle.abort();
    }
}
