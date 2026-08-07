use crate::{ChannelRef, RootId};

/// Typed evidence verdict from the root registry.
///
/// Nine variants, no booleans, no null-as-unknown. The gap/degraded
/// distinction matters: "I don't have evidence" is not "this is fake."
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RootVerdict {
    /// The message was admitted by the gateway and a root was minted.
    Admitted {
        root_id: RootId,
        channel: ChannelRef,
    },
    /// The message predates the current registry epoch — it was never
    /// eligible for admission in this process lifetime.
    PreEpoch,
    /// The message was admitted but the registry entry has expired.
    Expired,
    /// The message was admitted but evicted under capacity pressure.
    Evicted,
    /// Evidence is missing across a restart boundary — the registry
    /// was recreated and prior admissions were lost.
    RestartGap,
    /// Evidence is missing due to a transport gap (dione reconnection,
    /// message queue drain, etc.).
    TransportGap,
    /// The message was admitted but from a different channel than claimed.
    ChannelMismatch {
        root_id: RootId,
        admitted_channel: ChannelRef,
        claimed_channel: ChannelRef,
    },
    /// The message is not in the registry and coverage is complete —
    /// the registry was running, healthy, and did not admit it.
    UnknownComplete,
    /// The registry store is unavailable (poisoned mutex, etc.).
    Unavailable,
}

impl RootVerdict {
    pub fn is_admitted(&self) -> bool {
        matches!(self, Self::Admitted { .. })
    }

    pub fn is_degraded(&self) -> bool {
        matches!(
            self,
            Self::PreEpoch | Self::RestartGap | Self::TransportGap | Self::Unavailable
        )
    }

    pub fn is_denial(&self) -> bool {
        matches!(self, Self::UnknownComplete | Self::ChannelMismatch { .. })
    }
}
