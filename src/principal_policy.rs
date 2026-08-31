//! Typed admission and attention policy primitives.
//!
//! This module first captures the exact behavior of Dione's legacy
//! `allow_from`, `dm_policy`, and `require_mention` configuration. Runtime
//! integration remains in [`crate::gate`] until the typed policy cutover is
//! complete.

use crate::config::DmPolicy;

/// Whether a principal may enter a receiver's context.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Admission {
    /// Admit the principal's message.
    Admit,
    /// Hold the message as an access request.
    Request,
    /// Reject the message.
    Reject,
}

/// Whether admitted traffic may wake the receiver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Attention {
    /// Deliver admitted traffic without requiring a mention.
    Normal,
    /// Deliver admitted traffic only when the receiver is mentioned.
    MentionOnly,
    /// Do not push or wake on admitted traffic.
    Quiet,
}

/// The typed result of translating a legacy access rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyPolicyTranslation {
    /// The translated admission decision.
    pub admission: Admission,
    /// The translated attention policy.
    pub attention: Attention,
}

/// Legacy guild-channel inputs for one direct sender.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LegacyGuildPolicyInput {
    /// Whether the channel has a configured legacy policy.
    pub channel_is_configured: bool,
    /// Whether the legacy policy requires a mention for attention.
    pub require_mention: bool,
    /// Whether the legacy policy restricts admitted sender identities.
    pub identity_filter_is_active: bool,
    /// Whether the direct sender passes the legacy identity filter.
    pub sender_is_allowed: bool,
}

impl LegacyPolicyTranslation {
    /// Translate legacy DM configuration for one direct sender.
    pub fn dm(dm_policy: DmPolicy, sender_is_allowed: bool) -> Self {
        let admission = match (dm_policy, sender_is_allowed) {
            (DmPolicy::Disabled, _) => Admission::Reject,
            (_, true) => Admission::Admit,
            (DmPolicy::Queue, false) => Admission::Request,
            (DmPolicy::Drop, false) => Admission::Reject,
        };
        Self {
            admission,
            attention: Attention::Normal,
        }
    }

    /// Translate legacy guild-channel configuration for one direct sender.
    pub fn guild(input: LegacyGuildPolicyInput) -> Self {
        let admission = if !input.channel_is_configured
            || (input.identity_filter_is_active && !input.sender_is_allowed)
        {
            Admission::Reject
        } else {
            Admission::Admit
        };
        let attention = if input.require_mention {
            Attention::MentionOnly
        } else {
            Attention::Normal
        };
        Self {
            admission,
            attention,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guild_legacy_controls_translate_independently_and_combine() {
        let translate = |require_mention, identity_filter_is_active| {
            LegacyPolicyTranslation::guild(LegacyGuildPolicyInput {
                channel_is_configured: true,
                require_mention,
                identity_filter_is_active,
                sender_is_allowed: false,
            })
        };
        let unrestricted = translate(false, false);
        let identity_rejected = translate(false, true);
        let mention_required = translate(true, false);
        let identity_rejected_and_mention_required = translate(true, true);

        assert_eq!(
            (unrestricted.admission, unrestricted.attention),
            (Admission::Admit, Attention::Normal)
        );
        assert_eq!(
            (identity_rejected.admission, identity_rejected.attention),
            (Admission::Reject, Attention::Normal)
        );
        assert_eq!(
            (mention_required.admission, mention_required.attention),
            (Admission::Admit, Attention::MentionOnly)
        );
        assert_eq!(
            (
                identity_rejected_and_mention_required.admission,
                identity_rejected_and_mention_required.attention,
            ),
            (Admission::Reject, Attention::MentionOnly)
        );
    }
}
