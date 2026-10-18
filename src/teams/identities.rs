//! Opaque Teams coordinates; syntax never grants reply authority.

use serde::{Deserialize, Serialize};
use std::{fmt, str::FromStr};
use thiserror::Error;

/// The semantic coordinate whose syntax was invalid.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IdentityKind {
    /// The Microsoft tenant.
    Tenant,
    /// The OAuth application audience or client.
    Application,
    /// The bot account within Teams.
    Bot,
    /// The participant account within Teams.
    Actor,
    /// The Teams conversation.
    Conversation,
    /// An incoming or outgoing Teams activity.
    Activity,
}

/// An opaque coordinate failed validation without retaining its supplied value.
#[derive(Debug, Error, Eq, PartialEq)]
#[error("invalid Teams {identity:?} identity")]
pub struct IdentityError {
    /// The coordinate being parsed.
    pub identity: IdentityKind,
    /// The violated persisted-format constraint.
    #[source]
    pub kind: IdentityErrorKind,
}

/// Constraints shared by the historical Teams durable identity format.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum IdentityErrorKind {
    /// An identity must contain at least one byte.
    #[error("identity is empty")]
    Empty,
    /// An identity may contain at most 512 UTF-8 bytes.
    #[error("identity exceeds 512 bytes")]
    TooLong,
    /// Control characters cannot occur in an identity.
    #[error("identity contains a control character")]
    ControlCharacter,
}

fn validate(value: &str, identity: IdentityKind) -> Result<(), IdentityError> {
    let kind = if value.is_empty() {
        IdentityErrorKind::Empty
    } else if value.len() > 512 {
        IdentityErrorKind::TooLong
    } else if value.chars().any(char::is_control) {
        IdentityErrorKind::ControlCharacter
    } else {
        return Ok(());
    };
    Err(IdentityError { identity, kind })
}

/// An opaque Teams tenant coordinate, preserving its original spelling.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct TenantId(String);

impl TenantId {
    /// Validates a coordinate without normalizing its spelling.
    pub fn parse(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        validate(&value, IdentityKind::Tenant)?;
        Ok(Self(value))
    }

    /// Returns the original value at a wire boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for TenantId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for TenantId {
    type Error = IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<TenantId> for String {
    fn from(value: TenantId) -> Self {
        value.0
    }
}

impl fmt::Display for TenantId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An opaque Teams application coordinate, preserving its original spelling.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ApplicationId(String);

impl ApplicationId {
    /// Validates a coordinate without normalizing its spelling.
    pub fn parse(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        validate(&value, IdentityKind::Application)?;
        Ok(Self(value))
    }

    /// Returns the original value at a wire boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ApplicationId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for ApplicationId {
    type Error = IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ApplicationId> for String {
    fn from(value: ApplicationId) -> Self {
        value.0
    }
}

impl fmt::Display for ApplicationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An opaque Teams bot coordinate, preserving its original spelling.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct BotId(String);

impl BotId {
    /// Validates a coordinate without normalizing its spelling.
    pub fn parse(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        validate(&value, IdentityKind::Bot)?;
        Ok(Self(value))
    }

    /// Returns the original value at a wire boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for BotId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for BotId {
    type Error = IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<BotId> for String {
    fn from(value: BotId) -> Self {
        value.0
    }
}

impl fmt::Display for BotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An opaque Teams actor coordinate, preserving its original spelling.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActorId(String);

impl ActorId {
    /// Validates a coordinate without normalizing its spelling.
    pub fn parse(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        validate(&value, IdentityKind::Actor)?;
        Ok(Self(value))
    }

    /// Returns the original value at a wire boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ActorId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for ActorId {
    type Error = IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ActorId> for String {
    fn from(value: ActorId) -> Self {
        value.0
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An opaque Teams conversation coordinate, preserving its original spelling.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ConversationId(String);

impl ConversationId {
    /// Validates a coordinate without normalizing its spelling.
    pub fn parse(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        validate(&value, IdentityKind::Conversation)?;
        Ok(Self(value))
    }

    /// Returns the original value at a wire boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ConversationId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for ConversationId {
    type Error = IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ConversationId> for String {
    fn from(value: ConversationId) -> Self {
        value.0
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// An opaque Teams activity coordinate, preserving its original spelling.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ActivityId(String);

impl ActivityId {
    /// Validates a coordinate without normalizing its spelling.
    pub fn parse(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        validate(&value, IdentityKind::Activity)?;
        Ok(Self(value))
    }

    /// Returns the original value at a wire boundary.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl FromStr for ActivityId {
    type Err = IdentityError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::parse(value)
    }
}

impl TryFrom<String> for ActivityId {
    type Error = IdentityError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl From<ActivityId> for String {
    fn from(value: ActivityId) -> Self {
        value.0
    }
}

impl fmt::Display for ActivityId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::error::Error;

    #[test]
    fn opaque_spelling_and_utf8_byte_boundaries_are_preserved() {
        for spelling in [" Mixed/CASE + opaque ", "é", "x".repeat(512).as_str()] {
            let id = ActivityId::parse(spelling).unwrap();
            assert_eq!(id.as_str(), spelling);
            let encoded = serde_json::to_string(&id).unwrap();
            assert_eq!(serde_json::from_str::<ActivityId>(&encoded).unwrap(), id);
        }
        assert_ne!(
            ActivityId::parse("A").unwrap(),
            ActivityId::parse("a").unwrap()
        );
        assert!(ActorId::parse("é".repeat(256)).is_ok());
        let error = ActorId::parse("é".repeat(257)).unwrap_err();
        assert_eq!(error.identity, IdentityKind::Actor);
        assert_eq!(error.kind, IdentityErrorKind::TooLong);
        assert!(error.source().is_some());
        for spelling in ["", "a\n", "a\0"] {
            assert!(TenantId::parse(spelling).is_err());
            assert!(ApplicationId::parse(spelling).is_err());
            assert!(BotId::parse(spelling).is_err());
            assert!(ActorId::parse(spelling).is_err());
            assert!(ConversationId::parse(spelling).is_err());
            assert!(ActivityId::parse(spelling).is_err());
        }
    }
}
