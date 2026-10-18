//! Provider-independent observations returned by context reads.

use super::{ActorRef, ConversationRef, MessageRef, WorkspaceRef};
use chrono::{DateTime, Utc};

/// Whether the provider observation supplies a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Availability<T> {
    Known(T),
    Unavailable(UnavailableReason),
}
/// Why a value cannot be reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum UnavailableReason {
    NotReported,
    NotResolved,
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "The common availability vocabulary includes unsupported fields; the Discord REST projection reports modeled fields."
        )
    )]
    Unsupported,
}
/// Meaning of a native message reference, independent of its consistency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReferenceRelation {
    Reply,
    Forward,
    Unknown,
}
/// Agreement between explicit coordinates and a resolved observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReferenceConsistency {
    NoResolvedObservation,
    ResolvedOnly,
    InsufficientEvidence,
    Matches,
    Conflicts,
}
/// Read operation that supplied the observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ObservationOrigin {
    GatewayCreate,
    GatewayEdit,
    Exact,
    History,
    Pins,
    NewSince,
    Search,
}
/// Content removed locally from the observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum LocalTruncation {
    None,
}
/// Whether the provider supplied all possible message fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProviderCompleteness {
    Unknown,
}
/// Exclusive history direction; an anchor is requested, not independently observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum HistoryDirection {
    Recent,
    Before(MessageRef),
    After(MessageRef),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SenderObservation {
    reference: ActorRef,
    name: String,
    bot: Availability<bool>,
    represented_identity: Availability<ActorRef>,
}
impl SenderObservation {
    pub(crate) fn new(
        reference: ActorRef,
        name: String,
        bot: Availability<bool>,
        represented_identity: Availability<ActorRef>,
    ) -> Self {
        Self {
            reference,
            name,
            bot,
            represented_identity,
        }
    }
    pub(crate) fn reference(&self) -> &ActorRef {
        &self.reference
    }
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    pub(crate) fn bot(&self) -> &Availability<bool> {
        &self.bot
    }
    pub(crate) fn represented_identity(&self) -> &Availability<ActorRef> {
        &self.represented_identity
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ResolvedReference {
    reference: MessageRef,
    sender: ActorRef,
}
impl ResolvedReference {
    pub(crate) fn new(reference: MessageRef, sender: ActorRef) -> Self {
        Self { reference, sender }
    }
    pub(crate) fn reference(&self) -> &MessageRef {
        &self.reference
    }
    pub(crate) fn sender(&self) -> &ActorRef {
        &self.sender
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ReferenceObservation {
    kind: ReferenceRelation,
    target: Availability<MessageRef>,
    resolved: Availability<ResolvedReference>,
    consistency: ReferenceConsistency,
}
impl ReferenceObservation {
    pub(crate) fn new(
        kind: ReferenceRelation,
        target: Availability<MessageRef>,
        resolved: Availability<ResolvedReference>,
        consistency: ReferenceConsistency,
    ) -> Self {
        Self {
            kind,
            target,
            resolved,
            consistency,
        }
    }
    pub(crate) fn kind(&self) -> ReferenceRelation {
        self.kind
    }
    pub(crate) fn target(&self) -> &Availability<MessageRef> {
        &self.target
    }
    pub(crate) fn resolved(&self) -> &Availability<ResolvedReference> {
        &self.resolved
    }
    pub(crate) fn consistency(&self) -> ReferenceConsistency {
        self.consistency
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct AttachmentSummary {
    name: String,
    media_type: Availability<String>,
    size_bytes: u64,
}
impl AttachmentSummary {
    pub(crate) fn new(name: String, media_type: Availability<String>, size_bytes: u64) -> Self {
        Self {
            name,
            media_type,
            size_bytes,
        }
    }
    pub(crate) fn name(&self) -> &str {
        &self.name
    }
    pub(crate) fn media_type(&self) -> &Availability<String> {
        &self.media_type
    }
    pub(crate) fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ObservationExtent {
    origin: ObservationOrigin,
}
impl ObservationExtent {
    pub(crate) fn new(origin: ObservationOrigin) -> Self {
        Self { origin }
    }
    pub(crate) fn origin(&self) -> ObservationOrigin {
        self.origin
    }
}
impl ObservationExtent {
    pub(crate) fn local_truncation(&self) -> LocalTruncation {
        LocalTruncation::None
    }
    pub(crate) fn provider_completeness(&self) -> ProviderCompleteness {
        ProviderCompleteness::Unknown
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextMetadata {
    workspace: Availability<WorkspaceRef>,
    edited_at: Availability<Option<DateTime<Utc>>>,
    relation: Availability<ReferenceObservation>,
    attachments: Availability<Vec<AttachmentSummary>>,
    observation: ObservationExtent,
}
impl ContextMetadata {
    pub(crate) fn new(
        workspace: Availability<WorkspaceRef>,
        edited_at: Availability<Option<DateTime<Utc>>>,
        relation: Availability<ReferenceObservation>,
        attachments: Availability<Vec<AttachmentSummary>>,
        observation: ObservationExtent,
    ) -> Self {
        Self {
            workspace,
            edited_at,
            relation,
            attachments,
            observation,
        }
    }
    pub(crate) fn workspace(&self) -> &Availability<WorkspaceRef> {
        &self.workspace
    }
    pub(crate) fn edited_at(&self) -> &Availability<Option<DateTime<Utc>>> {
        &self.edited_at
    }
    pub(crate) fn relation(&self) -> &Availability<ReferenceObservation> {
        &self.relation
    }
    pub(crate) fn attachments(&self) -> &Availability<Vec<AttachmentSummary>> {
        &self.attachments
    }
    pub(crate) fn observation(&self) -> &ObservationExtent {
        &self.observation
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ContextObservation {
    reference: MessageRef,
    sender: SenderObservation,
    content: String,
    created_at: DateTime<Utc>,
    metadata: ContextMetadata,
}
impl ContextObservation {
    pub(crate) fn new(
        reference: MessageRef,
        sender: SenderObservation,
        content: String,
        created_at: DateTime<Utc>,
        metadata: ContextMetadata,
    ) -> Self {
        Self {
            reference,
            sender,
            content,
            created_at,
            metadata,
        }
    }
    pub(crate) fn reference(&self) -> &MessageRef {
        &self.reference
    }
    pub(crate) fn sender(&self) -> &SenderObservation {
        &self.sender
    }
    pub(crate) fn content(&self) -> &str {
        &self.content
    }
    pub(crate) fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
}
impl ContextObservation {
    pub(crate) fn workspace(&self) -> &Availability<WorkspaceRef> {
        self.metadata.workspace()
    }
    pub(crate) fn edited_at(&self) -> &Availability<Option<DateTime<Utc>>> {
        self.metadata.edited_at()
    }
    pub(crate) fn relation(&self) -> &Availability<ReferenceObservation> {
        self.metadata.relation()
    }
    pub(crate) fn attachments(&self) -> &Availability<Vec<AttachmentSummary>> {
        self.metadata.attachments()
    }
    pub(crate) fn observation(&self) -> &ObservationExtent {
        self.metadata.observation()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HistoryQuery {
    conversation: ConversationRef,
    direction: HistoryDirection,
    limit: u8,
}
impl HistoryQuery {
    pub(crate) fn new(
        conversation: ConversationRef,
        direction: HistoryDirection,
        limit: u8,
    ) -> Self {
        Self {
            conversation,
            direction,
            limit,
        }
    }
    pub(crate) fn conversation(&self) -> &ConversationRef {
        &self.conversation
    }
    pub(crate) fn direction(&self) -> &HistoryDirection {
        &self.direction
    }
    pub(crate) fn limit(&self) -> u8 {
        self.limit
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PageMetadata {
    query: HistoryQuery,
    count: usize,
    oldest: Option<MessageRef>,
    newest: Option<MessageRef>,
    has_more_hint: bool,
}
impl PageMetadata {
    pub(crate) fn from_observed(query: HistoryQuery, messages: &[MessageRef]) -> Self {
        let count = messages.len();
        let oldest = messages.first().cloned();
        let newest = messages.last().cloned();
        let has_more_hint = query.limit() > 0 && count == usize::from(query.limit());
        Self {
            query,
            count,
            oldest,
            newest,
            has_more_hint,
        }
    }
    pub(crate) fn query(&self) -> &HistoryQuery {
        &self.query
    }
    pub(crate) fn count(&self) -> usize {
        self.count
    }
    pub(crate) fn oldest(&self) -> Option<&MessageRef> {
        self.oldest.as_ref()
    }
    pub(crate) fn newest(&self) -> Option<&MessageRef> {
        self.newest.as_ref()
    }
    pub(crate) fn has_more_hint(&self) -> bool {
        self.has_more_hint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::{ConversationId, MessageId, ModuleInstanceId, ModuleSlotId, ProviderId};
    #[test]
    fn page_metadata_separates_requested_anchor_from_observed_bounds() {
        let source = ModuleInstanceId::new(
            ProviderId::new("test").unwrap(),
            ModuleSlotId::new("secondary").unwrap(),
        );
        let room = ConversationRef::new(source, ConversationId::opaque("room").unwrap());
        let anchor = MessageRef::new(room.clone(), MessageId::opaque("requested").unwrap());
        let query = HistoryQuery::new(room.clone(), HistoryDirection::Before(anchor.clone()), 2);
        let messages = vec![
            MessageRef::new(room.clone(), MessageId::opaque("oldest").unwrap()),
            MessageRef::new(room, MessageId::opaque("newest").unwrap()),
        ];
        let full = PageMetadata::from_observed(query.clone(), &messages);
        assert_eq!(full.count(), 2);
        assert!(full.has_more_hint());
        assert_eq!(full.oldest(), messages.first());
        assert_eq!(full.newest(), messages.last());
        assert_eq!(full.query().direction(), &HistoryDirection::Before(anchor));
        let empty = PageMetadata::from_observed(query, &[]);
        assert_eq!(empty.count(), 0);
        assert!(!empty.has_more_hint());
        assert_eq!(empty.oldest(), None);
        assert_eq!(empty.newest(), None);
    }
}
