//! Nonempty ordinary deliveries, source-bound managed work, and separate controls.

use super::events::{ControlEvent, MessageCreated, OrdinaryEvent};
use chrono_tz::Tz;
use dione_attention::{
    delivery::{AttentionDescriptor, AttentionDirective},
    types::RecordId,
};

/// Whether presentation includes retained evidence metadata.
#[derive(Clone, Copy, Debug)]
pub enum EvidenceVisibility {
    /// Include evidence metadata in host presentation.
    Enabled,
    /// Omit evidence metadata from host presentation.
    Disabled,
}
/// Timezone and evidence visibility supplied for host presentation.
#[derive(Clone, Copy, Debug)]
pub struct PresentationContext {
    timezone: Option<Tz>,
    evidence: EvidenceVisibility,
}
impl PresentationContext {
    /// Retains the supplied timezone and evidence visibility.
    pub fn new(timezone: Option<Tz>, evidence: EvidenceVisibility) -> Self {
        Self { timezone, evidence }
    }
    /// Returns the presentation timezone, with no override represented by None.
    pub fn timezone(self) -> Option<Tz> {
        self.timezone
    }
    /// Returns whether presentation includes evidence metadata.
    pub fn evidence(self) -> EvidenceVisibility {
        self.evidence
    }
}
/// A delivery assembled through the ordinary, managed, or control constructors.
///
/// The fields remain private so callers use the existing construction operations.
///
/// ```compile_fail
/// use dione_messages::delivery::{Delivery, DeliveryKind, PresentationContext};
///
/// fn forge(kind: DeliveryKind, presentation: PresentationContext) -> Delivery {
///     Delivery { kind, presentation }
/// }
/// ```
#[derive(Clone, Debug)]
pub struct Delivery {
    kind: DeliveryKind,
    presentation: PresentationContext,
}
/// The retained ordinary batch, managed message, or control notice.
#[derive(Clone, Debug)]
pub enum DeliveryKind {
    /// An ordinary batch with a required first event.
    Ordinary {
        /// The required first event that keeps the batch nonempty.
        first: OrdinaryEvent,
        /// Remaining events in their retained order.
        rest: Vec<OrdinaryEvent>,
    },
    /// A message paired with its recorded attention decision.
    Managed {
        /// The supplied creation observation.
        message: Box<MessageCreated>,
        /// The scheduling directive bound to the recorded admission.
        directive: AttentionDirective,
        /// The recorded decision identity; it is not an authorization token.
        record: RecordId,
    },
    /// A separate host control notice.
    Control(ControlEvent),
}
impl Delivery {
    /// Constructs an ordinary batch containing exactly one event.
    pub fn single(event: OrdinaryEvent, presentation: PresentationContext) -> Self {
        Self::batch(event, Vec::new(), presentation)
    }
    /// Constructs a nonempty ordinary batch from its required first event and remaining events.
    pub fn batch(
        first: OrdinaryEvent,
        rest: Vec<OrdinaryEvent>,
        presentation: PresentationContext,
    ) -> Self {
        Self {
            kind: DeliveryKind::Ordinary { first, rest },
            presentation,
        }
    }
    /// Retains a managed message and its recorded decision without checking current authority.
    pub fn managed(
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
    /// Constructs a separate host control delivery.
    pub fn control(event: ControlEvent, presentation: PresentationContext) -> Self {
        Self {
            kind: DeliveryKind::Control(event),
            presentation,
        }
    }
    /// Borrows the retained delivery shape.
    pub fn kind(&self) -> &DeliveryKind {
        &self.kind
    }
    /// Returns the supplied presentation context.
    pub fn presentation(&self) -> PresentationContext {
        self.presentation
    }
    /// Borrows a descriptor for managed work; ordinary and control deliveries return None.
    ///
    /// The descriptor carries retained source evidence, not proof of current authority.
    pub fn attention(&self) -> Option<AttentionDescriptor<'_>> {
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
