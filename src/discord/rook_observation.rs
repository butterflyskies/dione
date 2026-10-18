//! Lossless bounded native metadata beside the durable common observation.

use serde::Serialize;
use serenity::model::{
    Timestamp,
    channel::{Attachment, MessageReference, MessageType},
    id::{ApplicationId, ChannelId, GuildId, MessageId, UserId, WebhookId},
};
use thiserror::Error;

use super::{messaging::queries::DiscordNativeObservation, observation::DiscordMessageObservation};
use crate::{
    core::context::{Availability, UnavailableReason},
    rook::{NativeSupplement, ObservationError, RetainedObservation},
};

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

impl TryFrom<&DiscordMessageObservation> for RetainedObservation {
    type Error = NativeObservationError;

    fn try_from(value: &DiscordMessageObservation) -> Result<Self, Self::Error> {
        let supplement =
            native_supplement(value.common.context(), &value.native, value.thread_parent)?;
        Self::capture(&value.common)
            .with_native(supplement)
            .map_err(Into::into)
    }
}

pub(crate) fn native_supplement(
    context: &crate::core::context::ContextObservation,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, LoadedConfig},
        core::{
            ModuleInstanceId, ModuleSlotId, ProviderId,
            context::ObservationOrigin,
            observation::{MessageObservation, MessageOccurrence},
        },
        discord::messaging::queries::MessageView,
    };
    use serenity::model::channel::Message;

    fn fixture() -> DiscordMessageObservation {
        let message: Message = serde_json::from_value(serde_json::json!({
            "id":"30", "channel_id":"10", "guild_id":"40", "type":19,
            "author":{"id":"20", "username":"native sender", "discriminator":"0000", "bot":true},
            "content":"untrusted message", "timestamp":"2026-10-13T12:00:00Z",
            "edited_timestamp":"2026-10-13T12:00:01.123456789Z", "webhook_id":"50", "application_id":"60",
            "tts":false, "mention_everyone":false, "mentions":[], "mention_roles":[], "embeds":[], "pinned":false,
            "message_reference":{"type":0,"channel_id":"70","message_id":"80","guild_id":"90","fail_if_not_exists":false},
            "attachments":[{"id":"100","filename":"sample.bin","size":123,"url":"https://example.invalid/original","proxy_url":"https://example.invalid/proxy","description":"native description","content_type":"application/octet-stream","ephemeral":true}]
        })).unwrap();
        let source = ModuleInstanceId::new(
            ProviderId::new("discord").unwrap(),
            ModuleSlotId::new("secondary").unwrap(),
        );
        let view = MessageView::from_discord(
            &LoadedConfig::from_raw(Config::default()),
            &message,
            ObservationOrigin::GatewayCreate,
        )
        .unwrap();
        DiscordMessageObservation {
            common: MessageObservation::new(
                view.context(&source).unwrap(),
                MessageOccurrence::Created,
            )
            .unwrap(),
            native: view.into_native(),
            thread_parent: Some(ChannelId::new(5)),
        }
    }

    #[test]
    fn adapter_retains_native_metadata_without_parent_bodies() {
        let observed = fixture();
        let retained = RetainedObservation::try_from(&observed).unwrap();
        let supplement = serde_json::to_value(retained.native().unwrap()).unwrap();
        assert_eq!(supplement["codec"], "discord.observation");
        assert_eq!(supplement["version"], 1);
        let bytes: Vec<u8> = serde_json::from_value(supplement["bytes"].clone()).unwrap();
        let native: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(native["thread_parent"], "5");
        assert_eq!(native["webhook_id"], "50");
        assert_eq!(native["application_id"], "60");
        assert_eq!(native["reference"]["message_id"], "80");
        assert_eq!(
            native["attachments"][0]["url"],
            "https://example.invalid/original"
        );
        assert_eq!(
            native["attachments"][0]["description"],
            "native description"
        );
        assert!(native.get("referenced_message").is_none());
        assert_eq!(
            native["edited_at"],
            serde_json::to_value(observed.native.edited_at()).unwrap()
        );
    }

    #[test]
    fn adapter_rejects_mismatched_common_coordinates() {
        let mut observed = fixture();
        let context = observed.common.context();
        let source = context.reference().conversation().module().clone();
        let altered = crate::core::context::ContextObservation::new(
            crate::core::MessageRef::new(
                crate::core::ConversationRef::new(
                    source,
                    crate::core::ConversationId::opaque("wrong").unwrap(),
                ),
                crate::core::MessageId::opaque("wrong").unwrap(),
            ),
            context.sender().clone(),
            context.content().into(),
            *context.created_at(),
            crate::core::context::ContextMetadata::new(
                context.workspace().clone(),
                context.edited_at().clone(),
                context.relation().clone(),
                context.attachments().clone(),
                context.observation().clone(),
            ),
        );
        observed.common = MessageObservation::new(altered, MessageOccurrence::Created).unwrap();
        assert!(RetainedObservation::try_from(&observed).is_err());
    }
}
