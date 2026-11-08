//! Lossless bounded native metadata beside the durable common observation.

use dione_discord_queries::queries::DiscordNativeObservation;
use serde::Serialize;
use serenity::model::{
    Timestamp,
    channel::{Attachment, MessageReference, MessageType},
    id::{ApplicationId, ChannelId, GuildId, MessageId, UserId, WebhookId},
};
use thiserror::Error;

use dione_contracts::context::{Availability, UnavailableReason};
use dione_rook::{NativeSupplement, ObservationError};

#[derive(Serialize)]
struct NativeMetadata<'a> {
    channel_id: ChannelId,
    message_id: MessageId,
    guild_id: Option<GuildId>,
    thread_parent: Option<ChannelId>,
    message_type: MessageType,
    author_id: UserId,
    author_name: &'a str,
    author_bot: bool,
    webhook_id: Option<WebhookId>,
    application_id: Option<ApplicationId>,
    reference: Option<&'a MessageReference>,
    resolved_reference: Option<ResolvedCoordinates>,
    attachments: &'a [Attachment],
    edited_at: Option<Timestamp>,
}

#[derive(Serialize)]
struct ResolvedCoordinates {
    channel_id: ChannelId,
    message_id: MessageId,
    guild_id: Option<GuildId>,
    author_id: UserId,
}

/// A rejected Discord-to-durable observation conversion.
#[derive(Debug, Error)]
#[error("cannot retain Discord observation: {kind}")]
pub struct NativeObservationError {
    /// The failed coordinate, encoding, or common validation operation.
    #[source]
    pub kind: NativeObservationErrorKind,
}

/// Failures while preserving Discord's modeled native metadata.
#[derive(Debug, Error)]
pub enum NativeObservationErrorKind {
    /// Common message coordinates or raw sender disagree with native values.
    #[error("native and common message coordinates differ")]
    MessageCoordinates,
    /// Common workspace coordinates disagree with the native guild.
    #[error("native and common workspace coordinates differ")]
    WorkspaceCoordinates,
    /// Native metadata could not be serialized.
    #[error("cannot encode native observation")]
    Encoding(#[source] serde_json::Error),
    /// The retained observation or its supplemental payload is invalid.
    #[error(transparent)]
    Validation(ObservationError),
}

impl From<ObservationError> for NativeObservationError {
    fn from(error: ObservationError) -> Self {
        Self {
            kind: NativeObservationErrorKind::Validation(error),
        }
    }
}

/// Validate and encode native metadata beside its matching common observation.
///
/// # Errors
/// Rejects mismatched coordinates, workspace identity or invalid supplemental payloads.
pub fn native_supplement(
    context: &dione_contracts::context::ContextObservation,
    native: &DiscordNativeObservation,
    thread_parent: Option<ChannelId>,
) -> Result<NativeSupplement, NativeObservationError> {
    let reference = context.reference();
    let source = reference.conversation().module();
    if reference.local().as_numeric().map(|id| id.get()) != Some(native.message_id().get())
        || reference
            .conversation()
            .local()
            .as_numeric()
            .map(|id| id.get())
            != Some(native.channel_id().get())
        || context.sender().reference().module() != source
        || context
            .sender()
            .reference()
            .local()
            .as_numeric()
            .map(|id| id.get())
            != Some(native.author_id().get())
    {
        return Err(NativeObservationError {
            kind: NativeObservationErrorKind::MessageCoordinates,
        });
    }
    let workspace_matches = match (context.workspace(), native.guild_id()) {
        (Availability::Known(workspace), Some(guild)) => {
            workspace.module() == source
                && workspace.local().as_numeric().map(|id| id.get()) == Some(guild.get())
        }
        (Availability::Unavailable(UnavailableReason::NotReported), None) => true,
        _ => false,
    };
    if !workspace_matches {
        return Err(NativeObservationError {
            kind: NativeObservationErrorKind::WorkspaceCoordinates,
        });
    }
    let metadata = NativeMetadata {
        channel_id: native.channel_id(),
        message_id: native.message_id(),
        guild_id: native.guild_id(),
        thread_parent,
        message_type: native.message_type(),
        author_id: native.author_id(),
        author_name: native.author_name(),
        author_bot: native.author_bot(),
        webhook_id: native.webhook_id(),
        application_id: native.application_id(),
        reference: native.reference(),
        resolved_reference: native
            .resolved_reference()
            .map(|reference| ResolvedCoordinates {
                channel_id: reference.channel_id(),
                message_id: reference.message_id(),
                guild_id: reference.guild_id(),
                author_id: reference.author_id(),
            }),
        attachments: native.attachments(),
        edited_at: native.edited_at(),
    };
    let bytes = serde_json::to_vec(&metadata).map_err(|error| NativeObservationError {
        kind: NativeObservationErrorKind::Encoding(error),
    })?;
    let supplement = NativeSupplement::new("discord.observation".into(), 1, bytes)?;
    Ok(supplement)
}
