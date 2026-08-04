use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serenity::model::id::{ChannelId, MessageId, UserId};
use sha2::{Digest as _, Sha256};

const DEFAULT_TTL: Duration = Duration::from_secs(7200);
const DEFAULT_MAX_ENTRIES: usize = 16_384;

/// Evidence that a Discord message was admitted by the dione gateway.
///
/// This proves the gateway admitted the message. It does NOT prove the message
/// reached the notification stream or was processed by the LLM — those are
/// separate transitions this tracer bullet does not yet instrument.
#[derive(Debug, Clone)]
pub struct AdmittedRecord {
    pub channel_id: ChannelId,
    pub user_id: UserId,
    pub content_sha256: [u8; 32],
    #[expect(dead_code, reason = "retained for future audit/diagnostics use")]
    admitted_at: Instant,
    expires: Instant,
}

/// Result of verifying a message ID against the ingress ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifyResult {
    /// The message was admitted by the gateway from this channel.
    Admitted { channel_id: ChannelId },
    /// No admission evidence for this message ID is retained in this process.
    ///
    /// The message may never have been admitted, or its evidence may have been
    /// lost across restart, garbage collection, or capacity pressure.
    Unknown,
    /// The message was admitted but the ledger entry has expired.
    Expired,
    /// The message was admitted, but from a different channel than claimed.
    ChannelMismatch {
        admitted_channel: ChannelId,
        claimed_channel: ChannelId,
    },
    /// The ledger store is unavailable (poisoned mutex).
    Unavailable,
}

/// In-memory ledger of messages admitted by the dione gateway.
///
/// Thread-safe. Entries expire after TTL and are lazily cleaned on capacity pressure.
pub struct IngressLedger {
    entries: Mutex<HashMap<MessageId, AdmittedRecord>>,
    ttl: Duration,
    max_entries: usize,
    #[cfg(test)]
    observed_verifications: Mutex<Vec<VerifyResult>>,
}

impl Default for IngressLedger {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl: DEFAULT_TTL,
            max_entries: DEFAULT_MAX_ENTRIES,
            #[cfg(test)]
            observed_verifications: Mutex::new(Vec::new()),
        }
    }
}

impl IngressLedger {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_capacity(max_entries: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl: DEFAULT_TTL,
            max_entries,
            observed_verifications: Mutex::new(Vec::new()),
        }
    }

    #[cfg(test)]
    fn with_ttl(ttl: Duration) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            ttl,
            max_entries: DEFAULT_MAX_ENTRIES,
            observed_verifications: Mutex::new(Vec::new()),
        }
    }

    /// Record that a message was admitted by the dione gateway.
    ///
    /// Called in the discord event handler after a real message passes the
    /// gateway's inbound gate and before notification delivery is attempted.
    /// The content hash distinguishes conflicting duplicate admissions.
    ///
    /// Identical replays (same message ID, channel, user, and content) are
    /// idempotent. Conflicting evidence for an existing message ID preserves
    /// the first record and logs a warning.
    pub fn note_admitted(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        user_id: UserId,
        content: &str,
    ) {
        let now = Instant::now();
        let content_sha256 = Sha256::digest(content.as_bytes()).into();

        let Ok(mut entries) = self.entries.lock() else {
            tracing::warn!("ingress ledger unavailable (poisoned)");
            return;
        };

        if let Some(existing) = entries.get(&message_id) {
            if existing.channel_id == channel_id
                && existing.user_id == user_id
                && existing.content_sha256 == content_sha256
            {
                return;
            }
            tracing::warn!(
                message_id = message_id.get(),
                existing_channel = existing.channel_id.get(),
                new_channel = channel_id.get(),
                existing_user = existing.user_id.get(),
                new_user = user_id.get(),
                channel_changed = existing.channel_id != channel_id,
                user_changed = existing.user_id != user_id,
                content_changed = existing.content_sha256 != content_sha256,
                "ingress ledger: conflicting evidence for message_id; preserving first"
            );
            return;
        }

        if entries.len() >= self.max_entries {
            let cleanup_now = Instant::now();
            entries.retain(|_, r| r.expires > cleanup_now);
            if entries.len() >= self.max_entries {
                tracing::warn!(
                    capacity = self.max_entries,
                    dropped_message_id = message_id.get(),
                    "ingress ledger at capacity after cleanup; dropping new entry"
                );
                return;
            }
        }

        let record = AdmittedRecord {
            channel_id,
            user_id,
            content_sha256,
            admitted_at: now,
            expires: now + self.ttl,
        };
        entries.insert(message_id, record);
    }

    /// Check whether a message ID was admitted by the gateway.
    ///
    /// Returns the full record if found and not expired, None otherwise.
    pub fn get(&self, message_id: MessageId) -> Option<AdmittedRecord> {
        let entries = self.entries.lock().ok()?;
        let record = entries.get(&message_id)?;
        if Instant::now() >= record.expires {
            return None;
        }
        Some(record.clone())
    }

    /// Verify a message ID was admitted, with channel binding.
    ///
    /// This is the egress-side check: when the model references a message_id
    /// in a particular channel, verify both that the message was real AND that
    /// it came from the expected channel.
    pub fn verify(&self, message_id: MessageId, claimed_channel: ChannelId) -> VerifyResult {
        let result = match self.entries.lock() {
            Ok(entries) => match entries.get(&message_id) {
                None => VerifyResult::Unknown,
                Some(record) if Instant::now() >= record.expires => VerifyResult::Expired,
                Some(record) if record.channel_id == claimed_channel => VerifyResult::Admitted {
                    channel_id: record.channel_id,
                },
                Some(record) => VerifyResult::ChannelMismatch {
                    admitted_channel: record.channel_id,
                    claimed_channel,
                },
            },
            Err(_) => VerifyResult::Unavailable,
        };
        #[cfg(test)]
        if let Ok(mut observations) = self.observed_verifications.lock() {
            observations.push(result.clone());
        }
        result
    }

    #[cfg(test)]
    pub(crate) fn take_observed_verifications(&self) -> Vec<VerifyResult> {
        self.observed_verifications
            .lock()
            .map(|mut observations| std::mem::take(&mut *observations))
            .unwrap_or_default()
    }

    /// Remove expired entries.
    pub fn gc_expired(&self) {
        if let Ok(mut entries) = self.entries.lock() {
            let now = Instant::now();
            entries.retain(|_, r| r.expires > now);
        }
    }

    /// Number of entries currently in the ledger (for diagnostics).
    #[cfg(test)]
    fn len(&self) -> usize {
        self.entries.lock().map(|e| e.len()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const CHANNEL_A: ChannelId = ChannelId::new(100);
    const CHANNEL_B: ChannelId = ChannelId::new(200);
    const USER: UserId = UserId::new(437002871280631808);

    #[test]
    fn admitted_message_is_verifiable() {
        let ledger = IngressLedger::new();
        let msg_id = MessageId::new(1000);

        ledger.note_admitted(msg_id, CHANNEL_A, USER, "hello world");

        assert_eq!(
            ledger.verify(msg_id, CHANNEL_A),
            VerifyResult::Admitted {
                channel_id: CHANNEL_A
            }
        );
    }

    #[test]
    fn unknown_message_returns_unknown() {
        let ledger = IngressLedger::new();
        let msg_id = MessageId::new(9999);

        assert_eq!(ledger.verify(msg_id, CHANNEL_A), VerifyResult::Unknown);
    }

    #[test]
    fn channel_mismatch_detected() {
        let ledger = IngressLedger::new();
        let msg_id = MessageId::new(1000);

        ledger.note_admitted(msg_id, CHANNEL_A, USER, "hello");

        assert_eq!(
            ledger.verify(msg_id, CHANNEL_B),
            VerifyResult::ChannelMismatch {
                admitted_channel: CHANNEL_A,
                claimed_channel: CHANNEL_B,
            }
        );
    }

    #[test]
    fn get_returns_record_with_content_hash() {
        let ledger = IngressLedger::new();
        let msg_id = MessageId::new(1000);
        let content = "test content";

        ledger.note_admitted(msg_id, CHANNEL_A, USER, content);

        let record = ledger.get(msg_id).expect("should be present");
        assert_eq!(record.channel_id, CHANNEL_A);
        assert_eq!(record.user_id, USER);
        assert_eq!(
            record.content_sha256,
            <[u8; 32]>::from(Sha256::digest(content.as_bytes()))
        );
    }

    #[test]
    fn expired_entry_returns_expired() {
        let ledger = IngressLedger::with_ttl(Duration::from_millis(0));
        let msg_id = MessageId::new(1000);

        ledger.note_admitted(msg_id, CHANNEL_A, USER, "hello");

        // TTL is 0ms, so it expires immediately.
        std::thread::sleep(Duration::from_millis(1));
        assert_eq!(ledger.verify(msg_id, CHANNEL_A), VerifyResult::Expired);
    }

    #[test]
    fn capacity_triggers_cleanup() {
        let ledger = IngressLedger::with_capacity(2);

        ledger.note_admitted(MessageId::new(1), CHANNEL_A, USER, "a");
        ledger.note_admitted(MessageId::new(2), CHANNEL_A, USER, "b");
        assert_eq!(ledger.len(), 2);

        // Third entry triggers cleanup; since none are expired, it's dropped.
        ledger.note_admitted(MessageId::new(3), CHANNEL_A, USER, "c");
        assert_eq!(ledger.len(), 2);
    }

    #[test]
    fn gc_removes_expired() {
        let ledger = IngressLedger::with_ttl(Duration::from_millis(0));

        ledger.note_admitted(MessageId::new(1), CHANNEL_A, USER, "a");
        std::thread::sleep(Duration::from_millis(1));

        ledger.gc_expired();
        assert_eq!(ledger.len(), 0);
    }

    #[test]
    fn identical_replay_is_idempotent() {
        let ledger = IngressLedger::new();
        let msg_id = MessageId::new(1000);

        ledger.note_admitted(msg_id, CHANNEL_A, USER, "hello");
        ledger.note_admitted(msg_id, CHANNEL_A, USER, "hello");

        assert_eq!(
            ledger.verify(msg_id, CHANNEL_A),
            VerifyResult::Admitted {
                channel_id: CHANNEL_A
            }
        );
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn conflicting_evidence_preserves_first() {
        let ledger = IngressLedger::new();
        let msg_id = MessageId::new(1000);

        ledger.note_admitted(msg_id, CHANNEL_A, USER, "first");
        ledger.note_admitted(msg_id, CHANNEL_B, USER, "second");

        assert_eq!(
            ledger.verify(msg_id, CHANNEL_A),
            VerifyResult::Admitted {
                channel_id: CHANNEL_A
            }
        );
        assert_eq!(
            ledger.verify(msg_id, CHANNEL_B),
            VerifyResult::ChannelMismatch {
                admitted_channel: CHANNEL_A,
                claimed_channel: CHANNEL_B,
            }
        );
    }

    #[test]
    fn content_only_conflict_preserves_first_digest() {
        let ledger = IngressLedger::new();
        let msg_id = MessageId::new(1000);

        ledger.note_admitted(msg_id, CHANNEL_A, USER, "first");
        let first_digest = ledger.get(msg_id).expect("first record").content_sha256;
        ledger.note_admitted(msg_id, CHANNEL_A, USER, "changed");

        let preserved = ledger.get(msg_id).expect("preserved record");
        assert_eq!(preserved.content_sha256, first_digest);
        assert_eq!(
            preserved.content_sha256,
            <[u8; 32]>::from(Sha256::digest(b"first"))
        );
        assert_ne!(
            preserved.content_sha256,
            <[u8; 32]>::from(Sha256::digest(b"changed"))
        );
        assert_eq!(ledger.len(), 1);
    }

    #[test]
    fn poisoned_lock_returns_unavailable() {
        let ledger = std::sync::Arc::new(IngressLedger::new());
        ledger.note_admitted(MessageId::new(1), CHANNEL_A, USER, "hello");

        let poison_target = std::sync::Arc::clone(&ledger);
        let _ = std::thread::spawn(move || {
            let _guard = poison_target.entries.lock().unwrap();
            panic!("poison the mutex");
        })
        .join();

        assert_eq!(
            ledger.verify(MessageId::new(1), CHANNEL_A),
            VerifyResult::Unavailable
        );
    }

    #[test]
    fn multiple_messages_independently_tracked() {
        let ledger = IngressLedger::new();

        ledger.note_admitted(MessageId::new(1), CHANNEL_A, USER, "hello");
        ledger.note_admitted(MessageId::new(2), CHANNEL_B, USER, "world");

        assert_eq!(
            ledger.verify(MessageId::new(1), CHANNEL_A),
            VerifyResult::Admitted {
                channel_id: CHANNEL_A
            }
        );
        assert_eq!(
            ledger.verify(MessageId::new(2), CHANNEL_B),
            VerifyResult::Admitted {
                channel_id: CHANNEL_B
            }
        );
        assert_eq!(
            ledger.verify(MessageId::new(3), CHANNEL_A),
            VerifyResult::Unknown
        );
    }
}
