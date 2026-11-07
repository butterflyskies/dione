//! Observed provider events before host presentation.

use dione_contracts::{ActorRef, ConversationRef, MessageRef};
use thiserror::Error;
use {
    crate::timestamp::Timestamp,
    dione_evidence::locators::{SentexLocator, parse_sentex_locators},
};

/// Exact attachment size, including empty files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ByteSize(u64);
impl ByteSize {
    /// Retains an exact byte count, including zero.
    pub fn new(bytes: u64) -> Self {
        Self(bytes)
    }
    /// Returns the retained byte count.
    pub fn get(self) -> u64 {
        self.0
    }
}

/// Original text and the terminal locators parsed from that same observation.
#[derive(Clone, Debug)]
pub struct ObservedText {
    text: String,
    locators: Vec<SentexLocator>,
}
impl ObservedText {
    /// Retains the original text and parses terminal locators from those same bytes.
    pub fn new(text: String) -> Self {
        Self {
            locators: parse_sentex_locators(&text),
            text,
        }
    }
    /// Borrows the original text without presentation changes.
    pub fn text(&self) -> &str {
        &self.text
    }
    /// Borrows the terminal locators parsed when this observation was constructed.
    pub fn locators(&self) -> &[SentexLocator] {
        &self.locators
    }
}

/// Transport-origin classification supplied with an observed author.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthorOrigin {
    /// No transport-origin classification was supplied.
    Unknown,
    /// The observer classified the direct transport author as human.
    DirectHuman,
    /// The observer classified the direct transport author as a bot.
    DirectBot,
    /// The observer supplied verified webhook provenance.
    VerifiedWebhook,
}
/// The supplied reason a message was directed toward the construct.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Targeting {
    /// The observation came from a direct conversation.
    DirectMessage,
    /// The construct was mentioned directly.
    DirectMention,
    /// The message replies to the construct.
    ReplyToConstruct,
    /// The message matched a configured targeting pattern.
    ConfiguredPattern,
    /// No direct targeting reason was supplied.
    Ambient,
}
/// Reported outcome of contextual bell augmentation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BellStatus {
    /// Bell augmentation reported success.
    Ok,
    /// Bell augmentation reported partial results and a timeout.
    PartialTimeout,
    /// Bell augmentation reported partial results and an error.
    PartialError,
    /// Bell augmentation reported a timeout.
    Timeout,
    /// Bell augmentation reported an error.
    Error,
}
impl BellStatus {
    /// Returns the existing host-facing status spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::PartialTimeout => "partial_timeout",
            Self::PartialError => "partial_error",
            Self::Timeout => "timeout",
            Self::Error => "error",
        }
    }
}

/// Caller-supplied actor identity, display name, and transport classification.
#[derive(Clone, Debug)]
pub struct ObservedAuthor {
    actor: ActorRef,
    name: String,
    origin: AuthorOrigin,
}
impl ObservedAuthor {
    /// Retains the supplied actor, name, and origin without authenticating them.
    pub fn new(actor: ActorRef, name: String, origin: AuthorOrigin) -> Self {
        Self {
            actor,
            name,
            origin,
        }
    }
    /// Borrows the supplied source-qualified actor identity.
    pub fn actor(&self) -> &ActorRef {
        &self.actor
    }
    /// Borrows the observed display name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Returns the recorded transport-origin classification.
    pub fn origin(&self) -> AuthorOrigin {
        self.origin
    }
}
/// Descriptive attachment metadata without fetching or retaining file contents.
#[derive(Clone, Debug)]
pub struct ObservedAttachment {
    name: String,
    media_type: Option<String>,
    size: ByteSize,
}
impl ObservedAttachment {
    /// Retains the supplied attachment name, media type, and exact size.
    pub fn new(name: String, media_type: Option<String>, size: ByteSize) -> Self {
        Self {
            name,
            media_type,
            size,
        }
    }
    /// Borrows the attachment name captured by the observer.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Borrows the supplied media type when available.
    pub fn media_type(&self) -> Option<&str> {
        self.media_type.as_deref()
    }
    /// Returns the retained exact byte count.
    pub fn size(&self) -> ByteSize {
        self.size
    }
}
/// Optional reply metadata retained from the source observation.
#[derive(Clone, Debug, Default)]
pub struct ReplyObservation {
    target: Option<MessageRef>,
    author: Option<ActorRef>,
    name: Option<String>,
    preview: Option<String>,
}
impl ReplyObservation {
    /// Retains the supplied optional target, author, name, and preview independently.
    pub fn new(
        target: Option<MessageRef>,
        author: Option<ActorRef>,
        name: Option<String>,
        preview: Option<String>,
    ) -> Self {
        Self {
            target,
            author,
            name,
            preview,
        }
    }
    /// Borrows the observed reply target when available.
    pub fn target(&self) -> Option<&MessageRef> {
        self.target.as_ref()
    }
    /// Borrows the observed reply author when available.
    pub fn author(&self) -> Option<&ActorRef> {
        self.author.as_ref()
    }
    /// Borrows the reply author name when available.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    /// Borrows the observed reply preview when available.
    pub fn preview(&self) -> Option<&str> {
        self.preview.as_deref()
    }
}
/// Optional contextual bell text and its reported completion status.
#[derive(Clone, Debug, Default)]
pub struct BellObservation {
    text: Option<String>,
    status: Option<BellStatus>,
}
impl BellObservation {
    /// Retains the supplied optional augmentation text and status.
    pub fn new(text: Option<String>, status: Option<BellStatus>) -> Self {
        Self { text, status }
    }
    /// Borrows the retained contextual augmentation when available.
    pub fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
    /// Returns the reported augmentation status when available.
    pub fn status(&self) -> Option<BellStatus> {
        self.status
    }
}

/// A supplied message creation observation before host presentation.
///
/// Construction retains the supplied identities and content; it does not authenticate
/// an author or check current source access.
#[derive(Clone, Debug)]
pub struct MessageCreated {
    source: MessageRef,
    author: ObservedAuthor,
    content: ObservedText,
    targeting: Targeting,
    occurred_at: Timestamp,
    attachments: Vec<ObservedAttachment>,
    voice: bool,
    thread_parent: Option<ConversationRef>,
    reply: ReplyObservation,
    bells: BellObservation,
}
impl MessageCreated {
    /// Retains a creation observation with empty attachments, reply, and bell context.
    pub fn new(
        source: MessageRef,
        author: ObservedAuthor,
        text: String,
        targeting: Targeting,
        occurred_at: Timestamp,
    ) -> Self {
        Self {
            source,
            author,
            content: ObservedText::new(text),
            targeting,
            occurred_at,
            attachments: Vec::new(),
            voice: false,
            thread_parent: None,
            reply: ReplyObservation::default(),
            bells: BellObservation::default(),
        }
    }
    /// Replaces attachment metadata and the supplied voice-message classification.
    pub fn with_attachments(mut self, attachments: Vec<ObservedAttachment>, voice: bool) -> Self {
        self.attachments = attachments;
        self.voice = voice;
        self
    }
    /// Replaces the observed thread parent and reply metadata.
    pub fn with_context(
        mut self,
        thread_parent: Option<ConversationRef>,
        reply: ReplyObservation,
    ) -> Self {
        self.thread_parent = thread_parent;
        self.reply = reply;
        self
    }
    /// Replaces the retained contextual bell observation.
    pub fn with_bells(mut self, bells: BellObservation) -> Self {
        self.bells = bells;
        self
    }
    /// Borrows the source-qualified message identity.
    pub fn source(&self) -> &MessageRef {
        &self.source
    }
    /// Borrows the supplied author observation.
    pub fn author(&self) -> &ObservedAuthor {
        &self.author
    }
    /// Borrows the original text and its parsed terminal locators.
    pub fn content(&self) -> &ObservedText {
        &self.content
    }
    /// Returns the supplied targeting classification.
    pub fn targeting(&self) -> Targeting {
        self.targeting
    }
    /// Borrows the supplied occurrence timestamp.
    pub fn occurred_at(&self) -> &Timestamp {
        &self.occurred_at
    }
    /// Borrows the retained attachment metadata.
    pub fn attachments(&self) -> &[ObservedAttachment] {
        &self.attachments
    }
    /// Returns the supplied voice-message classification.
    pub fn is_voice(&self) -> bool {
        self.voice
    }
    /// Borrows the observed thread parent when available.
    pub fn thread_parent(&self) -> Option<&ConversationRef> {
        self.thread_parent.as_ref()
    }
    /// Borrows the retained reply observation.
    pub fn reply(&self) -> &ReplyObservation {
        &self.reply
    }
    /// Borrows the retained contextual bell observation.
    pub fn bells(&self) -> &BellObservation {
        &self.bells
    }
}

/// A supplied message edit observation before host presentation.
#[derive(Clone, Debug)]
pub struct MessageEdited {
    source: MessageRef,
    author: ActorRef,
    author_name: String,
    content: ObservedText,
    occurred_at: Timestamp,
    thread_parent: Option<ConversationRef>,
    reply_target: Option<MessageRef>,
}
impl MessageEdited {
    /// Retains an edit observation without a thread parent or reply target.
    pub fn new(
        source: MessageRef,
        author: ActorRef,
        author_name: String,
        text: String,
        occurred_at: Timestamp,
    ) -> Self {
        Self {
            source,
            author,
            author_name,
            content: ObservedText::new(text),
            occurred_at,
            thread_parent: None,
            reply_target: None,
        }
    }
    /// Replaces the observed thread parent and reply target.
    pub fn with_context(
        mut self,
        thread_parent: Option<ConversationRef>,
        reply_target: Option<MessageRef>,
    ) -> Self {
        self.thread_parent = thread_parent;
        self.reply_target = reply_target;
        self
    }
    /// Borrows the source-qualified message identity.
    pub fn source(&self) -> &MessageRef {
        &self.source
    }
    /// Borrows the supplied actor identity.
    pub fn author(&self) -> &ActorRef {
        &self.author
    }
    /// Borrows the author name captured for presentation.
    pub fn author_name(&self) -> &str {
        &self.author_name
    }
    /// Borrows the edited text and its parsed terminal locators.
    pub fn content(&self) -> &ObservedText {
        &self.content
    }
    /// Borrows the supplied edit occurrence timestamp.
    pub fn occurred_at(&self) -> &Timestamp {
        &self.occurred_at
    }
    /// Borrows the observed thread parent when available.
    pub fn thread_parent(&self) -> Option<&ConversationRef> {
        self.thread_parent.as_ref()
    }
    /// Borrows the observed reply target when available.
    pub fn reply_target(&self) -> Option<&MessageRef> {
        self.reply_target.as_ref()
    }
}

/// A message or reaction occurrence carried through ordinary delivery.
#[derive(Clone, Debug)]
pub enum OrdinaryEvent {
    /// A message creation with its retained context.
    Message(Box<MessageCreated>),
    /// A message edit with its retained context.
    MessageEdited(Box<MessageEdited>),
    /// An observed deletion of the referenced message.
    MessageDeleted {
        /// The deleted message identity.
        source: MessageRef,
        /// The observed parent conversation when available.
        thread_parent: Option<ConversationRef>,
    },
    /// An observed reaction to the referenced message.
    ReactionAdded {
        /// The message receiving the reaction.
        source: MessageRef,
        /// The supplied reacting actor identity.
        author: ActorRef,
        /// The reacting actor name captured for presentation.
        author_name: String,
        /// The observed reaction text.
        emoji: String,
        /// Whether the source adapter classified the reaction as the construct's own.
        self_react: bool,
    },
}
impl OrdinaryEvent {
    /// Borrows the message identity associated with this occurrence.
    pub fn source(&self) -> &MessageRef {
        match self {
            Self::Message(message) => message.source(),
            Self::MessageEdited(edit) => edit.source(),
            Self::MessageDeleted { source, .. } | Self::ReactionAdded { source, .. } => source,
        }
    }
    /// Borrows retained thread context; reaction observations carry none.
    pub fn thread_parent(&self) -> Option<&ConversationRef> {
        match self {
            Self::Message(message) => message.thread_parent(),
            Self::MessageEdited(edit) => edit.thread_parent(),
            Self::MessageDeleted { thread_parent, .. } => thread_parent.as_ref(),
            Self::ReactionAdded { .. } => None,
        }
    }
}

/// A nonempty host permission-request identifier; possession grants no permission.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PermissionRequestId(String);
impl PermissionRequestId {
    /// Retains a nonempty request identifier without granting authority.
    ///
    /// # Errors
    /// Rejects an empty string.
    pub fn new(value: String) -> Result<Self, ControlError> {
        if value.is_empty() {
            Err(ControlError {
                kind: ControlErrorKind::EmptyPermission,
            })
        } else {
            Ok(Self(value))
        }
    }
    /// Borrows the unchanged request identifier.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
/// The permission outcome reported to the host.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionDecision {
    /// The reported permission decision was approval.
    Granted,
    /// The reported permission decision was refusal.
    Denied,
}
/// Trace severity retained from the tracing library.
pub use tracing::Level as TraceLevel;
/// A supplied coordination transition for host presentation.
#[derive(Clone, Debug)]
pub enum CoordinationNotice {
    /// A responder completed the claimed message with a reply.
    Done {
        /// The source message whose claim completed.
        claimed: MessageRef,
        /// The supplied responder identity.
        responder: ActorRef,
        /// The reply associated with completion.
        reply: MessageRef,
    },
    /// The claimed message was promoted for handling.
    Promoted {
        /// The source message promoted for handling.
        claimed: MessageRef,
    },
}
/// A host control notice kept separate from ordinary and managed messages.
#[derive(Clone, Debug)]
pub enum ControlEvent {
    /// A reported decision for a host permission request.
    Permission {
        /// The nonempty request identity supplied by the host.
        request: PermissionRequestId,
        /// The reported permission outcome.
        decision: PermissionDecision,
    },
    /// A structured trace notice for host presentation.
    Trace {
        /// The trace severity.
        level: TraceLevel,
        /// The trace target retained for presentation.
        target: String,
        /// The trace message retained for presentation.
        message: String,
        /// The captured trace field names and rendered values.
        fields: Vec<(String, String)>,
    },
    /// A configuration change was rejected.
    ConfigurationRejected {
        /// The user-facing rejection text.
        display_error: String,
    },
    /// A retained coordination transition.
    Coordination(Box<CoordinationNotice>),
}
/// A rejected control-event identifier.
#[derive(Debug, Error)]
#[error("invalid control event: {kind}")]
pub struct ControlError {
    /// The violated control-event constraint.
    #[source]
    pub kind: ControlErrorKind,
}
/// The violated control-event construction rule.
#[derive(Debug, Error)]
pub enum ControlErrorKind {
    #[error("permission request must not be empty")]
    /// The permission-request identifier was empty.
    EmptyPermission,
}
