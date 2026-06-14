//! Snowflake: a validated Discord ID for use at the MCP tool boundary.

use std::num::NonZeroU64;

use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};

/// A Discord snowflake parsed from an MCP tool argument.
///
/// Rejects `0`, which would make serenity's `*Id::new` panic at construction
/// (its wrappers hold a `NonZeroU64`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Snowflake(NonZeroU64);

impl Snowflake {
    /// Returns `None` if `n` is zero.
    pub fn new(n: u64) -> Option<Self> {
        NonZeroU64::new(n).map(Self)
    }

    #[cfg(test)]
    pub fn get(self) -> u64 {
        self.0.get()
    }

    /// Converts to a serenity [`ChannelId`].
    pub fn channel(self) -> ChannelId {
        ChannelId::new(self.0.get())
    }

    /// Converts to a serenity [`MessageId`].
    pub fn message(self) -> MessageId {
        MessageId::new(self.0.get())
    }

    /// Converts to a serenity [`UserId`].
    pub fn user(self) -> UserId {
        UserId::new(self.0.get())
    }

    /// Converts to a serenity [`GuildId`].
    pub fn guild(self) -> GuildId {
        GuildId::new(self.0.get())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero() {
        assert!(Snowflake::new(0).is_none());
    }

    #[test]
    fn accepts_nonzero() {
        let s = Snowflake::new(42).unwrap();
        assert_eq!(s.get(), 42);
    }

    #[test]
    fn round_trips_to_serenity_ids() {
        let s = Snowflake::new(12345).unwrap();
        assert_eq!(s.channel(), ChannelId::new(12345));
        assert_eq!(s.message(), MessageId::new(12345));
        assert_eq!(s.user(), UserId::new(12345));
        assert_eq!(s.guild(), GuildId::new(12345));
    }
}
