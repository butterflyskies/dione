//! Descriptive Discord observations and their canonical conversion to typed deliveries.
//!
//! Event values carry observations, not authority to perform native operations.
#![warn(missing_docs)]

use dione_attention::types::SourceAuthorKind;
use dione_bell_rings::BellStatus;
use dione_messages::timestamp::Timestamp;
use serenity::model::id::{ChannelId, MessageId, UserId};

pub mod delivery;
pub use delivery::{DeliveryConversionError, DeliveryConversionErrorKind, batch, message, single};

/// Typed evidence that a guild message was directed to the construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MentionKind {
    /// Discord carried an explicit mention of the construct's bot user.
    DirectMention,
    /// The message replied to one authored by the construct's bot user.
    ReplyToConstruct,
    /// A configured mention pattern matched the message content.
    ConfiguredPattern,
}

/// Metadata about a Discord attachment, forwarded to the MCP client.
#[derive(Debug, Clone)]
pub struct AttachmentMeta {
    /// The observed attachment filename.
    pub name: String,
    /// The reported attachment media type, when present.
    pub content_type: Option<String>,
    /// The exact reported attachment size in bytes.
    pub size: u64,
}

/// A Discord message forwarded from the gateway to the MCP notification stream.
#[derive(Debug, Clone)]
pub struct MessageEvent {
    /// The observed Discord channel.
    pub chat_id: ChannelId,
    /// The observed Discord message.
    pub message_id: MessageId,
    /// The observed author display name.
    pub user: String,
    /// The observed Discord author.
    pub user_id: UserId,
    /// Verified author/transport class captured before delayed attention work.
    pub author_kind: SourceAuthorKind,
    /// The original observed text.
    pub content: String,
    /// Typed targeting evidence captured at the Discord ingress boundary.
    pub targeting: MessageTargeting,
    /// The original observation timestamp.
    pub timestamp: Timestamp,
    /// The observed attachment metadata in order.
    pub attachments: Vec<AttachmentMeta>,
    /// Whether Discord marked the observation as a voice message.
    pub is_voice_message: bool,
    /// If the message was sent in a thread, the parent channel ID.
    pub thread_parent_id: Option<ChannelId>,
    /// If the message is a reply, the ID of the message being replied to.
    pub reply_to_message_id: Option<MessageId>,
    /// If the message is a reply, the author ID of the replied-to message.
    pub reply_to_user_id: Option<UserId>,
    /// If the message is a reply, the author name of the replied-to message.
    pub reply_to_user: Option<String>,
    /// If the message is a reply, a short preview of the replied-to content.
    pub reply_to_content_preview: Option<String>,
    /// Pre-rendered bell metadata (live mode only).
    pub bells: Option<String>,
    /// Retrieval status for bell evaluation.
    pub bells_status: Option<BellStatus>,
}

/// Whether Discord ingress had explicit evidence that a message targeted this construct.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageTargeting {
    /// An accepted direct message.
    DirectMessage,
    /// A guild message with explicit typed mention evidence.
    GuildDirected(MentionKind),
    /// An opted-in guild message delivered without directed evidence.
    Ambient,
}

impl MessageTargeting {
    /// Returns whether the message is eligible for directed-only processing.
    pub fn is_directed(self) -> bool {
        !matches!(self, Self::Ambient)
    }
}

/// Events forwarded from the Discord gateway to the MCP notification stream.
#[derive(Debug, Clone)]
pub enum NotificationEvent {
    /// A newly observed Discord message.
    Message(MessageEvent),
    /// An observed reaction addition.
    Reaction {
        /// The observed Discord channel.
        chat_id: ChannelId,
        /// The observed Discord message.
        message_id: MessageId,
        /// The observed author display name.
        user: String,
        /// The observed Discord author.
        user_id: UserId,
        /// The observed reaction emoji.
        emoji: String,
        /// When true, this reaction was added by the bot itself (e.g. contradictionary celebrate).
        /// Gateway self-reactions are filtered, so this is only set for tool-initiated reacts.
        self_react: bool,
    },
    /// An observed permission response.
    PermissionResponse {
        /// The original permission request identifier.
        request_id: String,
        /// Whether the permission request was granted.
        granted: bool,
    },
    /// An observed structured trace.
    Trace {
        /// The recorded trace level.
        level: tracing::Level,
        /// The recorded trace target.
        target: String,
        /// The recorded trace message.
        message: String,
        /// The recorded structured trace fields.
        fields: Vec<(String, String)>,
    },
    /// An observed message edit.
    MessageEdit {
        /// The observed Discord channel.
        chat_id: ChannelId,
        /// The observed Discord message.
        message_id: MessageId,
        /// The observed author display name.
        user: String,
        /// The observed Discord author.
        user_id: UserId,
        /// The original edited text.
        new_content: String,
        /// The original observation timestamp.
        timestamp: Timestamp,
        /// If the edit was in a thread, the parent channel ID.
        thread_parent_id: Option<ChannelId>,
        /// If the edited message is a reply, the ID of the message being replied to.
        reply_to_message_id: Option<MessageId>,
    },
    /// An observed message deletion.
    MessageDelete {
        /// The observed Discord channel.
        chat_id: ChannelId,
        /// The observed Discord message.
        message_id: MessageId,
        /// If the delete was in a thread, the parent channel ID.
        thread_parent_id: Option<ChannelId>,
    },
    /// A rejected configuration observation.
    ConfigError {
        /// The descriptive configuration error text.
        error: String,
    },
    /// A push from the reply-coordination client: the construct ahead replied
    /// (`Done`) or everyone ahead released/timed out (`Promoted`).
    Coordination(dione_coordination::CoordinationEvent),
}

impl NotificationEvent {
    /// Whether this event carries an exact terminal author-offered evidence marker.
    pub fn has_offered_evidence(&self) -> bool {
        match self {
            Self::Message(message) => {
                !dione_evidence::parse_sentex_locators(&message.content).is_empty()
            }
            Self::MessageEdit { new_content, .. } => {
                !dione_evidence::parse_sentex_locators(new_content).is_empty()
            }
            Self::Reaction { .. }
            | Self::PermissionResponse { .. }
            | Self::Trace { .. }
            | Self::MessageDelete { .. }
            | Self::ConfigError { .. }
            | Self::Coordination(_) => false,
        }
    }
}

/// Bounded ordinary lifecycle metadata storage with opaque caller-owned lineage.
pub mod lifecycle_storage;
