//! Observed provider events before host presentation.

use super::{ActorRef, ConversationRef, MessageRef};
use crate::{
    evidence::locators::{SentexLocator, parse_sentex_locators},
    timestamp::Timestamp,
};
use thiserror::Error;

/// Exact attachment size, including empty files.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ByteSize(u64);
impl ByteSize {
    pub(crate) fn new(bytes: u64) -> Self {
        Self(bytes)
    }
    pub(crate) fn get(self) -> u64 {
        self.0
    }
}

/// Original text and the terminal locators parsed from that same observation.
#[derive(Clone, Debug)]
pub(crate) struct ObservedText {
    text: String,
    locators: Vec<SentexLocator>,
}
impl ObservedText {
    pub(crate) fn new(text: String) -> Self {
        Self {
            locators: parse_sentex_locators(&text),
            text,
        }
    }
    pub(crate) fn text(&self) -> &str {
        &self.text
    }
    pub(crate) fn locators(&self) -> &[SentexLocator] {
        &self.locators
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum AuthorOrigin {
    Unknown,
    DirectHuman,
    DirectBot,
    VerifiedWebhook,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Targeting {
    DirectMessage,
    DirectMention,
    ReplyToConstruct,
    ConfiguredPattern,
    Ambient,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BellStatus {
    Ok,
    PartialTimeout,
    PartialError,
    Timeout,
    Error,
}
impl BellStatus {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Ok => "ok",
            Self::PartialTimeout => "partial_timeout",
            Self::PartialError => "partial_error",
            Self::Timeout => "timeout",
            Self::Error => "error",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct ObservedAuthor {
    actor: ActorRef,
    name: String,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "retained source observation; legacy host projections omit this field"
        )
    )]
    origin: AuthorOrigin,
}
impl ObservedAuthor {
    pub(crate) fn new(actor: ActorRef, name: String, origin: AuthorOrigin) -> Self {
        Self {
            actor,
            name,
            origin,
        }
    }
    pub(crate) fn actor(&self) -> &ActorRef {
        &self.actor
    }
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "retained source observation; legacy host projections omit this field"
        )
    )]
    pub(crate) fn origin(&self) -> AuthorOrigin {
        self.origin
    }
}
#[derive(Clone, Debug)]
pub(crate) struct ObservedAttachment {
    name: String,
    media_type: Option<String>,
    size: ByteSize,
}
impl ObservedAttachment {
    pub(crate) fn new(name: String, media_type: Option<String>, size: ByteSize) -> Self {
        Self {
            name,
            media_type,
            size,
        }
    }
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    pub(crate) fn media_type(&self) -> Option<&str> {
        self.media_type.as_deref()
    }
    pub(crate) fn size(&self) -> ByteSize {
        self.size
    }
}
#[derive(Clone, Debug, Default)]
pub(crate) struct ReplyObservation {
    target: Option<MessageRef>,
    author: Option<ActorRef>,
    name: Option<String>,
    preview: Option<String>,
}
impl ReplyObservation {
    pub(crate) fn new(
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
    pub(crate) fn target(&self) -> Option<&MessageRef> {
        self.target.as_ref()
    }
    pub(crate) fn author(&self) -> Option<&ActorRef> {
        self.author.as_ref()
    }
    pub(crate) fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }
    pub(crate) fn preview(&self) -> Option<&str> {
        self.preview.as_deref()
    }
}
#[derive(Clone, Debug, Default)]
pub(crate) struct BellObservation {
    text: Option<String>,
    status: Option<BellStatus>,
}
impl BellObservation {
    pub(crate) fn new(text: Option<String>, status: Option<BellStatus>) -> Self {
        Self { text, status }
    }
    pub(crate) fn text(&self) -> Option<&str> {
        self.text.as_deref()
    }
    pub(crate) fn status(&self) -> Option<BellStatus> {
        self.status
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MessageCreated {
    source: MessageRef,
    author: ObservedAuthor,
    content: ObservedText,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "retained source observation; legacy host projections omit this field"
        )
    )]
    targeting: Targeting,
    occurred_at: Timestamp,
    attachments: Vec<ObservedAttachment>,
    voice: bool,
    thread_parent: Option<ConversationRef>,
    reply: ReplyObservation,
    bells: BellObservation,
}
impl MessageCreated {
    pub(crate) fn new(
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
    pub(crate) fn with_attachments(
        mut self,
        attachments: Vec<ObservedAttachment>,
        voice: bool,
    ) -> Self {
        self.attachments = attachments;
        self.voice = voice;
        self
    }
    pub(crate) fn with_context(
        mut self,
        thread_parent: Option<ConversationRef>,
        reply: ReplyObservation,
    ) -> Self {
        self.thread_parent = thread_parent;
        self.reply = reply;
        self
    }
    pub(crate) fn with_bells(mut self, bells: BellObservation) -> Self {
        self.bells = bells;
        self
    }
    pub(crate) fn source(&self) -> &MessageRef {
        &self.source
    }
    pub(crate) fn author(&self) -> &ObservedAuthor {
        &self.author
    }
    pub(crate) fn content(&self) -> &ObservedText {
        &self.content
    }
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "retained source observation; legacy host projections omit this field"
        )
    )]
    pub(crate) fn targeting(&self) -> Targeting {
        self.targeting
    }
    pub(crate) fn occurred_at(&self) -> &Timestamp {
        &self.occurred_at
    }
    pub(crate) fn attachments(&self) -> &[ObservedAttachment] {
        &self.attachments
    }
    pub(crate) fn is_voice(&self) -> bool {
        self.voice
    }
    pub(crate) fn thread_parent(&self) -> Option<&ConversationRef> {
        self.thread_parent.as_ref()
    }
    pub(crate) fn reply(&self) -> &ReplyObservation {
        &self.reply
    }
    pub(crate) fn bells(&self) -> &BellObservation {
        &self.bells
    }
}

#[derive(Clone, Debug)]
pub(crate) struct MessageEdited {
    source: MessageRef,
    author: ActorRef,
    author_name: String,
    content: ObservedText,
    occurred_at: Timestamp,
    thread_parent: Option<ConversationRef>,
    reply_target: Option<MessageRef>,
}
impl MessageEdited {
    pub(crate) fn new(
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
    pub(crate) fn with_context(
        mut self,
        thread_parent: Option<ConversationRef>,
        reply_target: Option<MessageRef>,
    ) -> Self {
        self.thread_parent = thread_parent;
        self.reply_target = reply_target;
        self
    }
    pub(crate) fn source(&self) -> &MessageRef {
        &self.source
    }
    pub(crate) fn author(&self) -> &ActorRef {
        &self.author
    }
    pub(crate) fn author_name(&self) -> &str {
        &self.author_name
    }
    pub(crate) fn content(&self) -> &ObservedText {
        &self.content
    }
    pub(crate) fn occurred_at(&self) -> &Timestamp {
        &self.occurred_at
    }
    pub(crate) fn thread_parent(&self) -> Option<&ConversationRef> {
        self.thread_parent.as_ref()
    }
    pub(crate) fn reply_target(&self) -> Option<&MessageRef> {
        self.reply_target.as_ref()
    }
}

#[derive(Clone, Debug)]
pub(crate) enum OrdinaryEvent {
    Message(Box<MessageCreated>),
    MessageEdited(Box<MessageEdited>),
    MessageDeleted {
        source: MessageRef,
        thread_parent: Option<ConversationRef>,
    },
    ReactionAdded {
        source: MessageRef,
        author: ActorRef,
        author_name: String,
        emoji: String,
        self_react: bool,
    },
}
impl OrdinaryEvent {
    pub(crate) fn source(&self) -> &MessageRef {
        match self {
            Self::Message(message) => message.source(),
            Self::MessageEdited(edit) => edit.source(),
            Self::MessageDeleted { source, .. } | Self::ReactionAdded { source, .. } => source,
        }
    }
    pub(crate) fn thread_parent(&self) -> Option<&ConversationRef> {
        match self {
            Self::Message(message) => message.thread_parent(),
            Self::MessageEdited(edit) => edit.thread_parent(),
            Self::MessageDeleted { thread_parent, .. } => thread_parent.as_ref(),
            Self::ReactionAdded { .. } => None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct PermissionRequestId(String);
impl PermissionRequestId {
    pub(crate) fn new(value: String) -> Result<Self, ControlError> {
        if value.is_empty() {
            Err(ControlError {
                kind: ControlErrorKind::EmptyPermission,
            })
        } else {
            Ok(Self(value))
        }
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum PermissionDecision {
    Granted,
    Denied,
}
pub(crate) use tracing::Level as TraceLevel;
#[derive(Clone, Debug)]
pub(crate) enum CoordinationNotice {
    Done {
        claimed: MessageRef,
        responder: ActorRef,
        reply: MessageRef,
    },
    Promoted {
        claimed: MessageRef,
    },
}
#[derive(Clone, Debug)]
pub(crate) enum ControlEvent {
    Permission {
        request: PermissionRequestId,
        decision: PermissionDecision,
    },
    Trace {
        level: TraceLevel,
        target: String,
        message: String,
        fields: Vec<(String, String)>,
    },
    ConfigurationRejected {
        display_error: String,
    },
    Coordination(Box<CoordinationNotice>),
}
#[derive(Debug, Error)]
#[error("invalid control event: {kind}")]
pub(crate) struct ControlError {
    #[source]
    pub(crate) kind: ControlErrorKind,
}
#[derive(Debug, Error)]
pub(crate) enum ControlErrorKind {
    #[error("permission request must not be empty")]
    EmptyPermission,
}
