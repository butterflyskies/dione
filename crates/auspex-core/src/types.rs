use std::fmt;

/// Transport-agnostic message identifier.
///
/// Dione converts Discord snowflakes into this; other transports would
/// supply their own opaque IDs. The inner value has no Discord semantics
/// inside auspex-core.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MessageRef(u64);

impl MessageRef {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for MessageRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
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
