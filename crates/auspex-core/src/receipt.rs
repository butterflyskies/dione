use crate::{ChannelRef, ContentHash, MessageRef, PrincipalRef};

/// Canonical receipt from a Discord gateway delivery.
#[derive(Debug, Clone)]
pub struct DiscordIngressReceipt {
    pub message: MessageRef,
    pub channel: ChannelRef,
    pub principal: PrincipalRef,
    pub content_hash: ContentHash,
}

/// The root cause of an activation.
///
/// Four ratified kinds; only Discord is enabled in Slice A.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum ActivationRoot {
    Discord(DiscordIngressReceipt),
    // Future: Console, Cron, Recovery
}

impl ActivationRoot {
    pub fn message_ref(&self) -> MessageRef {
        match self {
            Self::Discord(r) => r.message,
        }
    }

    pub fn channel_ref(&self) -> ChannelRef {
        match self {
            Self::Discord(r) => r.channel,
        }
    }
}
