//! Provider-qualified event identities shared by platforms and resident delivery.

use crate::teams::{ActivityId, BotId, ConversationId, TenantId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "provider", content = "id", rename_all = "snake_case")]
pub(crate) enum ProviderEventKey {
    Teams {
        tenant_id: TenantId,
        bot_id: BotId,
        conversation_id: ConversationId,
        activity_id: ActivityId,
    },
}

impl ProviderEventKey {
    pub(crate) fn teams(
        tenant_id: TenantId,
        bot_id: BotId,
        conversation_id: ConversationId,
        activity_id: ActivityId,
    ) -> Self {
        Self::Teams {
            tenant_id,
            bot_id,
            conversation_id,
            activity_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_event_key_deserialization_validates_every_id() {
        for field in ["tenant_id", "bot_id", "conversation_id", "activity_id"] {
            for invalid in [String::new(), "id\n".to_owned(), "x".repeat(513)] {
                let mut value = json!({
                    "provider": "teams",
                    "id": { "tenant_id": "tenant", "bot_id": "bot", "conversation_id": "conversation", "activity_id": "activity" }
                });
                value["id"][field] = json!(invalid);
                let error = serde_json::from_value::<ProviderEventKey>(value).unwrap_err();
                assert!(
                    error.to_string().contains("invalid Teams"),
                    "{field}: {error}"
                );
            }
        }
    }

    #[test]
    fn provider_event_key_retains_wire_shape_and_boundary_values() {
        let longest = "x".repeat(512);
        let expected = json!({
            "provider": "teams",
            "id": { "tenant_id": longest, "bot_id": longest, "conversation_id": longest, "activity_id": longest }
        });
        let key = ProviderEventKey::teams(
            longest.parse().unwrap(),
            longest.parse().unwrap(),
            longest.parse().unwrap(),
            longest.parse().unwrap(),
        );
        assert_eq!(serde_json::to_value(&key).unwrap(), expected);
        let mut extended = expected;
        extended["id"]["future_field"] = json!(true);
        assert_eq!(
            serde_json::from_value::<ProviderEventKey>(extended).unwrap(),
            key
        );
    }
}
