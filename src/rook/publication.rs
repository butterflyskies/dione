//! One durable text-publication slot per recipient and source occurrence.

use super::{ActorKey, ConversationKey, EventKey, MessageKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{collections::BTreeSet, future::Future, pin::Pin};
use thiserror::Error;

pub(super) const COMPLETION_HEADROOM: usize = 8192;
pub(super) const MAX_ALIASES: usize = 64;
const MAX_RECEIPT_BYTES: usize = 4096;
const MAX_PREPARED_BYTES: usize = 65_536;

/// A recipient-scoped retry identity. Its spelling is significant.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct IdempotencyKey(String);
impl IdempotencyKey {
    /// Validate an ASCII key containing between one and 128 visible characters.
    pub fn new(value: String) -> Result<Self, PublicationError> {
        if value.is_empty() || value.len() > 128 || !value.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(PublicationError::new(PublicationErrorKind::InvalidRequest));
        }
        Ok(Self(value))
    }
}
impl TryFrom<String> for IdempotencyKey {
    type Error = PublicationError;
    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}
impl From<IdempotencyKey> for String {
    fn from(value: IdempotencyKey) -> Self {
        value.0
    }
}

/// The exact destination. Both variants remain inside the admitted event's room.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "target",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum PublicationTarget {
    /// Send a new message into the verified room.
    Room(ConversationKey),
    /// Reply to this exact message regardless of legacy reply-mode settings.
    Reply(MessageKey),
}
impl PublicationTarget {
    /// The source-qualified room containing the target.
    pub fn room(&self) -> &ConversationKey {
        match self {
            Self::Room(room) => room,
            Self::Reply(message) => message.conversation(),
        }
    }
}

/// Explicit mention permission; message text never supplies permission.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum MentionPolicy {
    /// Disable user, role, everyone, and reply-author notifications.
    #[default]
    SuppressAll,
    /// Permit only the named users and the separately selected reply-author ping.
    ExplicitUsers {
        /// At most 100 distinct actors from the destination module.
        users: Vec<ActorKey>,
        /// Whether replying may notify the author of the exact target.
        reply_author: bool,
    },
}

/// A text-only request. Attachments and additional publication purposes are unsupported.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextPublication {
    /// Source occurrence whose sole default-reply slot owns this publication.
    pub event: EventKey,
    /// Retry identity scoped to the authenticated recipient.
    pub key: IdempotencyKey,
    /// Exact room or reply destination.
    pub target: PublicationTarget,
    /// Unmodified requested text, bounded to 32 KiB before provider preparation.
    pub text: String,
    /// Deliberate mention permission; omission suppresses all mentions.
    #[serde(default)]
    pub mentions: MentionPolicy,
}

/// Versioned request retained independently of its retry aliases.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "CanonicalPublicationWire")]
pub struct CanonicalPublication {
    version: u8,
    event: EventKey,
    target: PublicationTarget,
    text: String,
    mentions: MentionPolicy,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CanonicalPublicationWire {
    version: u8,
    event: EventKey,
    target: PublicationTarget,
    text: String,
    mentions: MentionPolicy,
}
impl TryFrom<CanonicalPublicationWire> for CanonicalPublication {
    type Error = PublicationError;
    fn try_from(wire: CanonicalPublicationWire) -> Result<Self, Self::Error> {
        let value = Self {
            version: wire.version,
            event: wire.event,
            target: wire.target,
            text: wire.text,
            mentions: wire.mentions,
        };
        value.validate()?;
        Ok(value)
    }
}
impl CanonicalPublication {
    pub(super) fn from_request(
        request: TextPublication,
    ) -> Result<(Self, IdempotencyKey), PublicationError> {
        let canonical = Self {
            version: 1,
            event: request.event,
            target: request.target,
            text: request.text,
            mentions: request.mentions,
        };
        canonical.validate()?;
        Ok((canonical, request.key))
    }
    /// The retained occurrence that authorizes this slot.
    pub fn event(&self) -> &EventKey {
        &self.event
    }
    /// The exact immutable destination.
    pub fn target(&self) -> &PublicationTarget {
        &self.target
    }
    /// Original text before adapter policy and rewrites.
    pub fn text(&self) -> &str {
        &self.text
    }
    /// Explicit mention permission.
    pub fn mentions(&self) -> &MentionPolicy {
        &self.mentions
    }
    pub(super) fn validate(&self) -> Result<(), PublicationError> {
        let invalid = || PublicationError::new(PublicationErrorKind::InvalidRequest);
        self.event.validate().map_err(|_| invalid())?;
        self.target.room().validate().map_err(|_| invalid())?;
        if let PublicationTarget::Reply(message) = &self.target {
            message.validate().map_err(|_| invalid())?;
        }
        if self.version != 1
            || self.text.is_empty()
            || self.text.len() > 32_768
            || self.target.room() != self.event.message().conversation()
        {
            return Err(invalid());
        }
        if let MentionPolicy::ExplicitUsers {
            users,
            reply_author,
        } = &self.mentions
            && (users.len() > 100
                || users.iter().collect::<BTreeSet<_>>().len() != users.len()
                || (*reply_author && !matches!(self.target, PublicationTarget::Reply(_)))
                || users.iter().any(|actor| {
                    actor.validate().is_err() || !actor.same_source(self.target.room())
                }))
        {
            return Err(invalid());
        }
        if serde_json::to_vec(self)
            .map_err(PublicationError::encoding)?
            .len()
            > 131_072
        {
            return Err(invalid());
        }
        Ok(())
    }
    pub(super) fn digest(&self) -> Result<String, PublicationError> {
        let mut hash = Sha256::new();
        hash.update(b"dione.rook.text-publication.v1\0");
        hash.update(serde_json::to_vec(self).map_err(PublicationError::encoding)?);
        Ok(format!("{:x}", hash.finalize()))
    }
}

/// Immutable adapter preparation. Its bytes must contain no credentials.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "PreparedPublicationWire")]
pub struct PreparedPublication {
    request: CanonicalPublication,
    codec: String,
    version: u16,
    payload: Vec<u8>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreparedPublicationWire {
    request: CanonicalPublication,
    codec: String,
    version: u16,
    payload: Vec<u8>,
}
impl TryFrom<PreparedPublicationWire> for PreparedPublication {
    type Error = PublicationError;
    fn try_from(wire: PreparedPublicationWire) -> Result<Self, Self::Error> {
        Self::new(wire.request, wire.codec, wire.version, wire.payload)
    }
}
impl PreparedPublication {
    /// Retain the exact bounded adapter request that will be dispatched once.
    pub fn new(
        request: CanonicalPublication,
        codec: String,
        version: u16,
        payload: Vec<u8>,
    ) -> Result<Self, PublicationError> {
        let value = Self {
            request,
            codec,
            version,
            payload,
        };
        value.validate()?;
        Ok(value)
    }
    /// The original request whose authority and identity remain unchanged.
    pub fn request(&self) -> &CanonicalPublication {
        &self.request
    }
    /// Adapter codec name.
    pub fn codec(&self) -> &str {
        &self.codec
    }
    /// Explicit adapter codec version.
    pub fn version(&self) -> u16 {
        self.version
    }
    /// Exact prepared adapter bytes.
    pub fn payload(&self) -> &[u8] {
        &self.payload
    }
    pub(super) fn validate(&self) -> Result<(), PublicationError> {
        self.request.validate()?;
        if self.codec.is_empty()
            || self.codec.len() > 64
            || self.version == 0
            || self.payload.len() > MAX_PREPARED_BYTES
        {
            return Err(PublicationError::new(
                PublicationErrorKind::InvalidPreparation,
            ));
        }
        Ok(())
    }
}

/// Authoritative exact message identity returned by a provider.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublicationReceipt {
    /// Exact source-qualified message created by this operation.
    pub message: MessageKey,
}
impl PublicationReceipt {
    pub(super) fn valid_for(&self, request: &CanonicalPublication) -> bool {
        self.message.validate().is_ok()
            && self.message.conversation() == request.target.room()
            && serde_json::to_vec(self).is_ok_and(|bytes| bytes.len() <= MAX_RECEIPT_BYTES)
    }
}

/// Finite actionable refusal codes, without untrusted provider bodies or secrets.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationRejection {
    /// The adapter or operation is unsupported.
    Unsupported,
    /// Current destination or recipient access was revoked.
    AccessDenied,
    /// Current access could not be established.
    AccessUnavailable,
    /// The grant expired or policy revision changed before dispatch.
    AuthorityChanged,
    /// The exact target could not be verified.
    InvalidTarget,
    /// Policy or a pre-send hook refused this content.
    Policy,
    /// Prepared text exceeds the provider's one-message limit.
    TooLarge,
    /// Coordination did not grant this operation authority.
    Coordination,
    /// No message POST occurred, but coordinator acquisition may remain held.
    CoordinationUncertain,
    /// The provider explicitly refused creation, such as a validated 403 or 429.
    ProviderRefused,
    /// Preparation timed out before any message POST.
    PreparationTimeout,
    /// The adapter returned invalid preparation.
    InvalidPreparation,
}

/// Why a released attempt requires investigation and cannot be retried.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationUncertainty {
    /// An interrupted process retained dispatch authority without a receipt.
    Restarted,
    /// The adapter did not return within the bounded attempt lifetime.
    Timeout,
    /// The transport cannot prove whether creation occurred.
    Transport,
    /// A successful response lacked a valid matching message identity.
    InvalidReceipt,
    /// The worker stopped before returning an outcome.
    WorkerFailed,
}

/// Distinguishes a local refusal from a request rejected by the provider.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RejectionPhase {
    /// No message POST was attempted.
    BeforeDispatch,
    /// The provider returned an authoritative non-creation response.
    ProviderRefused,
}

/// Durable progress of the sole default-reply publication slot.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublicationState {
    /// Accepted and retained, with no POST released.
    Reserved,
    /// Durable permission for exactly one POST, whose outcome is outstanding.
    Dispatching,
    /// The provider returned this exact message identity.
    Published {
        /// Authoritative message identity.
        receipt: PublicationReceipt,
    },
    /// A definite refusal. This first version never retries refused attempts.
    Rejected {
        /// Whether a POST was attempted.
        phase: RejectionPhase,
        /// Finite refusal classification.
        reason: PublicationRejection,
    },
    /// Creation may have occurred. Retry cannot release another POST.
    Unknown {
        /// Investigation requirement.
        reason: PublicationUncertainty,
    },
}

/// Why an accepted operation cannot currently commit its next durable state.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublicationBlock {
    /// Repair storage availability or synchronization before progress can resume.
    Storage,
    /// Increase the snapshot capacity without discarding retained publication identities.
    Capacity,
    /// The owner or adapter produced inconsistent state requiring investigation.
    InvalidState,
}

/// Inspectable progress and whether its latest outcome still needs disk synchronization.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PublicationStatus {
    /// Current progress, including any completion retained in memory after a disk failure.
    pub outcome: PublicationState,
    /// A visible or in-memory completion has not yet been durably synchronized.
    pub durability_pending: bool,
    /// Actionable reason a retained transition cannot currently commit.
    pub blocked: Option<PublicationBlock>,
}

/// Advertised limits of the installed text-only adapter.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct PublicationCapabilities {
    /// Whether exact text publication is installed.
    pub text: bool,
    /// Maximum requested text bytes, before the native one-message limit.
    pub max_text_bytes: usize,
    /// Whether uploads are supported. False for this text-only version.
    pub attachments: bool,
}

/// Result of one released provider attempt. Ambiguous failures cannot mean refusal.
#[derive(Clone, Debug)]
pub enum DispatchOutcome {
    /// Exact provider receipt.
    Published(PublicationReceipt),
    /// The final pre-POST policy check refused dispatch.
    NotSent(PublicationRejection),
    /// An authoritative provider response proves refusal.
    Refused(PublicationRejection),
    /// Creation may have occurred.
    Unknown(PublicationUncertainty),
}

type PublicationFollowUp = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// A provider outcome with optional secondary work that cannot change its receipt.
///
/// The owner starts the owned follow-up only after a valid receipt or definite refusal commits.
/// Crashes may interrupt secondary bookkeeping; the publication slot remains terminal.
pub struct PublicationDispatch {
    /// The provider's message-creation outcome, independent of ancillary work.
    pub outcome: DispatchOutcome,
    after_commit: Option<PublicationFollowUp>,
}
impl PublicationDispatch {
    /// Report a result without secondary bookkeeping.
    pub fn new(outcome: DispatchOutcome) -> Self {
        Self {
            outcome,
            after_commit: None,
        }
    }
    /// Capture exact adapter context for bounded work after a definite outcome becomes durable.
    ///
    /// Follow-ups attached to an unknown outcome are discarded. They must
    /// never send another message or attempt to recover an uncertain publication.
    pub fn with_after_commit(outcome: DispatchOutcome, future: PublicationFollowUp) -> Self {
        Self {
            outcome,
            after_commit: Some(future),
        }
    }
    pub(crate) fn into_parts(self) -> (DispatchOutcome, Option<PublicationFollowUp>) {
        (self.outcome, self.after_commit)
    }
}

/// Trusted adapter boundary. Preparation performs no message POST; dispatch performs at most one.
pub trait PublicationProvider: Send + Sync + 'static {
    /// Explicit provider support and limits.
    fn capabilities(&self) -> PublicationCapabilities;
    /// Run current policy and produce exact immutable dispatch bytes without posting.
    fn prepare<'a>(
        &'a self,
        request: &'a CanonicalPublication,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedPublication, PublicationRejection>> + Send + 'a>>;
    /// Recheck current native access and perform one POST with retries and redirects disabled.
    fn dispatch<'a>(
        &'a self,
        prepared: &'a PreparedPublication,
    ) -> Pin<Box<dyn Future<Output = PublicationDispatch> + Send + 'a>>;
}

/// Request validation or idempotency conflict at the publication boundary.
#[derive(Debug, Error)]
#[error("publication operation failed: {kind}")]
pub struct PublicationError {
    /// Typed reason and preserved encoding cause when applicable.
    #[source]
    pub kind: PublicationErrorKind,
}
impl PublicationError {
    pub(super) fn new(kind: PublicationErrorKind) -> Self {
        Self { kind }
    }
    fn encoding(error: serde_json::Error) -> Self {
        Self::new(PublicationErrorKind::Encoding(error))
    }
}
/// Publication failures that do not authorize another message POST.
#[derive(Debug, Error)]
pub enum PublicationErrorKind {
    /// No supported adapter is installed.
    #[error("publication is unsupported")]
    Unsupported,
    /// The request violates text, identity, or mention bounds.
    #[error("invalid publication request")]
    InvalidRequest,
    /// An adapter produced an invalid or oversized prepared request.
    #[error("invalid prepared publication")]
    InvalidPreparation,
    /// This retry key already identifies a different request or occurrence.
    #[error("idempotency key conflicts with a retained publication")]
    IdempotencyConflict,
    /// The event's sole reply slot already contains a different request.
    #[error("publication conflicts with the retained event slot")]
    PublicationConflict,
    /// Supplied tool arguments failed typed, path-aware decoding.
    #[error("publication input decoding failed")]
    Decode(#[source] serde_path_to_error::Error<serde_json::Error>),
    /// Typed serialization failed.
    #[error("publication encoding failed")]
    Encoding(#[source] serde_json::Error),
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PublicationRecord {
    pub(super) request: CanonicalPublication,
    pub(super) digest: String,
    pub(super) aliases: BTreeSet<IdempotencyKey>,
    pub(super) prepared: Option<PreparedPublication>,
    pub(super) state: PublicationState,
}
impl PublicationRecord {
    pub(super) fn headroom(&self) -> usize {
        if matches!(
            self.state,
            PublicationState::Reserved | PublicationState::Dispatching
        ) {
            COMPLETION_HEADROOM
        } else {
            0
        }
    }
    pub(super) fn validate(&self) -> Result<(), PublicationError> {
        self.request.validate()?;
        if self.digest != self.request.digest()?
            || self.aliases.is_empty()
            || self.aliases.len() > MAX_ALIASES
        {
            return Err(PublicationError::new(PublicationErrorKind::InvalidRequest));
        }
        if let Some(prepared) = &self.prepared {
            prepared.validate()?;
            if prepared.request != self.request {
                return Err(PublicationError::new(
                    PublicationErrorKind::InvalidPreparation,
                ));
            }
        }
        let valid = match &self.state {
            PublicationState::Reserved => self.prepared.is_none(),
            PublicationState::Dispatching | PublicationState::Unknown { .. } => {
                self.prepared.is_some()
            }
            PublicationState::Published { receipt } => {
                self.prepared.is_some() && receipt.valid_for(&self.request)
            }
            PublicationState::Rejected {
                phase: RejectionPhase::ProviderRefused,
                ..
            } => self.prepared.is_some(),
            PublicationState::Rejected {
                phase: RejectionPhase::BeforeDispatch,
                ..
            } => true,
        };
        if !valid {
            return Err(PublicationError::new(
                PublicationErrorKind::InvalidPreparation,
            ));
        }
        Ok(())
    }
}
