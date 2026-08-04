//! Dione adapter for auspex-core's ingress ledger.
//!
//! Converts Discord types (serenity MessageId/ChannelId/UserId) to
//! transport-agnostic auspex-core domain types and delegates all
//! state and verification logic to `auspex_core::ingress_ledger`.

use std::time::Instant;

use serenity::model::id::{ChannelId, MessageId, UserId};

use auspex_core::ingress_ledger as core;
use auspex_core::{ChannelRef, MessageRef, PrincipalRef};

/// Re-export the core verify result for callers that match on it.
pub use auspex_core::ingress_ledger::VerifyResult;

/// Dione-side ingress ledger wrapper.
///
/// Accepts Discord types, converts to auspex-core domain types,
/// and delegates to the transport-agnostic kernel.
pub struct IngressLedger {
    inner: core::IngressLedger,
}

impl Default for IngressLedger {
    fn default() -> Self {
        Self {
            inner: core::IngressLedger::new(),
        }
    }
}

impl IngressLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a Discord message was admitted by the dione gateway.
    pub fn note_admitted(
        &self,
        message_id: MessageId,
        channel_id: ChannelId,
        user_id: UserId,
        content: &str,
    ) {
        let content_hash = core::IngressLedger::hash_content(content);
        self.inner.note_admitted(
            MessageRef(message_id.get()),
            ChannelRef(channel_id.get()),
            PrincipalRef(user_id.get()),
            content_hash,
        );
    }

    /// Verify a Discord message was admitted, with channel binding.
    pub fn verify(&self, message_id: MessageId, claimed_channel: ChannelId) -> VerifyResult {
        self.inner.verify(
            MessageRef(message_id.get()),
            ChannelRef(claimed_channel.get()),
        )
    }

    /// The instant this ledger was created.
    pub fn epoch(&self) -> Instant {
        self.inner.epoch()
    }

    /// Remove expired entries.
    pub fn gc_expired(&self) {
        self.inner.gc_expired();
    }

    /// Access the observed verifications (test-only).
    #[cfg(test)]
    pub fn take_observed_verifications(&self) -> Vec<VerifyResult> {
        self.inner.take_observed_verifications()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adapter_admits_and_verifies_through_discord_types() {
        let ledger = IngressLedger::new();

        ledger.note_admitted(
            MessageId::new(42),
            ChannelId::new(100),
            UserId::new(999),
            "hello world",
        );

        assert_eq!(
            ledger.verify(MessageId::new(42), ChannelId::new(100)),
            VerifyResult::Admitted {
                channel: ChannelRef(100),
            }
        );
        assert_eq!(
            ledger.verify(MessageId::new(42), ChannelId::new(200)),
            VerifyResult::ChannelMismatch {
                admitted_channel: ChannelRef(100),
                claimed_channel: ChannelRef(200),
            }
        );
        assert_eq!(
            ledger.verify(MessageId::new(9999), ChannelId::new(100)),
            VerifyResult::Unknown,
        );
    }
}
