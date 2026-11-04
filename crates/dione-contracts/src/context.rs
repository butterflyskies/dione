//! Provider-independent observations returned by context reads.

use super::{ActorRef, ConversationRef, MessageRef, WorkspaceRef};
use chrono::{DateTime, Utc};

/// Whether the provider observation supplies a value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability<T> {
    /// The provider supplied this value.
    Known(T),
    /// The provider could not supply a value for the stated reason.
    Unavailable(UnavailableReason),
}
/// Why a value cannot be reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnavailableReason {
    /// The provider did not report this field.
    NotReported,
    /// The referenced value was not resolved.
    NotResolved,
    /// The provider does not support this field.
    Unsupported,
}
/// Meaning of a native message reference, independent of its consistency.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceRelation {
    /// The message replies to another message.
    Reply,
    /// The message forwards another message.
    Forward,
    /// The provider does not establish this relationship.
    Unknown,
}
/// Agreement between explicit coordinates and a resolved observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReferenceConsistency {
    /// No resolved target observation is available.
    NoResolvedObservation,
    /// Only the resolved target supplies coordinates.
    ResolvedOnly,
    /// The available coordinates cannot establish agreement.
    InsufficientEvidence,
    /// Explicit coordinates agree with the resolved observation.
    Matches,
    /// Explicit coordinates disagree with the resolved observation.
    Conflicts,
}
/// Read operation that supplied the observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ObservationOrigin {
    /// The provider gateway reported a created message.
    GatewayCreate,
    /// The provider gateway reported an edited message.
    GatewayEdit,
    /// An exact-message read supplied the observation.
    Exact,
    /// A history read supplied the observation.
    History,
    /// A pinned-message read supplied the observation.
    Pins,
    /// A new-since read supplied the observation.
    NewSince,
    /// A search supplied the observation.
    Search,
}
/// Content removed locally from the observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalTruncation {
    /// No content was removed locally.
    None,
}
/// Whether the provider supplied all possible message fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProviderCompleteness {
    /// The provider supplies no completeness guarantee.
    Unknown,
}
/// Exclusive history direction; an anchor is requested, not independently observed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HistoryDirection {
    /// Read the most recent messages.
    Recent,
    /// Read messages before the requested anchor.
    Before(MessageRef),
    /// Read messages after the requested anchor.
    After(MessageRef),
}

/// A provider-reported sender and descriptive identity metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SenderObservation {
    reference: ActorRef,
    name: String,
    bot: Availability<bool>,
    represented_identity: Availability<ActorRef>,
}
impl SenderObservation {
    /// Records the sender identity and provider-reported descriptive fields.
    pub fn new(
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
    /// Borrows the source-qualified identity.
    pub fn reference(&self) -> &ActorRef {
        &self.reference
    }
    /// Borrows the provider-reported name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Borrows whether the provider reports this sender as a bot.
    pub fn bot(&self) -> &Availability<bool> {
        &self.bot
    }
    /// Borrows the separately reported represented identity.
    pub fn represented_identity(&self) -> &Availability<ActorRef> {
        &self.represented_identity
    }
}

/// A resolved message reference and its observed sender.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedReference {
    reference: MessageRef,
    sender: ActorRef,
}
impl ResolvedReference {
    /// Pairs a resolved message with its observed sender.
    pub fn new(reference: MessageRef, sender: ActorRef) -> Self {
        Self { reference, sender }
    }
    /// Borrows the source-qualified identity.
    pub fn reference(&self) -> &MessageRef {
        &self.reference
    }
    /// Borrows the resolved message sender.
    pub fn sender(&self) -> &ActorRef {
        &self.sender
    }
}

/// A native reference with its independently reported resolution and agreement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReferenceObservation {
    kind: ReferenceRelation,
    target: Availability<MessageRef>,
    resolved: Availability<ResolvedReference>,
    consistency: ReferenceConsistency,
}
impl ReferenceObservation {
    /// Records the native reference and its independent resolution evidence.
    pub fn new(
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
    /// Returns the native reference relation.
    pub fn kind(&self) -> ReferenceRelation {
        self.kind
    }
    /// Borrows the explicit target coordinates, if reported.
    pub fn target(&self) -> &Availability<MessageRef> {
        &self.target
    }
    /// Borrows the resolved target observation, if available.
    pub fn resolved(&self) -> &Availability<ResolvedReference> {
        &self.resolved
    }
    /// Returns agreement between explicit and resolved coordinates.
    pub fn consistency(&self) -> ReferenceConsistency {
        self.consistency
    }
}

/// Descriptive attachment metadata without downloaded content.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AttachmentSummary {
    name: String,
    media_type: Availability<String>,
    size_bytes: u64,
}
impl AttachmentSummary {
    /// Records the attachment name, reported media type, and exact byte size.
    pub fn new(name: String, media_type: Availability<String>, size_bytes: u64) -> Self {
        Self {
            name,
            media_type,
            size_bytes,
        }
    }
    /// Borrows the provider-reported name.
    pub fn name(&self) -> &str {
        &self.name
    }
    /// Borrows the provider-reported media type.
    pub fn media_type(&self) -> &Availability<String> {
        &self.media_type
    }
    /// Returns the exact attachment size in bytes.
    pub fn size_bytes(&self) -> u64 {
        self.size_bytes
    }
}

/// The read operation and completeness facts of a provider observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ObservationExtent {
    origin: ObservationOrigin,
}
impl ObservationExtent {
    /// Records the operation that supplied this observation.
    pub fn new(origin: ObservationOrigin) -> Self {
        Self { origin }
    }
    /// Returns the operation that supplied this observation.
    pub fn origin(&self) -> ObservationOrigin {
        self.origin
    }
}
impl ObservationExtent {
    /// Returns whether content was removed locally.
    pub fn local_truncation(&self) -> LocalTruncation {
        LocalTruncation::None
    }
    /// Returns the provider field-completeness claim.
    pub fn provider_completeness(&self) -> ProviderCompleteness {
        ProviderCompleteness::Unknown
    }
}

/// Provider-reported metadata accompanying a common message observation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextMetadata {
    workspace: Availability<WorkspaceRef>,
    edited_at: Availability<Option<DateTime<Utc>>>,
    relation: Availability<ReferenceObservation>,
    attachments: Availability<Vec<AttachmentSummary>>,
    observation: ObservationExtent,
}
impl ContextMetadata {
    /// Records provider metadata without inventing unavailable values.
    pub fn new(
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
    /// Borrows the provider-reported workspace identity.
    pub fn workspace(&self) -> &Availability<WorkspaceRef> {
        &self.workspace
    }
    /// Borrows the provider-reported edit timestamp, including a known absence.
    pub fn edited_at(&self) -> &Availability<Option<DateTime<Utc>>> {
        &self.edited_at
    }
    /// Borrows the provider-reported reference metadata.
    pub fn relation(&self) -> &Availability<ReferenceObservation> {
        &self.relation
    }
    /// Borrows the reported attachment list, including a known empty list.
    pub fn attachments(&self) -> &Availability<Vec<AttachmentSummary>> {
        &self.attachments
    }
    /// Borrows the observation origin and completeness facts.
    pub fn observation(&self) -> &ObservationExtent {
        &self.observation
    }
}

/// Descriptive message context without current-access or action authority.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ContextObservation {
    reference: MessageRef,
    sender: SenderObservation,
    content: String,
    created_at: DateTime<Utc>,
    metadata: ContextMetadata,
}
impl ContextObservation {
    /// Records the message context supplied by an adapter.
    pub fn new(
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
    /// Borrows the source-qualified identity.
    pub fn reference(&self) -> &MessageRef {
        &self.reference
    }
    /// Borrows the observed sender metadata.
    pub fn sender(&self) -> &SenderObservation {
        &self.sender
    }
    /// Borrows the observed message content.
    pub fn content(&self) -> &str {
        &self.content
    }
    /// Borrows the provider-reported creation timestamp.
    pub fn created_at(&self) -> &DateTime<Utc> {
        &self.created_at
    }
}
impl ContextObservation {
    /// Borrows the provider-reported workspace identity.
    pub fn workspace(&self) -> &Availability<WorkspaceRef> {
        self.metadata.workspace()
    }
    /// Borrows the provider-reported edit timestamp, including a known absence.
    pub fn edited_at(&self) -> &Availability<Option<DateTime<Utc>>> {
        self.metadata.edited_at()
    }
    /// Borrows the provider-reported reference metadata.
    pub fn relation(&self) -> &Availability<ReferenceObservation> {
        self.metadata.relation()
    }
    /// Borrows the reported attachment list, including a known empty list.
    pub fn attachments(&self) -> &Availability<Vec<AttachmentSummary>> {
        self.metadata.attachments()
    }
    /// Borrows the observation origin and completeness facts.
    pub fn observation(&self) -> &ObservationExtent {
        self.metadata.observation()
    }
}

/// A source-qualified history request and its exclusive direction.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HistoryQuery {
    conversation: ConversationRef,
    direction: HistoryDirection,
    limit: u8,
}
impl HistoryQuery {
    /// Records the requested conversation, direction, and limit.
    pub fn new(conversation: ConversationRef, direction: HistoryDirection, limit: u8) -> Self {
        Self {
            conversation,
            direction,
            limit,
        }
    }
    /// Borrows the requested source-qualified conversation.
    pub fn conversation(&self) -> &ConversationRef {
        &self.conversation
    }
    /// Borrows the requested history direction.
    pub fn direction(&self) -> &HistoryDirection {
        &self.direction
    }
    /// Returns the requested message limit.
    pub fn limit(&self) -> u8 {
        self.limit
    }
}

/// Observed history bounds kept separate from the requested anchor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PageMetadata {
    query: HistoryQuery,
    count: usize,
    oldest: Option<MessageRef>,
    newest: Option<MessageRef>,
    has_more_hint: bool,
}
impl PageMetadata {
    /// Summarizes observed references without treating the requested anchor as observed.
    pub fn from_observed(query: HistoryQuery, messages: &[MessageRef]) -> Self {
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
    /// Borrows the request that produced this page.
    pub fn query(&self) -> &HistoryQuery {
        &self.query
    }
    /// Returns the number of observed message references.
    pub fn count(&self) -> usize {
        self.count
    }
    /// Borrows the first observed reference, if any.
    pub fn oldest(&self) -> Option<&MessageRef> {
        self.oldest.as_ref()
    }
    /// Borrows the last observed reference, if any.
    pub fn newest(&self) -> Option<&MessageRef> {
        self.newest.as_ref()
    }
    /// Returns whether a nonzero requested limit was filled.
    pub fn has_more_hint(&self) -> bool {
        self.has_more_hint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConversationId, MessageId, ModuleInstanceId, ModuleSlotId, ProviderId};
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
