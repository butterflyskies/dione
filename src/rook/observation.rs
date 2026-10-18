//! Private durable projection of common source observations.

use std::num::NonZeroU64;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::core::{
    ActorRef, ConversationRef, LocalIdentityRef, MessageRef, ModuleInstanceId, WorkspaceRef,
    context::{
        Availability, ObservationOrigin, ReferenceConsistency, ReferenceRelation, UnavailableReason,
    },
    observation::{MessageObservation, MessageOccurrence, SourceEventIdentity},
};

/// A rejected durable observation or supplemental metadata value.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
#[error("invalid retained observation: {kind}")]
pub struct ObservationError {
    /// The violated observation invariant.
    #[source]
    pub kind: ObservationErrorKind,
}

impl ObservationError {
    pub(crate) fn new(kind: ObservationErrorKind) -> Self {
        Self { kind }
    }
}

/// Structural validation failures at the durable observation boundary.
#[derive(Clone, Debug, PartialEq, Eq, Error)]
pub enum ObservationErrorKind {
    /// The provider or configured module slot is empty.
    #[error("empty source identity")]
    EmptySource,
    /// An opaque provider-local identifier is empty.
    #[error("empty local identity")]
    EmptyLocal,
    /// The native codec is empty or too long, or its version is zero.
    #[error("invalid native supplement codec")]
    InvalidCodec,
    /// The native payload exceeds one MiB.
    #[error("native supplement exceeds limit")]
    SupplementTooLarge,
    /// An edit occurrence differs from the retained context revision.
    #[error("event revision differs from context edit time")]
    RevisionMismatch,
    /// The sender belongs to another configured module.
    #[error("sender source differs from event source")]
    SenderSourceMismatch,
    /// The workspace belongs to another configured module.
    #[error("workspace source differs from event source")]
    WorkspaceSourceMismatch,
    /// The resolved sender does not belong to the resolved message's module.
    #[error("resolved sender source differs from resolved message source")]
    ResolvedSenderSourceMismatch,
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
struct SourceKey {
    provider: String,
    slot: String,
}

impl From<&ModuleInstanceId> for SourceKey {
    fn from(value: &ModuleInstanceId) -> Self {
        Self {
            provider: value.provider().as_str().into(),
            slot: value.slot().as_str().into(),
        }
    }
}

impl SourceKey {
    fn validate(&self) -> Result<(), ObservationError> {
        if self.provider.is_empty() || self.slot.is_empty() {
            return Err(ObservationError::new(ObservationErrorKind::EmptySource));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum LocalKey {
    Numeric(NonZeroU64),
    Opaque(String),
}

impl From<LocalIdentityRef<'_>> for LocalKey {
    fn from(value: LocalIdentityRef<'_>) -> Self {
        match value {
            LocalIdentityRef::Numeric(value) => Self::Numeric(value),
            LocalIdentityRef::Opaque(value) => Self::Opaque(value.into()),
        }
    }
}

impl LocalKey {
    fn validate(&self) -> Result<(), ObservationError> {
        if matches!(self, Self::Opaque(value) if value.is_empty()) {
            return Err(ObservationError::new(ObservationErrorKind::EmptyLocal));
        }
        Ok(())
    }
}

/// A conversation identity qualified by its configured module.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConversationKey {
    source: SourceKey,
    local: LocalKey,
}

impl From<&ConversationRef> for ConversationKey {
    fn from(value: &ConversationRef) -> Self {
        Self {
            source: value.module().into(),
            local: value.local().identity().into(),
        }
    }
}

impl ConversationKey {
    pub(crate) fn matches_source(&self, source: &ModuleInstanceId) -> bool {
        self.source == SourceKey::from(source)
    }

    pub(crate) fn numeric(&self) -> Option<NonZeroU64> {
        match self.local {
            LocalKey::Numeric(value) => Some(value),
            LocalKey::Opaque(_) => None,
        }
    }

    /// Checks that the source and provider-local identity components are valid.
    ///
    /// # Errors
    /// Rejects an empty provider, module slot, or opaque local identifier.
    pub fn validate(&self) -> Result<(), ObservationError> {
        self.source.validate()?;
        self.local.validate()
    }
}

/// An actor identity qualified by its configured module.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActorKey {
    source: SourceKey,
    local: LocalKey,
}

impl From<&ActorRef> for ActorKey {
    fn from(value: &ActorRef) -> Self {
        Self {
            source: value.module().into(),
            local: value.local().identity().into(),
        }
    }
}

impl ActorKey {
    pub(crate) fn same_source(&self, room: &ConversationKey) -> bool {
        self.source == room.source
    }

    pub(crate) fn numeric(&self) -> Option<NonZeroU64> {
        match self.local {
            LocalKey::Numeric(value) => Some(value),
            LocalKey::Opaque(_) => None,
        }
    }

    /// Checks that the source and provider-local identity components are valid.
    ///
    /// # Errors
    /// Rejects an empty provider, module slot, or opaque local identifier.
    pub fn validate(&self) -> Result<(), ObservationError> {
        self.source.validate()?;
        self.local.validate()
    }
}

/// A workspace identity qualified by its configured module.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceKey {
    source: SourceKey,
    local: LocalKey,
}

impl From<&WorkspaceRef> for WorkspaceKey {
    fn from(value: &WorkspaceRef) -> Self {
        Self {
            source: value.module().into(),
            local: value.local().identity().into(),
        }
    }
}

impl WorkspaceKey {
    /// Checks that the source and provider-local identity components are valid.
    ///
    /// # Errors
    /// Rejects an empty provider, module slot, or opaque local identifier.
    pub fn validate(&self) -> Result<(), ObservationError> {
        self.source.validate()?;
        self.local.validate()
    }
}

/// An exact message identity qualified by its conversation and module.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MessageKey {
    conversation: ConversationKey,
    local: LocalKey,
}

impl From<&MessageRef> for MessageKey {
    fn from(value: &MessageRef) -> Self {
        Self {
            conversation: value.conversation().into(),
            local: value.local().identity().into(),
        }
    }
}

impl MessageKey {
    /// Returns the source-qualified conversation containing this message.
    pub fn conversation(&self) -> &ConversationKey {
        &self.conversation
    }

    pub(crate) fn numeric(&self) -> Option<NonZeroU64> {
        match self.local {
            LocalKey::Numeric(value) => Some(value),
            LocalKey::Opaque(_) => None,
        }
    }

    /// Checks the conversation and provider-local message identity.
    ///
    /// # Errors
    /// Rejects an empty source or opaque local identifier.
    pub fn validate(&self) -> Result<(), ObservationError> {
        self.conversation.validate()?;
        self.local.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Occurrence {
    Created,
    Edited { revision: DateTime<Utc> },
}

/// An exact source message occurrence, independent of delivery order.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EventKey {
    message: MessageKey,
    occurrence: Occurrence,
}

impl From<&SourceEventIdentity> for EventKey {
    fn from(value: &SourceEventIdentity) -> Self {
        let occurrence = match value.occurrence() {
            MessageOccurrence::Created => Occurrence::Created,
            MessageOccurrence::Edited { revision } => Occurrence::Edited {
                revision: *revision,
            },
        };
        Self {
            message: value.message().into(),
            occurrence,
        }
    }
}

impl EventKey {
    /// Returns the exact message associated with this occurrence.
    pub fn message(&self) -> &MessageKey {
        &self.message
    }

    /// Checks that the source and provider-local identity components are valid.
    ///
    /// # Errors
    /// Rejects an empty provider, module slot, or opaque local identifier.
    pub fn validate(&self) -> Result<(), ObservationError> {
        self.message.validate()
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "availability",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum Observed<T> {
    Known(T),
    Unavailable(Missing),
}

impl<T> Observed<T> {
    fn capture<U>(value: &Availability<U>, convert: impl FnOnce(&U) -> T) -> Self {
        match value {
            Availability::Known(value) => Self::Known(convert(value)),
            Availability::Unavailable(reason) => Self::Unavailable(match reason {
                UnavailableReason::NotReported => Missing::NotReported,
                UnavailableReason::NotResolved => Missing::NotResolved,
                UnavailableReason::Unsupported => Missing::Unsupported,
            }),
        }
    }
    fn known(&self) -> Option<&T> {
        match self {
            Self::Known(value) => Some(value),
            Self::Unavailable(_) => None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Missing {
    NotReported,
    NotResolved,
    Unsupported,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Sender {
    reference: ActorKey,
    name: String,
    bot: Observed<bool>,
    represented_identity: Observed<ActorKey>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Resolved {
    reference: MessageKey,
    sender: ActorKey,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum RelationKind {
    Reply,
    Forward,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Consistency {
    NoResolvedObservation,
    ResolvedOnly,
    InsufficientEvidence,
    Matches,
    Conflicts,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Relation {
    kind: RelationKind,
    target: Observed<MessageKey>,
    resolved: Observed<Resolved>,
    consistency: Consistency,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Attachment {
    name: String,
    media_type: Observed<String>,
    size_bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Origin {
    GatewayCreate,
    GatewayEdit,
    Exact,
    History,
    Pins,
    NewSince,
    Search,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Truncation {
    None,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum Completeness {
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Extent {
    origin: Origin,
    local_truncation: Truncation,
    provider_completeness: Completeness,
}

/// Bounded adapter metadata that conveys no routing or reply authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeSupplement {
    codec: String,
    version: u16,
    bytes: Vec<u8>,
}

impl NativeSupplement {
    /// Creates bounded adapter metadata with a nonempty codec and nonzero version.
    ///
    /// The codec is at most 128 bytes and the payload is at most one MiB.
    /// The bytes are uninterpreted and never confer routing or reply authority.
    ///
    /// # Errors
    /// Rejects an empty or oversized codec, version zero, or an oversized payload.
    pub fn new(codec: String, version: u16, bytes: Vec<u8>) -> Result<Self, ObservationError> {
        let value = Self {
            codec,
            version,
            bytes,
        };
        value.validate()?;
        Ok(value)
    }

    /// Checks the codec, version, and byte-length limits.
    ///
    /// # Errors
    /// Rejects an empty or oversized codec, version zero, or a payload above one MiB.
    pub fn validate(&self) -> Result<(), ObservationError> {
        if self.codec.is_empty() || self.codec.len() > 128 || self.version == 0 {
            return Err(ObservationError::new(ObservationErrorKind::InvalidCodec));
        }
        if self.bytes.len() > 1_048_576 {
            return Err(ObservationError::new(
                ObservationErrorKind::SupplementTooLarge,
            ));
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ContextFields {
    sender: Sender,
    content: String,
    created_at: DateTime<Utc>,
    workspace: Observed<WorkspaceKey>,
    edited_at: Observed<Option<DateTime<Utc>>>,
    relation: Observed<Relation>,
    attachments: Observed<Vec<Attachment>>,
    extent: Extent,
}

impl ContextFields {
    pub(super) fn capture(context: &crate::core::context::ContextObservation) -> Self {
        let sender = context.sender();
        Self {
            sender: Sender {
                reference: sender.reference().into(),
                name: sender.name().into(),
                bot: Observed::capture(sender.bot(), |value| *value),
                represented_identity: Observed::capture(sender.represented_identity(), |value| {
                    value.into()
                }),
            },
            content: context.content().into(),
            created_at: *context.created_at(),
            workspace: Observed::capture(context.workspace(), |value| value.into()),
            edited_at: Observed::capture(context.edited_at(), |value| *value),
            relation: Observed::capture(context.relation(), |relation| Relation {
                kind: match relation.kind() {
                    ReferenceRelation::Reply => RelationKind::Reply,
                    ReferenceRelation::Forward => RelationKind::Forward,
                    ReferenceRelation::Unknown => RelationKind::Unknown,
                },
                target: Observed::capture(relation.target(), |value| value.into()),
                resolved: Observed::capture(relation.resolved(), |resolved| Resolved {
                    reference: resolved.reference().into(),
                    sender: resolved.sender().into(),
                }),
                consistency: match relation.consistency() {
                    ReferenceConsistency::NoResolvedObservation => {
                        Consistency::NoResolvedObservation
                    }
                    ReferenceConsistency::ResolvedOnly => Consistency::ResolvedOnly,
                    ReferenceConsistency::InsufficientEvidence => Consistency::InsufficientEvidence,
                    ReferenceConsistency::Matches => Consistency::Matches,
                    ReferenceConsistency::Conflicts => Consistency::Conflicts,
                },
            }),
            attachments: Observed::capture(context.attachments(), |attachments| {
                attachments
                    .iter()
                    .map(|attachment| Attachment {
                        name: attachment.name().into(),
                        media_type: Observed::capture(attachment.media_type(), Clone::clone),
                        size_bytes: attachment.size_bytes(),
                    })
                    .collect()
            }),
            extent: Extent {
                origin: match context.observation().origin() {
                    ObservationOrigin::GatewayCreate => Origin::GatewayCreate,
                    ObservationOrigin::GatewayEdit => Origin::GatewayEdit,
                    ObservationOrigin::Exact => Origin::Exact,
                    ObservationOrigin::History => Origin::History,
                    ObservationOrigin::Pins => Origin::Pins,
                    ObservationOrigin::NewSince => Origin::NewSince,
                    ObservationOrigin::Search => Origin::Search,
                },
                local_truncation: match context.observation().local_truncation() {
                    crate::core::context::LocalTruncation::None => Truncation::None,
                },
                provider_completeness: match context.observation().provider_completeness() {
                    crate::core::context::ProviderCompleteness::Unknown => Completeness::Unknown,
                },
            },
        }
    }

    pub(super) fn validate(&self, reference: &MessageKey) -> Result<(), ObservationError> {
        reference.validate()?;
        validate_context(reference, &self.sender, &self.workspace, &self.relation)
    }
    pub(super) fn sender(&self) -> &ActorKey {
        &self.sender.reference
    }
    pub(super) fn workspace(&self) -> Option<&WorkspaceKey> {
        self.workspace.known()
    }
    pub(super) fn content(&self) -> &str {
        &self.content
    }
    pub(super) fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }
}

fn validate_context(
    reference: &MessageKey,
    sender: &Sender,
    workspace: &Observed<WorkspaceKey>,
    relation: &Observed<Relation>,
) -> Result<(), ObservationError> {
    sender.reference.validate()?;
    let source = &reference.conversation.source;
    if &sender.reference.source != source {
        return Err(ObservationError::new(
            ObservationErrorKind::SenderSourceMismatch,
        ));
    }
    if let Some(sender) = sender.represented_identity.known() {
        sender.validate()?;
    }
    if let Some(workspace) = workspace.known() {
        workspace.validate()?;
        if &workspace.source != source {
            return Err(ObservationError::new(
                ObservationErrorKind::WorkspaceSourceMismatch,
            ));
        }
    }
    if let Some(relation) = relation.known() {
        if let Some(target) = relation.target.known() {
            target.validate()?;
        }
        if let Some(resolved) = relation.resolved.known() {
            resolved.reference.validate()?;
            resolved.sender.validate()?;
            if resolved.sender.source != resolved.reference.conversation.source {
                return Err(ObservationError::new(
                    ObservationErrorKind::ResolvedSenderSourceMismatch,
                ));
            }
        }
    }
    Ok(())
}

/// A validated durable observation with separate common and native metadata.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedObservation {
    key: EventKey,
    sender: Sender,
    content: String,
    created_at: DateTime<Utc>,
    workspace: Observed<WorkspaceKey>,
    edited_at: Observed<Option<DateTime<Utc>>>,
    relation: Observed<Relation>,
    attachments: Observed<Vec<Attachment>>,
    extent: Extent,
    native: Option<NativeSupplement>,
}

impl RetainedObservation {
    pub(crate) fn capture(value: &MessageObservation) -> Self {
        let ContextFields {
            sender,
            content,
            created_at,
            workspace,
            edited_at,
            relation,
            attachments,
            extent,
        } = ContextFields::capture(value.context());
        Self {
            key: value.identity().into(),
            sender,
            content,
            created_at,
            workspace,
            edited_at,
            relation,
            attachments,
            extent,
            native: None,
        }
    }

    /// Returns the source occurrence used for deduplication.
    pub fn key(&self) -> &EventKey {
        &self.key
    }

    /// Attaches bounded native metadata without changing common identities.
    ///
    /// Replaces any existing supplement; it does not alter the occurrence key.
    ///
    /// # Errors
    /// Returns a validation error if either the observation or supplement is invalid.
    pub fn with_native(mut self, supplement: NativeSupplement) -> Result<Self, ObservationError> {
        supplement.validate()?;
        self.native = Some(supplement);
        self.validate()?;
        Ok(self)
    }

    /// Returns supplemental adapter metadata, which conveys no authority.
    pub fn native(&self) -> Option<&NativeSupplement> {
        self.native.as_ref()
    }
    /// Returns the common conversation used by routing policy.
    pub fn conversation(&self) -> &ConversationKey {
        &self.key.message.conversation
    }
    /// Returns the common sender used by routing policy.
    pub fn sender(&self) -> &ActorKey {
        &self.sender.reference
    }
    /// Returns the observed workspace when supplied by the source.
    pub fn workspace(&self) -> Option<&WorkspaceKey> {
        self.workspace.known()
    }
    /// Returns the represented actor when supplied by the source.
    ///
    /// A represented principal can belong to another provider or module; this
    /// descriptive identity does not replace the raw sender or grant authority.
    pub fn represented_sender(&self) -> Option<&ActorKey> {
        self.sender.represented_identity.known()
    }

    /// Checks identities, source coherence, edit revision, and native payload bounds.
    ///
    /// Raw sender and workspace must belong to the event module. Represented
    /// principals and explicit references may identify other modules. A resolved
    /// sender must belong to its resolved message module; target/resolution
    /// disagreements remain observable through the retained consistency field.
    ///
    /// # Errors
    /// Returns the violated identity, source-coherence, revision, or payload bound.
    pub fn validate(&self) -> Result<(), ObservationError> {
        if let Some(native) = self.native() {
            native.validate()?;
        }
        self.key.validate()?;
        validate_context(
            self.key.message(),
            &self.sender,
            &self.workspace,
            &self.relation,
        )?;
        if let Occurrence::Edited { revision } = self.key.occurrence
            && self.edited_at != Observed::Known(Some(revision))
        {
            return Err(ObservationError::new(
                ObservationErrorKind::RevisionMismatch,
            ));
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(remote = "SourceKey", deny_unknown_fields)]
struct SourceKeyWire {
    provider: String,
    slot: String,
}

impl<'de> Deserialize<'de> for SourceKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = SourceKeyWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(remote = "ConversationKey", deny_unknown_fields)]
struct ConversationKeyWire {
    source: SourceKey,
    local: LocalKey,
}

impl<'de> Deserialize<'de> for ConversationKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = ConversationKeyWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(remote = "ActorKey", deny_unknown_fields)]
struct ActorKeyWire {
    source: SourceKey,
    local: LocalKey,
}

impl<'de> Deserialize<'de> for ActorKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = ActorKeyWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(remote = "WorkspaceKey", deny_unknown_fields)]
struct WorkspaceKeyWire {
    source: SourceKey,
    local: LocalKey,
}

impl<'de> Deserialize<'de> for WorkspaceKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = WorkspaceKeyWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(remote = "MessageKey", deny_unknown_fields)]
struct MessageKeyWire {
    conversation: ConversationKey,
    local: LocalKey,
}

impl<'de> Deserialize<'de> for MessageKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = MessageKeyWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(remote = "EventKey", deny_unknown_fields)]
struct EventKeyWire {
    message: MessageKey,
    occurrence: Occurrence,
}

impl<'de> Deserialize<'de> for EventKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = EventKeyWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(remote = "NativeSupplement", deny_unknown_fields)]
struct NativeSupplementWire {
    codec: String,
    version: u16,
    bytes: Vec<u8>,
}

impl<'de> Deserialize<'de> for NativeSupplement {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = NativeSupplementWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(remote = "RetainedObservation", deny_unknown_fields)]
struct RetainedObservationWire {
    key: EventKey,
    sender: Sender,
    content: String,
    created_at: DateTime<Utc>,
    workspace: Observed<WorkspaceKey>,
    edited_at: Observed<Option<DateTime<Utc>>>,
    relation: Observed<Relation>,
    attachments: Observed<Vec<Attachment>>,
    extent: Extent,
    native: Option<NativeSupplement>,
}

impl<'de> Deserialize<'de> for RetainedObservation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = RetainedObservationWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[derive(Deserialize)]
#[serde(
    remote = "LocalKey",
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
enum LocalKeyWire {
    Numeric(NonZeroU64),
    Opaque(String),
}

impl<'de> Deserialize<'de> for LocalKey {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = LocalKeyWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        ActorId, ConversationId, MessageId, ModuleSlotId, ProviderId, WorkspaceId,
        context::{
            AttachmentSummary, ContextMetadata, ContextObservation, ObservationExtent,
            ReferenceObservation, ResolvedReference, SenderObservation,
        },
    };

    fn source(slot: &str) -> ModuleInstanceId {
        ModuleInstanceId::new(
            ProviderId::new("test/provider").unwrap(),
            ModuleSlotId::new(slot).unwrap(),
        )
    }

    fn observation(edited: bool) -> MessageObservation {
        let source = source("secondary");
        let room = ConversationRef::new(source.clone(), ConversationId::opaque("room:42").unwrap());
        let author = ActorRef::new(
            source.clone(),
            ActorId::numeric(NonZeroU64::new(42).unwrap()),
        );
        let reference = MessageRef::new(room.clone(), MessageId::opaque("42").unwrap());
        let target = MessageRef::new(room, MessageId::numeric(NonZeroU64::new(42).unwrap()));
        let created = "2026-10-13T11:34:13.200000001Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let revision = "2026-10-13T11:35:13.200000002Z"
            .parse::<DateTime<Utc>>()
            .unwrap();
        let context = ContextObservation::new(
            reference,
            SenderObservation::new(
                author.clone(),
                "sender name".into(),
                Availability::Known(true),
                Availability::Known(ActorRef::new(
                    source.clone(),
                    ActorId::opaque("42").unwrap(),
                )),
            ),
            "original content".into(),
            created,
            ContextMetadata::new(
                Availability::Known(WorkspaceRef::new(
                    source,
                    WorkspaceId::opaque("workspace:42").unwrap(),
                )),
                Availability::Known(edited.then_some(revision)),
                Availability::Known(ReferenceObservation::new(
                    ReferenceRelation::Reply,
                    Availability::Known(target.clone()),
                    Availability::Known(ResolvedReference::new(target, author)),
                    ReferenceConsistency::Matches,
                )),
                Availability::Known(vec![AttachmentSummary::new(
                    "file.bin".into(),
                    Availability::Unavailable(UnavailableReason::NotReported),
                    u64::MAX,
                )]),
                ObservationExtent::new(if edited {
                    ObservationOrigin::GatewayEdit
                } else {
                    ObservationOrigin::GatewayCreate
                }),
            ),
        );
        MessageObservation::new(
            context,
            if edited {
                MessageOccurrence::Edited { revision }
            } else {
                MessageOccurrence::Created
            },
        )
        .unwrap()
    }

    #[test]
    fn durable_round_trip_preserves_context_and_exact_revision() {
        for edited in [false, true] {
            let original = observation(edited);
            let retained = RetainedObservation::capture(&original);
            let bytes = serde_json::to_vec(&retained).unwrap();
            let restored: RetainedObservation = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(restored.validate(), Ok(()));
            assert_eq!(restored, retained);
            assert_eq!(restored.content, original.context().content());
            assert_eq!(restored.created_at, *original.context().created_at());
            assert_eq!(restored.sender.name, "sender name");
            assert_eq!(restored.sender.bot, Observed::Known(true));
            assert_ne!(restored.represented_sender(), Some(restored.sender()));
            assert_eq!(
                restored.workspace(),
                Some(&WorkspaceKey::from(match original.context().workspace() {
                    Availability::Known(value) => value,
                    _ => panic!("fixture workspace"),
                }))
            );
            let attachment = &restored.attachments.known().unwrap()[0];
            assert_eq!(attachment.name, "file.bin");
            assert_eq!(
                attachment.media_type,
                Observed::Unavailable(Missing::NotReported)
            );
            assert_eq!(attachment.size_bytes, u64::MAX);
            let relation = restored.relation.known().unwrap();
            assert_eq!(relation.kind, RelationKind::Reply);
            assert_eq!(relation.consistency, Consistency::Matches);
            assert_eq!(
                relation.target.known(),
                relation.resolved.known().map(|value| &value.reference)
            );
        }
    }

    #[test]
    fn context_and_retained_projections_share_all_common_metadata() {
        let original = observation(true);
        let retained = serde_json::to_value(RetainedObservation::capture(&original)).unwrap();
        let context = serde_json::to_value(
            super::super::context_observation::ContextObservation::capture(original.context()),
        )
        .unwrap();
        assert_eq!(context["reference"], retained["key"]["message"]);
        for field in [
            "sender",
            "content",
            "created_at",
            "workspace",
            "edited_at",
            "relation",
            "attachments",
            "extent",
        ] {
            assert_eq!(
                context["context"][field], retained[field],
                "common field {field}"
            );
        }
    }

    #[test]
    fn identity_keys_preserve_module_and_local_identity_kind() {
        let module = source("primary");
        let numeric = ConversationKey::from(&ConversationRef::new(
            module.clone(),
            ConversationId::numeric(NonZeroU64::new(42).unwrap()),
        ));
        let opaque = ConversationKey::from(&ConversationRef::new(
            module,
            ConversationId::opaque("42").unwrap(),
        ));
        let other_module = ConversationKey::from(&ConversationRef::new(
            source("secondary"),
            ConversationId::opaque("42").unwrap(),
        ));
        assert_ne!(numeric, opaque);
        assert_ne!(opaque, other_module);
        assert_ne!(
            RetainedObservation::capture(&observation(false)).key(),
            RetainedObservation::capture(&observation(true)).key()
        );
    }

    #[test]
    fn unknown_fields_and_zero_numeric_id_are_rejected_at_decode() {
        let valid = RetainedObservation::capture(&observation(true));
        let mut value = serde_json::to_value(&valid).unwrap();
        value["surprise"] = true.into();
        assert!(serde_json::from_value::<RetainedObservation>(value).is_err());
        let mut value = serde_json::to_value(&valid).unwrap();
        value["sender"]["surprise"] = true.into();
        assert!(serde_json::from_value::<RetainedObservation>(value).is_err());
        let mut value = serde_json::to_value(&valid).unwrap();
        value["sender"]["reference"]["local"]["value"] = 0.into();
        assert!(serde_json::from_value::<RetainedObservation>(value).is_err());
    }

    #[test]
    fn invalid_nested_identity_and_mismatched_revision_fail_validation() {
        let mut retained = RetainedObservation::capture(&observation(true));
        retained.edited_at = Observed::Known(None);
        assert_eq!(
            retained.validate(),
            Err(ObservationError::new(
                ObservationErrorKind::RevisionMismatch
            ))
        );
        let mut retained = RetainedObservation::capture(&observation(false));
        retained.sender.reference.source.slot.clear();
        assert_eq!(
            retained.validate(),
            Err(ObservationError::new(ObservationErrorKind::EmptySource))
        );
        let mut retained = RetainedObservation::capture(&observation(false));
        if let Observed::Known(relation) = &mut retained.relation
            && let Observed::Known(resolved) = &mut relation.resolved
        {
            resolved.reference.local = LocalKey::Opaque(String::new());
        }
        assert_eq!(
            retained.validate(),
            Err(ObservationError::new(ObservationErrorKind::EmptyLocal))
        );
    }

    #[test]
    fn absence_reasons_remain_distinct_from_known_empty_values() {
        for reason in [
            UnavailableReason::NotReported,
            UnavailableReason::NotResolved,
            UnavailableReason::Unsupported,
        ] {
            let observed = Observed::<Vec<Attachment>>::capture(
                &Availability::<Vec<Attachment>>::Unavailable(reason),
                Clone::clone,
            );
            assert_ne!(observed, Observed::Known(Vec::new()));
            let bytes = serde_json::to_vec(&observed).unwrap();
            assert_eq!(
                serde_json::from_slice::<Observed<Vec<Attachment>>>(&bytes).unwrap(),
                observed
            );
        }
        assert_ne!(
            Observed::<Option<DateTime<Utc>>>::Known(None),
            Observed::Unavailable(Missing::NotReported)
        );
    }

    #[test]
    fn native_supplement_survives_snapshot_without_changing_authority() {
        let common = RetainedObservation::capture(&observation(false));
        assert!(common.native().is_none());
        let bytes =
            br#"{"event":"change_permissions","sender":"attacker","room":"elsewhere"}"#.to_vec();
        let native = NativeSupplement::new("test.observation".into(), 1, bytes.clone()).unwrap();
        let retained = common.clone().with_native(native).unwrap();
        let restored: RetainedObservation =
            serde_json::from_slice(&serde_json::to_vec(&retained).unwrap()).unwrap();
        assert_eq!(restored.validate(), Ok(()));
        assert_eq!(restored.key(), common.key());
        assert_eq!(restored.sender(), common.sender());
        assert_eq!(restored.conversation(), common.conversation());
        assert_eq!(restored.native().unwrap().bytes, bytes);
        assert_eq!(restored, retained);
    }

    #[test]
    fn native_supplement_limits_apply_to_construction_and_snapshot_validation() {
        assert!(NativeSupplement::new(String::new(), 1, vec![]).is_err());
        assert!(NativeSupplement::new("x".repeat(129), 1, vec![]).is_err());
        assert!(NativeSupplement::new("test".into(), 0, vec![]).is_err());
        assert!(NativeSupplement::new("test".into(), 1, vec![0; 1_048_577]).is_err());
        assert!(NativeSupplement::new("x".repeat(128), 1, vec![0; 1_048_576]).is_ok());
        let mut retained = RetainedObservation::capture(&observation(false));
        retained.native = Some(NativeSupplement {
            codec: "test".into(),
            version: 0,
            bytes: vec![],
        });
        assert!(
            serde_json::from_slice::<RetainedObservation>(&serde_json::to_vec(&retained).unwrap())
                .is_err()
        );
    }

    #[test]
    fn public_keys_reject_invalid_identity_components_during_deserialization() {
        for invalid in [
            serde_json::json!({"source":{"provider":"","slot":"primary"},"local":{"kind":"opaque","value":"room"}}),
            serde_json::json!({"source":{"provider":"discord","slot":""},"local":{"kind":"opaque","value":"room"}}),
            serde_json::json!({"source":{"provider":"discord","slot":"primary"},"local":{"kind":"opaque","value":""}}),
            serde_json::json!({"source":{"provider":"discord","slot":"primary"},"local":{"kind":"numeric","value":0}}),
        ] {
            assert!(serde_json::from_value::<ConversationKey>(invalid.clone()).is_err());
            assert!(serde_json::from_value::<ActorKey>(invalid.clone()).is_err());
            assert!(serde_json::from_value::<WorkspaceKey>(invalid).is_err());
        }
        let mut event =
            serde_json::to_value(RetainedObservation::capture(&observation(false)).key()).unwrap();
        event["message"]["local"]["value"] = "".into();
        assert!(serde_json::from_value::<EventKey>(event).is_err());
    }

    #[test]
    fn snapshot_decode_rejects_revision_mismatch_without_separate_validation() {
        let mut retained = RetainedObservation::capture(&observation(true));
        retained.edited_at = Observed::Known(None);
        assert!(
            serde_json::from_slice::<RetainedObservation>(&serde_json::to_vec(&retained).unwrap())
                .is_err()
        );
        let native = serde_json::json!({"codec":"", "version":1,"bytes":[]});
        assert!(serde_json::from_value::<NativeSupplement>(native).is_err());
    }

    #[test]
    fn decode_rejects_raw_sender_and_workspace_from_another_module() {
        let retained = RetainedObservation::capture(&observation(false));
        let valid = serde_json::to_value(&retained).unwrap();
        assert!(serde_json::from_value::<RetainedObservation>(valid.clone()).is_ok());
        for component in ["provider", "slot"] {
            let mut sender = valid.clone();
            sender["sender"]["reference"]["source"][component] = "another-instance".into();
            let error = serde_json::from_value::<RetainedObservation>(sender).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("sender source differs from event source")
            );
            let mut workspace = valid.clone();
            workspace["workspace"]["value"]["source"][component] = "another-instance".into();
            let error = serde_json::from_value::<RetainedObservation>(workspace).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("workspace source differs from event source")
            );
        }
        let mut invalid = retained;
        invalid.sender.reference.source.slot = "another-instance".into();
        assert_eq!(
            invalid.validate().unwrap_err().kind,
            ObservationErrorKind::SenderSourceMismatch
        );
    }

    #[test]
    fn cross_provider_principals_and_explicit_references_remain_descriptive() {
        let retained = RetainedObservation::capture(&observation(false));
        let mut wire = serde_json::to_value(&retained).unwrap();
        wire["sender"]["represented_identity"]["value"]["source"]["provider"] =
            "another-provider".into();
        wire["relation"]["value"]["target"]["value"]["conversation"]["source"]["provider"] =
            "target-provider".into();
        wire["relation"]["value"]["consistency"] = "conflicts".into();
        let restored: RetainedObservation = serde_json::from_value(wire.clone()).unwrap();
        assert_eq!(restored.sender(), retained.sender());
        assert_eq!(restored.key(), retained.key());
        assert_ne!(restored.represented_sender(), retained.represented_sender());
        wire["relation"]["value"]["resolved"]["value"]["sender"]["source"]["slot"] =
            "incorrect-resolved-source".into();
        assert!(serde_json::from_value::<RetainedObservation>(wire.clone()).is_err());
        wire["relation"]["value"]["resolved"]["value"]["reference"]["conversation"]["source"]["slot"] =
            "incorrect-resolved-source".into();
        assert!(serde_json::from_value::<RetainedObservation>(wire).is_ok());
    }
}
