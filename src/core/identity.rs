//! Identities for configured module instances and their conversations.

use std::{fmt, num::NonZeroU64};
use thiserror::Error;

/// The provider namespace used by configured module instances.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ProviderId(String);

impl ProviderId {
    /// Borrows the provider namespace.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Validates a provider namespace at the composition boundary.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentityError {
                kind: IdentityErrorKind::EmptyProvider,
            });
        }
        Ok(Self(value))
    }
}

/// The configured slot within a provider namespace.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ModuleSlotId(String);

impl ModuleSlotId {
    /// Borrows the configured slot.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Validates an explicit configured slot without deriving it from credentials.
    pub fn new(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentityError {
                kind: IdentityErrorKind::EmptyInstance,
            });
        }
        Ok(Self(value))
    }
}

/// A configured module slot, independent of credentials and display names.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ModuleInstanceId {
    provider: ProviderId,
    instance: ModuleSlotId,
}

impl ModuleInstanceId {
    /// Borrows the provider namespace.
    pub fn provider(&self) -> &ProviderId {
        &self.provider
    }
    /// Borrows the configured slot.
    pub fn slot(&self) -> &ModuleSlotId {
        &self.instance
    }

    /// Qualifies a configured slot with its distinct provider namespace.
    pub fn new(provider: ProviderId, instance: ModuleSlotId) -> Self {
        Self { provider, instance }
    }
}

/// A provider-local identity whose numeric and opaque forms remain distinct.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ConversationId(LocalIdentity);

#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
enum LocalIdentity {
    Numeric(NonZeroU64),
    Opaque(String),
}

impl ConversationId {
    /// Borrows the provider-local identity without losing its kind.
    pub(crate) fn identity(&self) -> LocalIdentityRef<'_> {
        self.0.identity()
    }

    /// Returns opaque text without interpreting numeric-looking values.
    pub fn as_opaque(&self) -> Option<&str> {
        match &self.0 {
            LocalIdentity::Numeric(_) => None,
            LocalIdentity::Opaque(value) => Some(value),
        }
    }

    /// Returns a numeric identity without interpreting opaque text.
    pub fn as_numeric(&self) -> Option<NonZeroU64> {
        match self.0 {
            LocalIdentity::Numeric(value) => Some(value),
            LocalIdentity::Opaque(_) => None,
        }
    }

    /// Preserves the numeric ordering of a nonzero provider identifier.
    pub fn numeric(value: NonZeroU64) -> Self {
        Self(LocalIdentity::Numeric(value))
    }

    /// Preserves an opaque identifier without parsing numeric-looking text.
    pub fn opaque(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentityError {
                kind: IdentityErrorKind::EmptyConversation,
            });
        }
        Ok(Self(LocalIdentity::Opaque(value)))
    }
}

/// A conversation qualified by its configured module instance.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ConversationRef {
    module: ModuleInstanceId,
    local: ConversationId,
}

impl ConversationRef {
    /// Borrows the configured module.
    pub fn module(&self) -> &ModuleInstanceId {
        &self.module
    }
    /// Borrows the provider-local conversation.
    pub fn local(&self) -> &ConversationId {
        &self.local
    }

    /// Qualifies a validated local identity with its module instance.
    pub fn new(module: ModuleInstanceId, local: ConversationId) -> Self {
        Self { module, local }
    }
}

/// A rejected scheduling identity.
#[derive(Debug, Error)]
#[error("invalid scheduling identity")]
pub struct IdentityError {
    /// The invalid identity component.
    #[source]
    pub kind: IdentityErrorKind,
}

/// Identity construction failures.
#[derive(Debug, Error, Eq, PartialEq)]
pub enum IdentityErrorKind {
    /// The provider name is empty.
    #[error("provider must not be empty")]
    EmptyProvider,
    /// The configured module slot is empty.
    #[error("module instance must not be empty")]
    EmptyInstance,
    /// The opaque conversation identity is empty.
    #[error("conversation identity must not be empty")]
    EmptyConversation,
    /// The opaque workspace identity is empty.
    #[error("workspace identity must not be empty")]
    EmptyWorkspace,
    /// The opaque actor identity is empty.
    #[error("actor identity must not be empty")]
    EmptyActor,
    /// The opaque message identity is empty.
    #[error("message identity must not be empty")]
    EmptyMessage,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_reject_empty_components_and_accept_observed_values() {
        assert_eq!(
            ProviderId::new("").unwrap_err().kind,
            IdentityErrorKind::EmptyProvider
        );
        assert_eq!(
            ModuleSlotId::new("").unwrap_err().kind,
            IdentityErrorKind::EmptyInstance
        );
        assert_eq!(
            ConversationId::opaque("").unwrap_err().kind,
            IdentityErrorKind::EmptyConversation
        );
        assert!(ProviderId::new("discord").is_ok());
        assert!(ModuleSlotId::new("primary").is_ok());
        assert!(ConversationId::opaque("room/42").is_ok());
        assert_ne!(
            ConversationId::numeric(NonZeroU64::new(42).unwrap()),
            ConversationId::opaque("42").unwrap()
        );
    }
}

/// A provider-local actor identity with distinct numeric and opaque forms.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ActorId(LocalIdentity);
impl ActorId {
    /// Borrows the provider-local identity without losing its kind.
    pub(crate) fn identity(&self) -> LocalIdentityRef<'_> {
        self.0.identity()
    }

    /// Returns opaque text without interpreting numeric-looking values.
    pub fn as_opaque(&self) -> Option<&str> {
        match &self.0 {
            LocalIdentity::Numeric(_) => None,
            LocalIdentity::Opaque(value) => Some(value),
        }
    }

    /// Preserves a nonzero provider identifier.
    pub fn numeric(value: NonZeroU64) -> Self {
        Self(LocalIdentity::Numeric(value))
    }
    /// Preserves opaque text without interpreting numeric-looking identifiers.
    pub fn opaque(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentityError {
                kind: IdentityErrorKind::EmptyActor,
            });
        }
        Ok(Self(LocalIdentity::Opaque(value)))
    }
    /// Returns a numeric identity without interpreting opaque text.
    pub fn as_numeric(&self) -> Option<NonZeroU64> {
        match self.0 {
            LocalIdentity::Numeric(value) => Some(value),
            LocalIdentity::Opaque(_) => None,
        }
    }
}

/// A provider-local message identity with distinct numeric and opaque forms.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MessageId(LocalIdentity);
impl MessageId {
    /// Borrows the provider-local identity without losing its kind.
    pub(crate) fn identity(&self) -> LocalIdentityRef<'_> {
        self.0.identity()
    }

    /// Returns opaque text without interpreting numeric-looking values.
    pub fn as_opaque(&self) -> Option<&str> {
        match &self.0 {
            LocalIdentity::Numeric(_) => None,
            LocalIdentity::Opaque(value) => Some(value),
        }
    }

    /// Preserves a nonzero provider identifier.
    pub fn numeric(value: NonZeroU64) -> Self {
        Self(LocalIdentity::Numeric(value))
    }
    /// Preserves opaque text without interpreting numeric-looking identifiers.
    pub fn opaque(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentityError {
                kind: IdentityErrorKind::EmptyMessage,
            });
        }
        Ok(Self(LocalIdentity::Opaque(value)))
    }
    /// Returns a numeric identity without interpreting opaque text.
    pub fn as_numeric(&self) -> Option<NonZeroU64> {
        match self.0 {
            LocalIdentity::Numeric(value) => Some(value),
            LocalIdentity::Opaque(_) => None,
        }
    }
}

/// An actor qualified by its module.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct ActorRef {
    module: ModuleInstanceId,
    local: ActorId,
}
impl ActorRef {
    /// Qualifies a validated local identity.
    pub fn new(module: ModuleInstanceId, local: ActorId) -> Self {
        Self { module, local }
    }
    /// Borrows the complete scope.
    pub fn module(&self) -> &ModuleInstanceId {
        &self.module
    }
    /// Borrows the provider-local identity.
    pub fn local(&self) -> &ActorId {
        &self.local
    }
}

/// A message qualified by its conversation.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct MessageRef {
    conversation: ConversationRef,
    local: MessageId,
}
impl MessageRef {
    /// Qualifies a validated local identity.
    pub fn new(conversation: ConversationRef, local: MessageId) -> Self {
        Self {
            conversation,
            local,
        }
    }
    /// Borrows the complete scope.
    pub fn conversation(&self) -> &ConversationRef {
        &self.conversation
    }
    /// Borrows the provider-local identity.
    pub fn local(&self) -> &MessageId {
        &self.local
    }
}

impl fmt::Display for ConversationId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            LocalIdentity::Numeric(value) => value.fmt(formatter),
            LocalIdentity::Opaque(value) => value.fmt(formatter),
        }
    }
}

impl fmt::Display for ActorId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            LocalIdentity::Numeric(value) => value.fmt(formatter),
            LocalIdentity::Opaque(value) => value.fmt(formatter),
        }
    }
}

impl fmt::Display for MessageId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            LocalIdentity::Numeric(value) => value.fmt(formatter),
            LocalIdentity::Opaque(value) => value.fmt(formatter),
        }
    }
}

/// A provider-local identity whose numeric and opaque forms remain distinct.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct WorkspaceId(LocalIdentity);

impl WorkspaceId {
    /// Borrows the provider-local identity without losing its kind.
    pub(crate) fn identity(&self) -> LocalIdentityRef<'_> {
        self.0.identity()
    }

    /// Returns opaque text without interpreting numeric-looking values.
    pub fn as_opaque(&self) -> Option<&str> {
        match &self.0 {
            LocalIdentity::Numeric(_) => None,
            LocalIdentity::Opaque(value) => Some(value),
        }
    }

    /// Returns a numeric identity without interpreting opaque text.
    pub fn as_numeric(&self) -> Option<NonZeroU64> {
        match self.0 {
            LocalIdentity::Numeric(value) => Some(value),
            LocalIdentity::Opaque(_) => None,
        }
    }

    /// Preserves the numeric ordering of a nonzero provider identifier.
    pub fn numeric(value: NonZeroU64) -> Self {
        Self(LocalIdentity::Numeric(value))
    }

    /// Preserves an opaque identifier without parsing numeric-looking text.
    pub fn opaque(value: impl Into<String>) -> Result<Self, IdentityError> {
        let value = value.into();
        if value.is_empty() {
            return Err(IdentityError {
                kind: IdentityErrorKind::EmptyWorkspace,
            });
        }
        Ok(Self(LocalIdentity::Opaque(value)))
    }
}

/// A workspace qualified by its configured module instance.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct WorkspaceRef {
    module: ModuleInstanceId,
    local: WorkspaceId,
}

impl WorkspaceRef {
    /// Borrows the configured module.
    pub fn module(&self) -> &ModuleInstanceId {
        &self.module
    }
    /// Borrows the provider-local workspace.
    pub fn local(&self) -> &WorkspaceId {
        &self.local
    }

    /// Qualifies a validated local identity with its module instance.
    pub fn new(module: ModuleInstanceId, local: WorkspaceId) -> Self {
        Self { module, local }
    }
}

/// Borrowed provider-local identity preserving its numeric or opaque form.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LocalIdentityRef<'a> {
    Numeric(NonZeroU64),
    Opaque(&'a str),
}
impl LocalIdentity {
    fn identity(&self) -> LocalIdentityRef<'_> {
        match self {
            Self::Numeric(value) => LocalIdentityRef::Numeric(*value),
            Self::Opaque(value) => LocalIdentityRef::Opaque(value),
        }
    }
}

#[cfg(test)]
mod workspace_tests {
    use super::*;
    #[test]
    fn workspace_preserves_identity_kind_and_source_scope() {
        assert_eq!(
            WorkspaceId::opaque("").unwrap_err().kind,
            IdentityErrorKind::EmptyWorkspace
        );
        let numeric = WorkspaceId::numeric(NonZeroU64::new(42).unwrap());
        let opaque = WorkspaceId::opaque("42").unwrap();
        assert_ne!(numeric, opaque);
        assert_eq!(
            numeric.identity(),
            LocalIdentityRef::Numeric(NonZeroU64::new(42).unwrap())
        );
        assert_eq!(opaque.identity(), LocalIdentityRef::Opaque("42"));
        let source = ModuleInstanceId::new(
            ProviderId::new("provider/with:punctuation").unwrap(),
            ModuleSlotId::new("secondary").unwrap(),
        );
        let workspace = WorkspaceRef::new(source.clone(), opaque);
        assert_eq!(workspace.module(), &source);
        assert_eq!(
            workspace.module().provider().as_str(),
            "provider/with:punctuation"
        );
        assert_eq!(workspace.module().slot().as_str(), "secondary");
    }
}
