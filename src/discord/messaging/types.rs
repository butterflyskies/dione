//! Domain outcomes for Discord publication and typed preparation failures.
use super::queries::ObservationError;
use crate::{
    contradictionary::AutoRewrite,
    evidence::SentexLocator,
    no_rly::{
        consent::{BounceTicket, RejectedHandle},
        delivery::{DeliveryCause, PostedMessages},
    },
    pre_send::PipelineError,
};
use serenity::model::id::{ChannelId, MessageId};
use std::{fmt, io};
use thiserror::Error;

/// Delivery-stage outcome and the rewrites applied before that stage.
#[derive(Debug)]
pub struct Report<T> {
    /// The operation outcome.
    pub outcome: T,
    /// Rewrites visible to the sender.
    pub auto_rewrites: Vec<AutoRewrite>,
}
/// A completed logical publication, including an empty publication.
#[derive(Debug)]
pub struct DeliveryReceipt {
    pub(crate) message_ids: Vec<MessageId>,
    pub(crate) locators: Vec<SentexLocator>,
}
impl DeliveryReceipt {
    /// Published message IDs in order.
    pub fn message_ids(&self) -> &[MessageId] {
        &self.message_ids
    }
}
/// A publication that stopped after at least one message.
#[derive(Debug)]
pub struct PartialReceipt {
    /// Messages published by this attempt.
    pub sent: PostedMessages,
    /// Why the remaining content was not published.
    pub cause: DeliveryCause,
}
/// The outcome of a channel reply.
#[derive(Debug)]
pub enum ReplyOutcome {
    /// All chunks were delivered.
    Delivered(DeliveryReceipt),
    /// Consent is required before delivery.
    Held(BounceTicket),
    /// Some chunks reached Discord.
    PartiallyDelivered(PartialReceipt),
}
/// The route used for a completed DM request.
#[derive(Debug)]
pub enum DmDeliveryRoute {
    /// Delivered in the recipient's DM channel.
    Direct(ChannelId),
    /// A hook redirected the request to a channel.
    Redirected,
}
/// The outcome of a DM request.
#[derive(Debug)]
pub enum DmOutcome {
    /// All chunks were delivered on the specified route.
    Delivered {
        /// Messages and evidence published by the operation.
        delivery: DeliveryReceipt,
        /// The destination selected by preparation.
        route: DmDeliveryRoute,
    },
    /// Consent is required before delivery.
    Held(BounceTicket),
    /// Some chunks reached Discord.
    PartiallyDelivered(PartialReceipt),
}
impl Report<ReplyOutcome> {
    pub(crate) fn dm(self, route: DmDeliveryRoute) -> Report<DmOutcome> {
        Report {
            auto_rewrites: self.auto_rewrites,
            outcome: match self.outcome {
                ReplyOutcome::Delivered(delivery) => DmOutcome::Delivered { delivery, route },
                ReplyOutcome::Held(ticket) => DmOutcome::Held(ticket),
                ReplyOutcome::PartiallyDelivered(partial) => DmOutcome::PartiallyDelivered(partial),
            },
        }
    }
}
/// An outbound failure with delivery-stage rewrite context.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct MessagingError {
    /// The structured failure cause.
    #[source]
    pub kind: MessagingErrorKind,
    /// Rewrites applied at the delivery stage.
    pub auto_rewrites: Vec<AutoRewrite>,
}
impl From<MessagingErrorKind> for MessagingError {
    fn from(kind: MessagingErrorKind) -> Self {
        Self {
            kind,
            auto_rewrites: Vec::new(),
        }
    }
}
impl From<TargetError> for MessagingError {
    fn from(cause: TargetError) -> Self {
        MessagingErrorKind::Target(cause).into()
    }
}
/// A redirect prohibited by the operation's semantics.
#[derive(Debug, Clone, Copy)]
pub enum RedirectRestrictedSurface {
    /// An edit cannot move a message.
    Edit,
    /// Voice output cannot change channels during preparation.
    Voice,
}
impl fmt::Display for RedirectRestrictedSurface {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Edit => "message edits",
            Self::Voice => "voice output",
        })
    }
}
/// Failures of outbound preparation, delivery, and legacy Discord queries.
#[derive(Debug, Error)]
pub enum MessagingErrorKind {
    /// Discord returned an invalid observation.
    #[error(transparent)]
    Observation(#[from] ObservationError),
    /// Current outbound policy denies the channel.
    #[error("channel {0} is not a permitted outbound target")]
    OutboundDenied(ChannelId),
    /// Message identity could not be verified.
    #[error(transparent)]
    Target(#[from] TargetError),
    /// A pre-send hook failed.
    #[error(transparent)]
    Pipeline(#[from] PipelineError),
    /// The blocking hook task failed.
    #[error("pre-send pipeline task failed: {0}")]
    PipelineTask(#[source] tokio::task::JoinError),
    /// Overrides named hooks without an installed pipeline.
    #[error("no_rly_hooks named hooks, but no pre-send hooks are registered")]
    HooksNotRegistered,
    /// A hook declined publication with construct feedback.
    #[error("{reason}")]
    HookHalted {
        /// The hook's explanation.
        reason: String,
        /// Feedback supplied to the construct, including an empty list.
        feedback: Vec<String>,
    },
    /// This operation cannot follow a redirect.
    #[error("pre-send redirect is not supported for {0}")]
    RedirectUnsupported(RedirectRestrictedSurface),
    /// Hook overrides require a caption.
    #[error("no_rly_hooks cannot be used when no caption is sent")]
    CaptionlessHookOverrides,
    /// Raw locators must be supplied as structured evidence.
    #[error(
        "raw terminal sentex locators are not accepted in content; use structured handles on reply or send_dm"
    )]
    RawSentexLocators,
    /// Evidence could not be appended outside quoted text.
    #[error("sentex locators cannot be attached inside quoted or fenced content")]
    SentexInsideQuoteOrFence,
    /// Evidence needs a single message.
    #[error("sentex-bearing messages must fit in one Discord message")]
    SentexRequiresSingleMessage,
    /// Evidence cannot enter the consent lifecycle.
    #[error(
        "sentex-bearing messages cannot enter the no_rly hold lifecycle; revise and send a fresh sentex-bearing reply"
    )]
    SentexCannotBeHeld,
    /// Initiating DMs is disabled.
    #[error("dm_policy is set to disabled; cannot initiate DMs")]
    DmDisabled,
    /// Discord failed to create the recipient's DM channel.
    #[error("failed to create DM channel: {0}")]
    CreateDm(#[source] Box<serenity::Error>),
    /// The coordinator has already erased its transport error into a reason.
    #[error("claim-once unavailable and fail-closed: {0}")]
    CoordinationUnavailable(String),
    /// Another construct owns the answer claim.
    #[error("claim-once: {winner_display} is already answering this message")]
    CoordinationBusy {
        /// Constructs ahead of this requester.
        ahead: Vec<String>,
        /// Cache-only human display of the first winner; not an authority ID.
        winner_display: String,
    },
    /// Delivery failed before any message was published.
    #[error(transparent)]
    Delivery(#[from] DeliveryCause),
    /// A replacement requires non-whitespace text.
    #[error("rephrase content must not be empty")]
    EmptyRephrase,
    /// Consent rejected the requested handle action.
    #[error(transparent)]
    Handle(#[from] RejectedHandle),
    /// Discord rejected an operation.
    #[error(transparent)]
    Discord(#[from] Box<serenity::Error>),
    /// An attachment could not be sent.
    #[error("failed to send: {0}")]
    SendAttachment(#[source] Box<serenity::Error>),
    /// A file could not be read.
    #[error("failed to read file: {0}")]
    ReadFile(#[source] Box<serenity::Error>),
    /// A query's target could not be fetched.
    #[error("failed to fetch message: {0}")]
    FetchMessage(#[source] Box<serenity::Error>),
    /// The inbox directory could not be created.
    #[error("failed to create inbox: {0}")]
    CreateInbox(#[source] io::Error),
    /// Pagination directions are mutually exclusive.
    #[error("cannot specify both 'before' and 'after'")]
    ConflictingPagination,
    /// The message contains no attachments.
    #[error("message has no attachments")]
    NoAttachments,
    /// Upload paths must be absolute.
    #[error("file_path must be absolute")]
    RelativeFilePath,
    /// Upload policy rejected the file.
    #[error("file_path is not permitted for upload")]
    FileNotPermitted,
    /// Custom emoji snowflakes cannot be zero.
    #[error("invalid custom emoji {0:?}: emoji ID must be nonzero")]
    ZeroEmojiId(String),
}
/// The Discord mutation requiring target verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetOperation {
    /// A reply reference.
    ReplyTo,
    /// A reaction target.
    React,
    /// A message edit.
    EditMessage,
    /// Message deletion.
    DeleteMessage,
    /// Message pinning.
    PinMessage,
    /// Message unpinning.
    UnpinMessage,
    /// A thread starting from a message.
    CreateThread,
}
impl TargetOperation {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::ReplyTo => "reply_to",
            Self::React => "react",
            Self::EditMessage => "edit_message",
            Self::DeleteMessage => "delete_message",
            Self::PinMessage => "pin_message",
            Self::UnpinMessage => "unpin_message",
            Self::CreateThread => "create_thread",
        }
    }
}
impl fmt::Display for TargetOperation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
/// Failure to establish a permitted message target.
#[derive(Debug, Error)]
pub enum TargetError {
    /// Management requires a receipt or an authenticated own send.
    #[error("message target requires an ingress receipt or own send")]
    IngressRequired,
    /// Canonical existence alone does not establish audience admission.
    #[error("target requires an admitted ingress receipt for this audience")]
    CanonicalRequiresIngress,
    /// Discord returned a different target.
    #[error("canonical message did not match the requested target")]
    CanonicalMismatch,
    /// The target's guild could not be resolved.
    #[error("could not verify target guild: {0}")]
    GuildLookup(#[source] Box<serenity::Error>),
    /// The target could not be fetched.
    #[error("could not verify message target: {0}")]
    MessageLookup(#[source] Box<serenity::Error>),
    /// Discord confirmed the target does not exist.
    #[error("message {message} was not found by Discord in channel {channel}; {operation} blocked")]
    NotFound {
        /// The requested message.
        message: MessageId,
        /// The requested channel.
        channel: ChannelId,
        /// The blocked mutation.
        operation: TargetOperation,
    },
    /// A known message belongs to another channel.
    #[error(
        "message {message} was admitted in channel {admitted}, not claimed channel {claimed}; {operation} blocked"
    )]
    ChannelMismatch {
        /// The requested message.
        message: MessageId,
        /// The channel recorded by ingress.
        admitted: crate::ingress_ledger::ChannelRef,
        /// The channel claimed by the caller.
        claimed: crate::ingress_ledger::ChannelRef,
        /// The blocked mutation.
        operation: TargetOperation,
    },
}

/// Whether a reply may notify the referenced message's author.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplyPing {
    /// Permit the reply notification.
    Allow,
    /// Suppress the reply notification.
    Suppress,
}
impl ReplyPing {
    /// Convert an external suppression flag into reply policy.
    pub const fn from_suppressed(suppress: bool) -> Self {
        if suppress {
            Self::Suppress
        } else {
            Self::Allow
        }
    }
    pub(crate) const fn suppressed(self) -> bool {
        matches!(self, Self::Suppress)
    }
}
/// Whether Discord may expand links into previews.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkPreviews {
    /// Permit previews.
    Allow,
    /// Suppress previews.
    Suppress,
}
impl LinkPreviews {
    /// Convert an external suppression flag into preview policy.
    pub const fn from_suppressed(suppress: bool) -> Self {
        if suppress {
            Self::Suppress
        } else {
            Self::Allow
        }
    }
    pub(crate) const fn suppressed(self) -> bool {
        matches!(self, Self::Suppress)
    }
}

impl From<ObservationError> for MessagingError {
    fn from(source: ObservationError) -> Self {
        MessagingErrorKind::Observation(source).into()
    }
}
