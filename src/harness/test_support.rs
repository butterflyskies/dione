//! Typed fixtures shared by adapter and routing tests.

use crate::{
    core::{
        ModuleInstanceId, ModuleSlotId, ProviderId,
        delivery::{Delivery, EvidenceVisibility, PresentationContext},
        events::{ControlEvent, TraceLevel},
    },
    discord::{delivery, events::NotificationEvent},
};
use serde_json::Value;

pub(crate) fn module() -> ModuleInstanceId {
    ModuleInstanceId::new(
        ProviderId::new("discord").unwrap(),
        ModuleSlotId::new("primary").unwrap(),
    )
}
pub(crate) fn context() -> PresentationContext {
    PresentationContext::new(None, EvidenceVisibility::Enabled)
}
pub(crate) fn delivery(event: NotificationEvent) -> Delivery {
    delivery::single(&module(), event, context()).unwrap()
}
pub(crate) fn text(text: &str) -> Delivery {
    Delivery::control(
        ControlEvent::Trace {
            level: TraceLevel::INFO,
            target: "fixture".to_owned(),
            message: text.to_owned(),
            fields: Vec::new(),
        },
        context(),
    )
}
pub(crate) trait FixtureNotification {
    fn fixture_notification(self) -> Value;
    fn fixture_notification_with_evidence(self, enabled: bool) -> Value;
}
impl FixtureNotification for NotificationEvent {
    fn fixture_notification(self) -> Value {
        self.fixture_notification_with_evidence(true)
    }
    fn fixture_notification_with_evidence(self, enabled: bool) -> Value {
        let value = delivery::single(
            &module(),
            self,
            PresentationContext::new(
                None,
                if enabled {
                    EvidenceVisibility::Enabled
                } else {
                    EvidenceVisibility::Disabled
                },
            ),
        )
        .unwrap();
        crate::codex::legacy_delivery::encode(
            &value,
            &crate::codex::LegacyDiscordScope::new(module()),
        )
        .unwrap()
    }
}
