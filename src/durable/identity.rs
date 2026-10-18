//! Checked identities shared by durable queue codecs and consumers.

use serde::{Deserialize, Deserializer, Serialize};
use std::{fmt, str::FromStr};
use thiserror::Error;

/// A rejected exact identity value.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct IdentityError {
    /// The invalid identity category.
    #[source]
    pub kind: IdentityErrorKind,
}
/// Exhaustive durable identity validation failures.
#[derive(Debug, Error)]
pub enum IdentityErrorKind {
    /// Consumer identities use the existing ASCII identifier alphabet.
    #[error("invalid consumer id")]
    Consumer,
    /// Lease tokens are bounded opaque strings addressable by the public parser.
    #[error("invalid delivery token")]
    Token,
    /// Endpoint identities must be bounded, nonempty and free of edge whitespace.
    #[error("invalid endpoint id")]
    Endpoint,
}

/// Monotonic event identity; zero is a valid historical value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct EventId(u64);
impl EventId {
    /// Constructs a historical or allocated event identifier.
    pub fn new(value: u64) -> Self {
        Self(value)
    }
    /// Returns the exact stored integer.
    pub fn get(self) -> u64 {
        self.0
    }
}
impl fmt::Display for EventId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Exact persisted registration identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct ConsumerId(String);
impl ConsumerId {
    /// Parses an external consumer argument with historical trim semantics.
    pub fn parse(value: &str) -> Result<Self, IdentityError> {
        Self::from_stored(value.trim())
    }
    /// Validates stored bytes without changing their spelling.
    pub fn from_stored(value: &str) -> Result<Self, IdentityError> {
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
        {
            return Err(IdentityError {
                kind: IdentityErrorKind::Consumer,
            });
        }
        Ok(Self(value.to_owned()))
    }
    /// Borrows the exact identifier spelling.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl fmt::Display for ConsumerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl<'de> Deserialize<'de> for ConsumerId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_stored(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl FromStr for ConsumerId {
    type Err = IdentityError;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

/// Opaque token authorizing acknowledgement of one retained lease.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct DeliveryToken(String);
impl DeliveryToken {
    /// Parses an external acknowledgement argument with historical trim semantics.
    pub fn parse(value: &str) -> Result<Self, IdentityError> {
        Self::from_stored(value.trim())
    }
    /// Accepts exact bounded opaque tokens, including internal whitespace and Unicode.
    pub fn from_stored(value: &str) -> Result<Self, IdentityError> {
        if value.is_empty() || value.len() > 128 || value.trim() != value {
            return Err(IdentityError {
                kind: IdentityErrorKind::Token,
            });
        }
        Ok(Self(value.to_owned()))
    }
    /// Borrows the exact stored token.
    pub fn as_str(&self) -> &str {
        &self.0
    }
    pub(super) fn generated(event: EventId, generation: u64) -> Self {
        Self(format!("dione-{event}-{generation}"))
    }
}
impl fmt::Display for DeliveryToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
impl<'de> Deserialize<'de> for DeliveryToken {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::from_stored(&String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}

/// Exact adapter-validated resident destination, opaque to queue routing.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(transparent)]
pub struct EndpointId(String);
impl EndpointId {
    /// Validates an opaque endpoint without trimming or interpreting its format.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() || value.len() > 256 || value.trim() != value {
            return Err(IdentityError {
                kind: IdentityErrorKind::Endpoint,
            });
        }
        Ok(Self(value))
    }
    /// Borrows the exact endpoint identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for EndpointId {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(serde::de::Error::custom)
    }
}
impl fmt::Display for EndpointId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}
