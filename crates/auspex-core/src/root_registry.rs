use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::receipt::ActivationRoot;
use crate::verdict::RootVerdict;
use crate::{ChannelRef, EpochId, MessageRef, RootId};

const DEFAULT_TTL: Duration = Duration::from_secs(7200);
const DEFAULT_MAX_ROOTS: usize = 16_384;

/// A minted root with its activation source and timing.
#[derive(Debug, Clone)]
struct RootRecord {
    root_id: RootId,
    root: ActivationRoot,
    epoch: EpochId,
    admitted_at: Instant,
    expires: Instant,
}

/// Coverage state for the registry — tracks whether evidence is
/// complete or degraded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoverageState {
    /// The registry has been running continuously since the current epoch.
    Complete,
    /// There was a transport gap; evidence may be missing.
    TransportGap,
}

/// Thread-safe root registry. Mints `RootId` on admission, tracks epochs,
/// and returns typed verdicts.
///
/// This is the Slice A deliverable: diagnostics and evidence surface.
/// It does NOT gate egress — that requires the Slice B custody binding.
pub struct RootRegistry {
    roots_by_message: Mutex<HashMap<MessageRef, RootRecord>>,
    roots_by_id: Mutex<HashMap<RootId, MessageRef>>,
    next_root_id: AtomicU64,
    epoch: Mutex<EpochId>,
    next_epoch_id: AtomicU64,
    epoch_start: Mutex<Instant>,
    coverage: Mutex<CoverageState>,
    ttl: Duration,
    max_roots: usize,
}

impl Default for RootRegistry {
    fn default() -> Self {
        Self {
            roots_by_message: Mutex::new(HashMap::new()),
            roots_by_id: Mutex::new(HashMap::new()),
            next_root_id: AtomicU64::new(1),
            epoch: Mutex::new(EpochId::new(1)),
            next_epoch_id: AtomicU64::new(2),
            epoch_start: Mutex::new(Instant::now()),
            coverage: Mutex::new(CoverageState::Complete),
            ttl: DEFAULT_TTL,
            max_roots: DEFAULT_MAX_ROOTS,
        }
    }
}

impl RootRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            ..Self::default()
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn with_capacity(max_roots: usize) -> Self {
        Self {
            max_roots,
            ..Self::default()
        }
    }

    pub fn current_epoch(&self) -> EpochId {
        *self.epoch.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn coverage(&self) -> CoverageState {
        *self.coverage.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Open a new epoch. Called on process restart or reconnection.
    /// All prior roots remain queryable until they expire, but new
    /// admissions belong to the new epoch.
    pub fn new_epoch(&self) -> EpochId {
        let id = EpochId::new(self.next_epoch_id.fetch_add(1, Ordering::Relaxed));
        if let Ok(mut epoch) = self.epoch.lock() {
            *epoch = id;
        }
        if let Ok(mut start) = self.epoch_start.lock() {
            *start = Instant::now();
        }
        if let Ok(mut cov) = self.coverage.lock() {
            *cov = CoverageState::Complete;
        }
        id
    }

    /// Mark a transport gap — evidence may be incomplete until the
    /// next epoch.
    pub fn mark_transport_gap(&self) {
        if let Ok(mut cov) = self.coverage.lock() {
            *cov = CoverageState::TransportGap;
        }
    }

    fn mint_root_id(&self) -> RootId {
        RootId::new(self.next_root_id.fetch_add(1, Ordering::Relaxed))
    }

    /// Admit a root and mint an opaque RootId.
    ///
    /// Returns the minted RootId and the verdict. Idempotent: re-admitting
    /// the same message returns the existing RootId with an Admitted verdict.
    pub fn admit_root(&self, root: ActivationRoot) -> (Option<RootId>, RootVerdict) {
        let message = root.message_ref();
        let channel = root.channel_ref();
        let epoch = self.current_epoch();

        let Ok(mut by_message) = self.roots_by_message.lock() else {
            return (None, RootVerdict::Unavailable);
        };

        if let Some(existing) = by_message.get(&message) {
            if existing.root.channel_ref() == channel {
                return (
                    Some(existing.root_id),
                    RootVerdict::Admitted {
                        root_id: existing.root_id,
                        channel,
                    },
                );
            }
            return (
                Some(existing.root_id),
                RootVerdict::ChannelMismatch {
                    root_id: existing.root_id,
                    admitted_channel: existing.root.channel_ref(),
                    claimed_channel: channel,
                },
            );
        }

        if by_message.len() >= self.max_roots {
            let now = Instant::now();
            by_message.retain(|_, r| r.expires > now);
            if by_message.len() >= self.max_roots {
                return (None, RootVerdict::Evicted);
            }
        }

        let root_id = self.mint_root_id();
        let now = Instant::now();
        let record = RootRecord {
            root_id,
            root,
            epoch,
            admitted_at: now,
            expires: now + self.ttl,
        };

        by_message.insert(message, record);

        if let Ok(mut by_id) = self.roots_by_id.lock() {
            by_id.insert(root_id, message);
        }

        (Some(root_id), RootVerdict::Admitted { root_id, channel })
    }

    /// Verify whether a message was admitted, with full typed verdict.
    pub fn verify(&self, message: MessageRef, claimed_channel: ChannelRef) -> RootVerdict {
        let Ok(by_message) = self.roots_by_message.lock() else {
            return RootVerdict::Unavailable;
        };

        let epoch_start = self
            .epoch_start
            .lock()
            .map(|s| *s)
            .unwrap_or_else(|e| *e.into_inner());

        match by_message.get(&message) {
            None => {
                let coverage = self.coverage();
                match coverage {
                    CoverageState::Complete => RootVerdict::UnknownComplete,
                    CoverageState::TransportGap => RootVerdict::TransportGap,
                }
            }
            Some(record) => {
                let now = Instant::now();

                if now >= record.expires {
                    return RootVerdict::Expired;
                }

                if record.admitted_at < epoch_start && record.epoch != self.current_epoch() {
                    return RootVerdict::RestartGap;
                }

                if record.root.channel_ref() == claimed_channel {
                    RootVerdict::Admitted {
                        root_id: record.root_id,
                        channel: record.channel_ref(),
                    }
                } else {
                    RootVerdict::ChannelMismatch {
                        root_id: record.root_id,
                        admitted_channel: record.root.channel_ref(),
                        claimed_channel,
                    }
                }
            }
        }
    }

    /// Look up a root by its RootId.
    pub fn get_root(&self, root_id: RootId) -> Option<MessageRef> {
        self.roots_by_id
            .lock()
            .ok()
            .and_then(|by_id| by_id.get(&root_id).copied())
    }

    /// Number of roots currently tracked.
    pub fn len(&self) -> usize {
        self.roots_by_message.lock().map(|m| m.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Remove expired entries.
    pub fn gc_expired(&self) {
        let now = Instant::now();
        let expired_messages: Vec<MessageRef>;

        {
            let Ok(mut by_message) = self.roots_by_message.lock() else {
                return;
            };
            expired_messages = by_message
                .iter()
                .filter(|(_, r)| r.expires <= now)
                .map(|(m, _)| *m)
                .collect();
            for msg in &expired_messages {
                if let Some(record) = by_message.remove(msg)
                    && let Ok(mut by_id) = self.roots_by_id.lock()
                {
                    by_id.remove(&record.root_id);
                }
            }
        }
    }
}

impl RootRecord {
    fn channel_ref(&self) -> ChannelRef {
        self.root.channel_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::receipt::DiscordIngressReceipt;
    use crate::{ContentHash, PrincipalRef};
    use sha2::{Digest as _, Sha256};

    const CH_A: ChannelRef = ChannelRef::new(100);
    const CH_B: ChannelRef = ChannelRef::new(200);
    const USER: PrincipalRef = PrincipalRef::new(437002871280631808);

    fn hash(s: &str) -> ContentHash {
        ContentHash(Sha256::digest(s.as_bytes()).into())
    }

    fn discord_root(msg_id: u64, channel: ChannelRef, content: &str) -> ActivationRoot {
        ActivationRoot::Discord(DiscordIngressReceipt {
            message: MessageRef::new(msg_id),
            channel,
            principal: USER,
            content_hash: hash(content),
        })
    }

    #[test]
    fn admit_and_verify() {
        let reg = RootRegistry::new();
        let root = discord_root(1000, CH_A, "hello");

        let (root_id, verdict) = reg.admit_root(root);
        assert!(root_id.is_some());
        assert!(verdict.is_admitted());

        let verify = reg.verify(MessageRef::new(1000), CH_A);
        assert!(verify.is_admitted());
    }

    #[test]
    fn unknown_message_with_complete_coverage() {
        let reg = RootRegistry::new();
        let verdict = reg.verify(MessageRef::new(9999), CH_A);
        assert_eq!(verdict, RootVerdict::UnknownComplete);
        assert!(verdict.is_denial());
    }

    #[test]
    fn unknown_message_with_transport_gap() {
        let reg = RootRegistry::new();
        reg.mark_transport_gap();
        let verdict = reg.verify(MessageRef::new(9999), CH_A);
        assert_eq!(verdict, RootVerdict::TransportGap);
        assert!(verdict.is_degraded());
    }

    #[test]
    fn channel_mismatch_on_verify() {
        let reg = RootRegistry::new();
        reg.admit_root(discord_root(1000, CH_A, "hello"));

        let verdict = reg.verify(MessageRef::new(1000), CH_B);
        assert!(verdict.is_denial());
        assert!(matches!(verdict, RootVerdict::ChannelMismatch { .. }));
    }

    #[test]
    fn idempotent_readmission() {
        let reg = RootRegistry::new();
        let (id1, _) = reg.admit_root(discord_root(1000, CH_A, "hello"));
        let (id2, _) = reg.admit_root(discord_root(1000, CH_A, "hello"));
        assert_eq!(id1, id2);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn expired_entry() {
        let reg = RootRegistry::with_ttl(Duration::from_millis(0));
        reg.admit_root(discord_root(1000, CH_A, "hello"));
        std::thread::sleep(Duration::from_millis(1));

        let verdict = reg.verify(MessageRef::new(1000), CH_A);
        assert_eq!(verdict, RootVerdict::Expired);
    }

    #[test]
    fn capacity_eviction() {
        let reg = RootRegistry::with_capacity(2);
        reg.admit_root(discord_root(1, CH_A, "a"));
        reg.admit_root(discord_root(2, CH_A, "b"));

        let (root_id, verdict) = reg.admit_root(discord_root(3, CH_A, "c"));
        assert!(root_id.is_none());
        assert_eq!(verdict, RootVerdict::Evicted);
    }

    #[test]
    fn new_epoch_advances() {
        let reg = RootRegistry::new();
        let e1 = reg.current_epoch();
        let e2 = reg.new_epoch();
        assert_ne!(e1, e2);
        assert_eq!(reg.current_epoch(), e2);
    }

    #[test]
    fn root_id_lookup() {
        let reg = RootRegistry::new();
        let (root_id, _) = reg.admit_root(discord_root(1000, CH_A, "hello"));
        let root_id = root_id.unwrap();

        let msg = reg.get_root(root_id);
        assert_eq!(msg, Some(MessageRef::new(1000)));
    }

    #[test]
    fn gc_removes_expired_from_both_maps() {
        let reg = RootRegistry::with_ttl(Duration::from_millis(0));
        let (root_id, _) = reg.admit_root(discord_root(1, CH_A, "a"));
        let root_id = root_id.unwrap();
        std::thread::sleep(Duration::from_millis(1));

        reg.gc_expired();
        assert_eq!(reg.len(), 0);
        assert!(reg.get_root(root_id).is_none());
    }

    #[test]
    fn transport_gap_resets_on_new_epoch() {
        let reg = RootRegistry::new();
        reg.mark_transport_gap();
        assert_eq!(reg.coverage(), CoverageState::TransportGap);

        reg.new_epoch();
        assert_eq!(reg.coverage(), CoverageState::Complete);
    }

    #[test]
    fn root_ids_are_monotonically_unique() {
        let reg = RootRegistry::new();
        let (id1, _) = reg.admit_root(discord_root(1, CH_A, "a"));
        let (id2, _) = reg.admit_root(discord_root(2, CH_A, "b"));
        let (id3, _) = reg.admit_root(discord_root(3, CH_A, "c"));
        assert_ne!(id1, id2);
        assert_ne!(id2, id3);
    }

    #[test]
    fn verdict_classification() {
        assert!(RootVerdict::PreEpoch.is_degraded());
        assert!(RootVerdict::RestartGap.is_degraded());
        assert!(RootVerdict::TransportGap.is_degraded());
        assert!(RootVerdict::Unavailable.is_degraded());

        assert!(RootVerdict::UnknownComplete.is_denial());

        assert!(RootVerdict::Expired.is_degraded() == false);
        assert!(RootVerdict::Evicted.is_degraded() == false);
    }
}
