use crate::{ChannelRef, ContentHash, MessageRef, PrincipalRef};
use sha2::{Digest as _, Sha256};
use std::{
    collections::HashMap,
    sync::Mutex,
    time::{Duration, Instant},
};

const DEFAULT_TTL: Duration = Duration::from_secs(7200);
const DEFAULT_MAX_ENTRIES: usize = 16_384;

/// Evidence that a message was admitted by a gateway adapter.
///
/// This proves the gateway received and forwarded the message. It does NOT
/// prove the message reached the notification stream or was processed by
/// the LLM — those are separate transitions.
#[derive(Debug, Clone)]
pub struct AdmittedRecord {
    pub channel: ChannelRef,
    pub principal: PrincipalRef,
    pub content_hash: ContentHash,
    #[expect(dead_code, reason = "retained for future audit/diagnostics use")]
    admitted_at: Instant,
    expires: Instant,
}

/// Result of verifying a message against the ingress ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyResult {
    /// The message was admitted by the gateway from this channel.
    Admitted { channel: ChannelRef },
    /// No admission evidence for this message is retained.
    Unknown,
    /// The message was admitted but the ledger entry has expired.
    Expired,
    /// The message was admitted, but from a different channel than claimed.
    ChannelMismatch {
        admitted_channel: ChannelRef,
        claimed_channel: ChannelRef,
    },
    /// The ledger store is unavailable (poisoned mutex).
    Unavailable,
}

/// In-memory ledger of messages admitted by a gateway adapter.
///
/// Thread-safe. Entries expire after TTL and are lazily cleaned on capacity
/// pressure. The epoch is the instant the ledger was created — messages
/// predating it were never eligible for admission.
pub struct IngressLedger {
    entries: Mutex<HashMap<MessageRef, AdmittedRecord>>,
    ttl: Duration,
    max_entries: usize,
    epoch: Instant,
    #[cfg(any(test, feature = "test-support"))]
    observed_verifications: Mutex<Vec<VerifyResult>>,
}

impl Default for IngressLedger {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl: DEFAULT_TTL,
            max_entries: DEFAULT_MAX_ENTRIES,
            epoch: Instant::now(),
            #[cfg(any(test, feature = "test-support"))]
            observed_verifications: Mutex::new(Vec::new()),
        }
    }
}

impl IngressLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// The instant this ledger was created. Messages predating this epoch
    /// were never eligible for admission.
    pub fn epoch(&self) -> Instant {
        self.epoch
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    fn with_capacity(max_entries: usize) -> Self {
        Self {
            max_entries,
            ..Self::default()
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    fn with_ttl(ttl: Duration) -> Self {
        Self {
            ttl,
            ..Self::default()
        }
    }

    /// Compute a content hash from raw message content.
    pub fn hash_content(content: &str) -> ContentHash {
        ContentHash(Sha256::digest(content.as_bytes()).into())
    }

    /// Record that a message was admitted by the gateway.
    ///
    /// Identical replays (same message, channel, principal, and content) are
    /// idempotent. Conflicting evidence preserves the first record.
    ///
    /// Returns `true` if the record was inserted, `false` if idempotent or
    /// conflicting.
    pub fn note_admitted(
        &self,
        message: MessageRef,
        channel: ChannelRef,
        principal: PrincipalRef,
        content_hash: ContentHash,
    ) -> bool {
        let now = Instant::now();

        let Ok(mut entries) = self.entries.lock() else {
            return false;
        };

        if let Some(existing) = entries.get(&message) {
            if existing.channel == channel
                && existing.principal == principal
                && existing.content_hash == content_hash
            {
                return false;
            }
            return false;
        }

        if entries.len() >= self.max_entries {
            let cleanup_now = Instant::now();
            entries.retain(|_, r| r.expires > cleanup_now);
            if entries.len() >= self.max_entries {
                return false;
            }
        }

        let record = AdmittedRecord {
            channel,
            principal,
            content_hash,
            admitted_at: now,
            expires: now + self.ttl,
        };
        entries.insert(message, record);
        true
    }

    /// Verify a message was admitted, with channel binding.
    pub fn verify(&self, message: MessageRef, claimed_channel: ChannelRef) -> VerifyResult {
        let result = match self.entries.lock() {
            Ok(entries) => match entries.get(&message) {
                None => VerifyResult::Unknown,
                Some(record) if Instant::now() >= record.expires => VerifyResult::Expired,
                Some(record) if record.channel == claimed_channel => VerifyResult::Admitted {
                    channel: record.channel,
                },
                Some(record) => VerifyResult::ChannelMismatch {
                    admitted_channel: record.channel,
                    claimed_channel,
                },
            },
            Err(_) => VerifyResult::Unavailable,
        };
        #[cfg(any(test, feature = "test-support"))]
        if let Ok(mut observations) = self.observed_verifications.lock() {
            observations.push(result.clone());
        }
        result
    }

    /// Remove expired entries.
    pub fn gc_expired(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            let now = Instant::now();
            entries.retain(|_, r| r.expires > now);
        }
    }

    #[cfg(any(test, feature = "test-support"))]
    pub fn take_observed_verifications(&self) -> Vec<VerifyResult> {
        self.observed_verifications
            .lock()
            .map(|mut observations| std::mem::take(&mut *observations))
            .unwrap_or_default()
    }

    #[cfg(any(test, feature = "test-support"))]
    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CH_A: ChannelRef = ChannelRef::new(100);
    const CH_B: ChannelRef = ChannelRef::new(200);
    const USER: PrincipalRef = PrincipalRef::new(437002871280631808);

    fn hash(s: &str) -> ContentHash {
        IngressLedger::hash_content(s)
    }

    #[test]
    fn admitted_message_is_verifiable() {
        let ledger = IngressLedger::new();
        let msg = MessageRef::discord(1000);

        ledger.note_admitted(msg, CH_A, USER, hash("hello world"));

        assert_eq!(
            ledger.verify(msg, CH_A),
            VerifyResult::Admitted { channel: CH_A }
        );
    }

    #[test]
    fn unknown_message_returns_unknown() {
        let ledger = IngressLedger::new();
        assert_eq!(
            ledger.verify(MessageRef::discord(9999), CH_A),
            VerifyResult::Unknown
        );
    }

    #[test]
    fn channel_mismatch_detected() {
        let ledger = IngressLedger::new();
        let msg = MessageRef::discord(1000);

        ledger.note_admitted(msg, CH_A, USER, hash("hello"));

        assert_eq!(
            ledger.verify(msg, CH_B),
            VerifyResult::ChannelMismatch {
                admitted_channel: CH_A,
                claimed_channel: CH_B,
            }
        );
    }

    #[test]
    fn identical_replay_is_idempotent() {
        let ledger = IngressLedger::new();
        let msg = MessageRef::discord(1000);
        let h = hash("hello");

        assert!(ledger.note_admitted(msg, CH_A, USER, h));
        assert!(!ledger.note_admitted(msg, CH_A, USER, h));
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn conflicting_evidence_preserves_first() {
        let ledger = IngressLedger::new();
        let msg = MessageRef::discord(1000);

        ledger.note_admitted(msg, CH_A, USER, hash("first"));
        ledger.note_admitted(msg, CH_B, USER, hash("second"));

        assert_eq!(
            ledger.verify(msg, CH_A),
            VerifyResult::Admitted { channel: CH_A }
        );
    }

    #[test]
    fn expired_entry_returns_expired() {
        let ledger = IngressLedger::with_ttl(Duration::from_millis(0));
        let msg = MessageRef::discord(1000);

        ledger.note_admitted(msg, CH_A, USER, hash("hello"));
        std::thread::sleep(Duration::from_millis(1));

        assert_eq!(ledger.verify(msg, CH_A), VerifyResult::Expired);
    }

    #[test]
    fn capacity_drops_new_entry() {
        let ledger = IngressLedger::with_capacity(2);

        ledger.note_admitted(MessageRef::discord(1), CH_A, USER, hash("a"));
        ledger.note_admitted(MessageRef::discord(2), CH_A, USER, hash("b"));
        assert_eq!(ledger.len(), 2);

        assert!(!ledger.note_admitted(MessageRef::discord(3), CH_A, USER, hash("c")));
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn gc_removes_expired() {
        let ledger = IngressLedger::with_ttl(Duration::from_millis(0));

        ledger.note_admitted(MessageRef::discord(1), CH_A, USER, hash("a"));
        std::thread::sleep(Duration::from_millis(1));

        ledger.gc_expired();
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn poisoned_lock_returns_unavailable() {
        let ledger = std::sync::Arc::new(IngressLedger::new());
        ledger.note_admitted(MessageRef::discord(1), CH_A, USER, hash("hello"));

        let poison_target = std::sync::Arc::clone(&ledger);
        let _ = std::thread::spawn(move || {
            let _guard = poison_target.entries.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();

        assert_eq!(
            ledger.verify(MessageRef::discord(1), CH_A),
            VerifyResult::Unavailable
        );
    }

    #[test]
    fn epoch_is_recorded() {
        let before = Instant::now();
        let ledger = IngressLedger::new();
        let after = Instant::now();

        assert!(ledger.epoch() >= before);
        assert!(ledger.epoch() <= after);
    }
}
