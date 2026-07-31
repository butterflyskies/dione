//! Guild-level push mute store.
//!
//! Manages muted-server state: mute, unmute, query, and persistence.
//! Uses `ArcSwap` for lock-free reads consistent with the config pattern.
//! Persists to `$DIONE_STATE_DIR/guild_mutes.json` via atomic write.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use camino::Utf8Path;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

// ── Types ────────────────────────────────────────────────────────────────────

/// A single guild mute entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GuildMute {
    pub guild_id: u64,
    pub muted_until: DateTime<Utc>,
    pub muted_by: String,
    pub reason: Option<String>,
    pub muted_at: DateTime<Utc>,
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
    /// Expired entries are treated as unmuted (lazy expiry).
    pub fn is_guild_muted(&self, guild_id: u64) -> bool {
        self.mutes
            .get(&guild_id)
            .is_some_and(|mute| mute.is_active())
    }

    /// Returns all currently active mutes.
    pub fn active_mutes(&self) -> Vec<&GuildMute> {
        self.mutes.values().filter(|m| m.is_active()).collect()
    }

    /// Returns a new `MuteState` with expired entries removed.
    fn pruned(&self) -> Self {
        Self {
            mutes: self
                .mutes
                .iter()
                .filter(|(_, m)| m.is_active())
                .map(|(&k, v)| (k, v.clone()))
                .collect(),
        }
    }
}

// ── Store ────────────────────────────────────────────────────────────────────

/// Thread-safe mute store with lock-free reads and atomic persistence.
pub struct MuteStore {
    state: ArcSwap<MuteState>,
    file_path: camino::Utf8PathBuf,
}

impl MuteStore {
    /// Load mute state from disk, pruning expired entries.
    pub async fn load(state_dir: &Utf8Path) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let file_path = state_dir.join("guild_mutes.json");
        let state = match tokio::fs::read_to_string(&file_path).await {
            Ok(contents) => {
                let raw: MuteState = serde_json::from_str(&contents)?;
                let pruned = raw.pruned();
                tracing::info!(
                    active = pruned.mutes.len(),
                    "loaded guild mute state"
                );
                pruned
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                tracing::debug!("no guild mutes file found, starting empty");
                MuteState::default()
            }
            Err(e) => return Err(e.into()),
        };

        Ok(Self {
            state: ArcSwap::from_pointee(state),
            file_path,
        })
    }

    /// Create a store from an initial state (for testing or wiring).
    pub fn from_state(state: MuteState, state_dir: &Utf8Path) -> Self {
        Self {
            state: ArcSwap::from_pointee(state),
            file_path: state_dir.join("guild_mutes.json"),
        }
    }

    /// Lock-free read of current mute state.
    pub fn load_state(&self) -> Arc<MuteState> {
        self.state.load_full()
    }

    /// Returns `true` if the guild is currently muted.
    pub fn is_guild_muted(&self, guild_id: u64) -> bool {
        self.state.load().is_guild_muted(guild_id)
    }

    /// Mute a guild for `ttl_minutes` from now.
    ///
    /// Re-issuing a mute that would shorten the existing one is rejected
    /// (requires explicit unmute-then-mute to shorten).
    pub async fn mute_guild(
        &self,
        guild_id: u64,
        ttl_minutes: u64,
        muted_by: String,
        reason: Option<String>,
    ) -> Result<GuildMute, String> {
        let now = Utc::now();
        let muted_until = now + chrono::Duration::minutes(ttl_minutes as i64);

        // Guard against accidental shortening.
        let current = self.state.load_full();
        if let Some(existing) = current.mutes.get(&guild_id) {
            if existing.is_active() && muted_until < existing.muted_until {
                return Err(format!(
                    "guild {} is already muted until {} ({} seconds remaining); \
                     new TTL would shorten the mute. Unmute first to replace.",
                    guild_id,
                    existing.muted_until.to_rfc3339(),
                    existing.remaining_seconds(),
                ));
            }
        }

        let mute = GuildMute {
            guild_id,
            muted_until,
            muted_by: muted_by.clone(),
            reason: reason.clone(),
            muted_at: now,
        };

        let mut new_state = MuteState::clone(&current);
        new_state.mutes.insert(guild_id, mute.clone());
        self.state.store(Arc::new(new_state));

        self.save().await.map_err(|e| format!("failed to persist mute state: {e}"))?;

        tracing::info!(
            guild_id,
            muted_until = %muted_until.to_rfc3339(),
            muted_by = %muted_by,
            reason = reason.as_deref().unwrap_or("(none)"),
            ttl_minutes,
            "guild muted"
        );

        Ok(mute)
    }

    /// Manually unmute a guild. Returns the removed mute entry, or an error
    /// if the guild was not muted.
    pub async fn unmute_guild(&self, guild_id: u64) -> Result<GuildMute, String> {
        let current = self.state.load_full();
        let existing = current
            .mutes
            .get(&guild_id)
            .filter(|m| m.is_active())
            .cloned()
            .ok_or_else(|| format!("guild {guild_id} is not currently muted"))?;

        let mut new_state = MuteState::clone(&current);
        new_state.mutes.remove(&guild_id);
        self.state.store(Arc::new(new_state));

        self.save().await.map_err(|e| format!("failed to persist mute state: {e}"))?;

        tracing::info!(
            guild_id,
            was_muted_until = %existing.muted_until.to_rfc3339(),
            "guild manually unmuted"
        );

        Ok(existing)
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

    /// Atomic write to disk (write-tmp then rename).
    async fn save(&self) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let state = self.state.load();
        // Only persist active mutes.
        let pruned = state.pruned();
        let serialized = serde_json::to_string_pretty(&pruned)?;

        let tmp_path = format!("{}.tmp", self.file_path);
        tokio::fs::write(&tmp_path, &serialized).await?;
        if let Err(e) = tokio::fs::rename(&tmp_path, &self.file_path).await {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(e.into());
        }
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
        };
        assert!(mute.is_active());
        assert!(mute.remaining_seconds() > 0);
    }

    #[test]
    fn pruned_removes_expired_entries() {
        let mut state = MuteState::default();
        state.mutes.insert(
            1,
            GuildMute {
                guild_id: 1,
                muted_until: Utc::now() - chrono::Duration::seconds(10),
                muted_by: "test".into(),
                reason: None,
                muted_at: Utc::now() - chrono::Duration::seconds(70),
            },
        );
        state.mutes.insert(
            2,
            GuildMute {
                guild_id: 2,
                muted_until: Utc::now() + chrono::Duration::seconds(300),
                muted_by: "test".into(),
                reason: None,
                muted_at: Utc::now(),
            },
        );
        let pruned = state.pruned();
        assert_eq!(pruned.mutes.len(), 1);
        assert!(pruned.mutes.contains_key(&2));
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
            },
        );

        let store = MuteStore::from_state(state, path);
        let active = store.list_muted();
        assert_eq!(active.len(), 1);
        assert_eq!(active[0].guild_id, 1);
    }

    #[tokio::test]
    async fn persistence_roundtrip() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = camino::Utf8Path::from_path(dir.path()).unwrap();
        let store = empty_store(path);

        store
            .mute_guild(42, 60, "admin".into(), Some("test".into()))
            .await
            .expect("mute should succeed");

        // Load from the same path and verify.
        let store2 = MuteStore::load(path).await.expect("load should succeed");
        assert!(store2.is_guild_muted(42));
        let mutes = store2.list_muted();
        assert_eq!(mutes.len(), 1);
        assert_eq!(mutes[0].guild_id, 42);
        assert_eq!(mutes[0].reason.as_deref(), Some("test"));
    }
}
