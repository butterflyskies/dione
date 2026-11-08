//! Root sealed-observation capture adapter; metadata projection is owned by the read library.
use super::observation::DiscordMessageObservation;
use dione_discord_context::native::{NativeObservationError, native_supplement};
use dione_rook::RetainedObservation;
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

#[cfg(test)]
mod tests {
    use super::*;
    use dione_config::Config;
    use dione_config::LoadedConfig;
    use dione_contracts::{
        ModuleInstanceId, ModuleSlotId, ProviderId,
        context::ObservationOrigin,
        observation::{MessageObservation, MessageOccurrence},
    };
    use dione_discord_queries::queries::MessageView;
    use serenity::model::channel::Message;
    use serenity::model::id::ChannelId;

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
            &LoadedConfig::try_from_raw(Config::default()).expect("valid test configuration"),
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
        let altered = dione_contracts::context::ContextObservation::new(
            dione_contracts::MessageRef::new(
                dione_contracts::ConversationRef::new(
                    source,
                    dione_contracts::ConversationId::opaque("wrong").unwrap(),
                ),
                dione_contracts::MessageId::opaque("wrong").unwrap(),
            ),
            context.sender().clone(),
            context.content().into(),
            *context.created_at(),
            dione_contracts::context::ContextMetadata::new(
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
