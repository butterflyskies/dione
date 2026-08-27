use std::fmt;

/// Provider-tagged message identifier.
///
/// Each transport owns a distinct variant so native identifiers cannot
/// silently collide across providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MessageRef {
    /// A Discord message identified by its snowflake.
    Discord { snowflake: u64 },
}

impl MessageRef {
    /// Construct a reference to a Discord message.
    pub const fn discord(snowflake: u64) -> Self {
        Self::Discord { snowflake }
    }

    /// Return the Discord snowflake when this is a Discord message.
    pub const fn discord_snowflake(self) -> Option<u64> {
        match self {
            Self::Discord { snowflake } => Some(snowflake),
        }
    }
}

impl fmt::Display for MessageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Discord { snowflake } => write!(f, "discord:{snowflake}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::MessageRef;

    #[test]
    fn message_ref_preserves_discord_provider_and_snowflake() {
        let message = MessageRef::discord(42);

        assert_eq!(message, MessageRef::Discord { snowflake: 42 });
        assert_eq!(message.discord_snowflake(), Some(42));
    }

    #[test]
    fn message_ref_display_includes_provider_namespace() {
        let message = MessageRef::discord(42);

        assert_eq!(message.to_string(), "discord:42");
    }
}

/// Transport-agnostic channel/destination identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ChannelRef(u64);

impl ChannelRef {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for ChannelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Transport-agnostic principal (user/bot) identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PrincipalRef(u64);

impl PrincipalRef {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for PrincipalRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// SHA-256 hash of message content.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContentHash(pub(crate) [u8; 32]);

/// Opaque activation root identifier, minted by the RootRegistry.
///
/// The model may inspect a redacted description but never supplies,
/// replaces, or rebinds this ID.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RootId(u64);

impl RootId {
    pub(crate) const fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for RootId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "root-{}", self.0)
    }
}

/// Epoch identifier — monotonically increasing, reset on process restart.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct EpochId(u64);

impl EpochId {
    pub(crate) const fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for EpochId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "epoch-{}", self.0)
    }
}
