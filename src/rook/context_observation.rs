//! Authoritative context reads without a fabricated gateway occurrence.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::observation::{
    ActorKey, ContextFields, MessageKey, NativeSupplement, ObservationError, WorkspaceKey,
};

/// An authoritative provider context read, independent of event delivery identity.
///
/// This value describes the message at read time. Its edited timestamp does not
/// claim to reconstruct the content of an earlier delivered occurrence.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ContextObservation {
    reference: MessageKey,
    context: ContextFields,
    native: Option<NativeSupplement>,
}

impl ContextObservation {
    pub(crate) fn capture(value: &crate::core::context::ContextObservation) -> Self {
        Self {
            reference: value.reference().into(),
            context: ContextFields::capture(value),
            native: None,
        }
    }

    /// Attaches descriptive adapter metadata without changing common identities.
    ///
    /// # Errors
    /// Rejects invalid common identities or a native supplement exceeding its bounds.
    pub fn with_native_supplement(
        mut self,
        supplement: NativeSupplement,
    ) -> Result<Self, ObservationError> {
        self.native = Some(supplement);
        self.validate()?;
        Ok(self)
    }

    /// Returns the exact source-qualified message reference.
    pub fn reference(&self) -> &MessageKey {
        &self.reference
    }

    /// Returns the authoritative raw sender of the observed message.
    pub fn sender(&self) -> &ActorKey {
        self.context.sender()
    }

    /// Returns the observed workspace when the provider supplies it.
    pub fn workspace(&self) -> Option<&WorkspaceKey> {
        self.context.workspace()
    }

    /// Returns content observed during this read, without local truncation.
    pub fn content(&self) -> &str {
        self.context.content()
    }

    /// Returns the original creation time, preserving source timestamp precision.
    pub fn created_at(&self) -> DateTime<Utc> {
        self.context.created_at()
    }

    /// Returns descriptive native metadata, which conveys no access authority.
    pub fn native(&self) -> Option<&NativeSupplement> {
        self.native.as_ref()
    }

    /// Validates common source coherence and bounded native metadata.
    ///
    /// # Errors
    /// Rejects invalid identities, inconsistent raw sender/workspace sources, or
    /// oversized native data. Cross-provider represented principals and explicit
    /// references remain descriptive and do not grant access to another room.
    pub fn validate(&self) -> Result<(), ObservationError> {
        self.context.validate(&self.reference)?;
        if let Some(native) = self.native() {
            native.validate()?;
        }
        Ok(())
    }
}

#[derive(Deserialize)]
#[serde(remote = "ContextObservation", deny_unknown_fields)]
struct ContextObservationWire {
    reference: MessageKey,
    context: ContextFields,
    native: Option<NativeSupplement>,
}

impl<'de> Deserialize<'de> for ContextObservation {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = ContextObservationWire::deserialize(deserializer)?;
        value.validate().map_err(serde::de::Error::custom)?;
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{
        ActorId, ActorRef, ConversationId, ConversationRef, MessageId, MessageRef,
        ModuleInstanceId, ModuleSlotId, ProviderId, WorkspaceId, WorkspaceRef,
        context::{
            Availability, ContextMetadata, ObservationExtent, ObservationOrigin, SenderObservation,
            UnavailableReason,
        },
    };

    fn context() -> crate::core::context::ContextObservation {
        let source = ModuleInstanceId::new(
            ProviderId::new("provider").unwrap(),
            ModuleSlotId::new("slot").unwrap(),
        );
        let room = ConversationRef::new(source.clone(), ConversationId::opaque("room").unwrap());
        let created_at = "2026-10-13T12:00:00.123456789Z".parse().unwrap();
        let edited_at = "2026-10-13T13:00:00.987654321Z".parse().unwrap();
        crate::core::context::ContextObservation::new(
            MessageRef::new(room, MessageId::opaque("message").unwrap()),
            SenderObservation::new(
                ActorRef::new(source.clone(), ActorId::opaque("sender").unwrap()),
                "Sender".into(),
                Availability::Known(false),
                Availability::Unavailable(UnavailableReason::NotResolved),
            ),
            "current authoritative body".into(),
            created_at,
            ContextMetadata::new(
                Availability::Known(WorkspaceRef::new(
                    source,
                    WorkspaceId::opaque("workspace").unwrap(),
                )),
                Availability::Known(Some(edited_at)),
                Availability::Unavailable(UnavailableReason::NotReported),
                Availability::Known(vec![]),
                ObservationExtent::new(ObservationOrigin::History),
            ),
        )
    }

    #[test]
    fn context_round_trip_retains_read_metadata_without_occurrence() {
        let original = context();
        let observation = ContextObservation::capture(&original)
            .with_native_supplement(
                NativeSupplement::new("test.context".into(), 1, vec![1, 2, 3]).unwrap(),
            )
            .unwrap();
        let wire = serde_json::to_value(&observation).unwrap();
        assert!(wire.get("key").is_none());
        assert!(wire.get("occurrence").is_none());
        assert_eq!(wire["context"]["extent"]["origin"], "history");
        assert_eq!(
            wire["context"]["edited_at"]["value"],
            "2026-10-13T13:00:00.987654321Z"
        );
        let restored: ContextObservation = serde_json::from_value(wire).unwrap();
        assert_eq!(restored, observation);
        assert_eq!(restored.content(), original.content());
        assert_eq!(restored.created_at(), *original.created_at());
        assert_eq!(
            restored.reference(),
            &MessageKey::from(original.reference())
        );
    }

    #[test]
    fn context_decode_enforces_shared_source_and_unknown_field_rules() {
        let valid = serde_json::to_value(ContextObservation::capture(&context())).unwrap();
        let mut sender = valid.clone();
        sender["context"]["sender"]["reference"]["source"]["slot"] = "other".into();
        assert!(serde_json::from_value::<ContextObservation>(sender).is_err());
        let mut workspace = valid.clone();
        workspace["context"]["workspace"]["value"]["source"]["provider"] = "other".into();
        assert!(serde_json::from_value::<ContextObservation>(workspace).is_err());
        let mut occurrence = valid;
        occurrence["occurrence"] = "created".into();
        assert!(serde_json::from_value::<ContextObservation>(occurrence).is_err());
    }
}
