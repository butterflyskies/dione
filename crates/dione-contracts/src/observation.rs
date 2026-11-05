//! Source occurrences independent of attention and delivery order.

use chrono::{DateTime, Utc};

use super::{
    MessageRef,
    context::{Availability, ContextObservation},
};

/// A created occurrence or an edit with its exact provider revision.
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub enum MessageOccurrence {
    /// The provider observed creation of the message.
    Created,
    /// The provider observed an edit at this exact revision.
    Edited {
        /// Revision that must agree with the context edit timestamp.
        revision: DateTime<Utc>,
    },
}

/// A source-qualified occurrence produced through a checked message observation.
///
/// ```compile_fail
/// use dione_contracts::MessageRef;
/// use dione_contracts::observation::{MessageOccurrence, SourceEventIdentity};
/// fn forge(message: MessageRef, occurrence: MessageOccurrence) -> SourceEventIdentity {
///     SourceEventIdentity { message, occurrence }
/// }
/// ```
#[derive(Clone, Debug, Eq, PartialEq, Hash)]
pub struct SourceEventIdentity {
    message: MessageRef,
    occurrence: MessageOccurrence,
}
impl SourceEventIdentity {
    /// Borrows the source-qualified message identity.
    pub fn message(&self) -> &MessageRef {
        &self.message
    }
    /// Borrows the checked occurrence identity.
    pub fn occurrence(&self) -> &MessageOccurrence {
        &self.occurrence
    }
}
/// A descriptive context paired with a matching source occurrence.
#[derive(Clone, Debug)]
pub struct MessageObservation {
    identity: SourceEventIdentity,
    context: ContextObservation,
}

impl MessageObservation {
    /// Rejects an edited occurrence whose revision differs from the context edit timestamp.
    pub fn new(context: ContextObservation, occurrence: MessageOccurrence) -> Option<Self> {
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
    /// Borrows the checked source occurrence identity.
    pub fn identity(&self) -> &SourceEventIdentity {
        &self.identity
    }
    /// Borrows the descriptive provider context.
    pub fn context(&self) -> &ContextObservation {
        &self.context
    }
}
