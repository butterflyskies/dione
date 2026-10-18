//! Nonempty ordinary deliveries, source-bound managed work, and separate controls.

use super::events::{ControlEvent, MessageCreated, OrdinaryEvent};
use crate::attention::{
    delivery::{AttentionDescriptor, AttentionDirective},
    types::RecordId,
};
use chrono_tz::Tz;

#[derive(Clone, Copy, Debug)]
pub(crate) enum EvidenceVisibility {
    Enabled,
    Disabled,
}
#[derive(Clone, Copy, Debug)]
pub(crate) struct PresentationContext {
    timezone: Option<Tz>,
    evidence: EvidenceVisibility,
}
impl PresentationContext {
    pub(crate) fn new(timezone: Option<Tz>, evidence: EvidenceVisibility) -> Self {
        Self { timezone, evidence }
    }
    pub(crate) fn timezone(self) -> Option<Tz> {
        self.timezone
    }
    pub(crate) fn evidence(self) -> EvidenceVisibility {
        self.evidence
    }
}
#[derive(Clone, Debug)]
pub(crate) struct Delivery {
    kind: DeliveryKind,
    presentation: PresentationContext,
}
#[derive(Clone, Debug)]
pub(crate) enum DeliveryKind {
    Ordinary {
        first: OrdinaryEvent,
        rest: Vec<OrdinaryEvent>,
    },
    Managed {
        message: Box<MessageCreated>,
        directive: AttentionDirective,
        record: RecordId,
    },
    Control(ControlEvent),
}
impl Delivery {
    pub(crate) fn single(event: OrdinaryEvent, presentation: PresentationContext) -> Self {
        Self::batch(event, Vec::new(), presentation)
    }
    pub(crate) fn batch(
        first: OrdinaryEvent,
        rest: Vec<OrdinaryEvent>,
        presentation: PresentationContext,
    ) -> Self {
        Self {
            kind: DeliveryKind::Ordinary { first, rest },
            presentation,
        }
    }
    pub(crate) fn managed(
        message: MessageCreated,
        directive: AttentionDirective,
        record: RecordId,
        presentation: PresentationContext,
    ) -> Self {
        Self {
            kind: DeliveryKind::Managed {
                message: Box::new(message),
                directive,
                record,
            },
            presentation,
        }
    }
    pub(crate) fn control(event: ControlEvent, presentation: PresentationContext) -> Self {
        Self {
            kind: DeliveryKind::Control(event),
            presentation,
        }
    }
    pub(crate) fn kind(&self) -> &DeliveryKind {
        &self.kind
    }
    pub(crate) fn presentation(&self) -> PresentationContext {
        self.presentation
    }
    pub(crate) fn attention(&self) -> Option<AttentionDescriptor<'_>> {
        match &self.kind {
            DeliveryKind::Managed {
                message,
                directive,
                record,
            } => Some(AttentionDescriptor::new(
                record,
                *directive,
                message.source(),
                message.author().actor(),
                message.content().text(),
            )),
            DeliveryKind::Ordinary { .. } | DeliveryKind::Control(_) => None,
        }
    }
}
