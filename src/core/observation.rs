//! Source occurrences independent of attention and delivery order.

use chrono::{DateTime, Utc};

use super::{
    MessageRef,
    context::{Availability, ContextObservation},
};

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) enum MessageOccurrence {
    Created,
    Edited { revision: DateTime<Utc> },
}

#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub(crate) struct SourceEventIdentity {
    message: MessageRef,
    occurrence: MessageOccurrence,
}
impl SourceEventIdentity {
    pub(crate) fn message(&self) -> &MessageRef {
        &self.message
    }
    pub(crate) fn occurrence(&self) -> &MessageOccurrence {
        &self.occurrence
    }
}
#[derive(Clone, Debug)]
pub(crate) struct MessageObservation {
    identity: SourceEventIdentity,
    context: ContextObservation,
}

impl MessageObservation {
    pub(crate) fn new(context: ContextObservation, occurrence: MessageOccurrence) -> Option<Self> {
        if let MessageOccurrence::Edited { revision } = &occurrence
            && context.edited_at() != &Availability::Known(Some(*revision))
        {
            return None;
        }
        Some(Self {
            identity: SourceEventIdentity {
                message: context.reference().clone(),
                occurrence,
            },
            context,
        })
    }
    pub(crate) fn identity(&self) -> &SourceEventIdentity {
        &self.identity
    }
    pub(crate) fn context(&self) -> &ContextObservation {
        &self.context
    }
}
