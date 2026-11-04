//! Recipient-scoped authoritative context reads, independent of handling claims.

use super::{
    context_observation::ContextObservation,
    model::{AccessDecision, AuthenticatedRecipient, AuthorizationRevision, OwnerError},
    observation::{ConversationKey, EventKey, MessageKey, ObservationError, RetainedObservation},
    owner::RookHandle,
};
use futures_util::future::BoxFuture;
use serde::{Deserialize, Deserializer, Serialize};
use std::{collections::BTreeSet, error::Error, sync::Arc, time::Duration};
use thiserror::Error;

/// A typed context refusal; failed reads do not settle events or disable subscriptions.
#[derive(Debug, Error)]
#[error("rook context read failed: {kind}")]
pub struct ContextError {
    /// Classification of the failed read, independent of its provider cause.
    pub kind: ContextErrorKind,
    #[source]
    cause: Option<Box<dyn Error + Send + Sync>>,
}
impl ContextError {
    /// Construct a boundary refusal that has no underlying provider cause.
    pub fn new(kind: ContextErrorKind) -> Self {
        Self { kind, cause: None }
    }
    /// Classify a provider failure without discarding its original diagnostic cause.
    pub fn with_cause(kind: ContextErrorKind, cause: impl Error + Send + Sync + 'static) -> Self {
        Self {
            kind,
            cause: Some(Box::new(cause)),
        }
    }
}
/// Read failures remain separate from an empty successful history page.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ContextErrorKind {
    /// The provider does not implement this operation or source instance.
    #[error("context operation is unsupported")]
    Unsupported,
    /// A target or cursor is outside the event's source-qualified conversation.
    #[error("context reference is outside the permitted conversation")]
    InvalidReference,
    /// The authoritative provider has no accessible original with this identity.
    #[error("message was not found")]
    NotFound,
    /// Current recipient or provider policy denies this read.
    #[error("context access denied")]
    AccessDenied,
    /// Current access could not be established; this is not a denial or success.
    #[error("context access verification unavailable")]
    AccessUnavailable,
    /// The provider failed to complete a context read.
    #[error("context provider failed")]
    Provider,
    /// Common metadata or source identity violated its validated contract.
    #[error("context observation is invalid")]
    InvalidObservation,
    /// A provider returned the wrong operation, message, page or duplicate identity.
    #[error("context response does not match the requested operation")]
    InvalidResponse,
    /// A page limit is outside the advertised finite range.
    #[error("context page limit is invalid")]
    InvalidLimit,
    /// The result exceeds the finite tool response budget.
    #[error("context response exceeds the size limit")]
    ResponseTooLarge,
    /// The durable owner refused authorization or is unavailable.
    #[error("context owner refused the operation")]
    Owner,
    /// Tool arguments do not match the typed request schema.
    #[error("invalid context tool arguments")]
    InvalidArguments,
    /// No context tool is registered under this name.
    #[error("unknown context tool")]
    UnknownTool,
    /// The provider exceeded the bounded read deadline.
    #[error("context read timed out")]
    Timeout,
}
impl From<OwnerError> for ContextError {
    fn from(error: OwnerError) -> Self {
        Self::with_cause(ContextErrorKind::Owner, error)
    }
}
impl From<ObservationError> for ContextError {
    fn from(error: ObservationError) -> Self {
        Self::with_cause(ContextErrorKind::InvalidObservation, error)
    }
}

/// An exclusive history direction. The requested anchor is not an observed bound.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "anchor",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ContextDirection {
    /// Most recent messages visible in the permitted conversation.
    Recent,
    /// Messages older than this exact source-qualified anchor.
    Before(MessageKey),
    /// Messages newer than this exact source-qualified anchor.
    After(MessageKey),
}
/// Validated request size, bounded independently of the provider implementation.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PageLimit(u8);
impl PageLimit {
    /// Accept one through one hundred messages per request.
    pub fn new(value: u8) -> Result<Self, ContextError> {
        if !(1..=100).contains(&value) {
            return Err(ContextError::new(ContextErrorKind::InvalidLimit));
        }
        Ok(Self(value))
    }
    /// Borrow the validated provider limit.
    pub fn get(self) -> u8 {
        self.0
    }
}
impl<'de> Deserialize<'de> for PageLimit {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        Self::new(u8::deserialize(decoder)?).map_err(serde::de::Error::custom)
    }
}
/// A provider-neutral exact-message or single-conversation history query.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "query",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum ContextQuery {
    /// Fetch the current authoritative original by exact ID.
    Exact(MessageKey),
    /// Fetch a bounded page in one direction.
    History {
        /// Source-qualified authorized room or thread.
        room: ConversationKey,
        /// Exclusive pagination direction and optional requested anchor.
        direction: ContextDirection,
        /// Maximum returned records.
        limit: PageLimit,
    },
}
impl ContextQuery {
    /// The single conversation whose current access this query requires.
    pub fn room(&self) -> &ConversationKey {
        match self {
            Self::Exact(target) => target.conversation(),
            Self::History { room, .. } => room,
        }
    }
    /// Validate coordinates and prevent cross-conversation cursor substitution.
    pub fn validate(&self) -> Result<(), ContextError> {
        self.room().validate()?;
        match self {
            Self::Exact(target) => target.validate()?,
            Self::History {
                room, direction, ..
            } => match direction {
                ContextDirection::Recent => (),
                ContextDirection::Before(anchor) | ContextDirection::After(anchor) => {
                    anchor.validate()?;
                    if anchor.conversation() != room {
                        return Err(ContextError::new(ContextErrorKind::InvalidReference));
                    }
                }
            },
        }
        Ok(())
    }
}
/// A context query bound to a retained occurrence admitted for this recipient.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRequest {
    /// Retained event authorizing same-conversation context inspection.
    pub event: EventKey,
    /// Exact target or history operation; content cannot widen its scope.
    pub query: ContextQuery,
}
/// A validated page with honest provider pagination hints, not a completeness claim.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextPage {
    query: ContextQuery,
    messages: Vec<ContextObservation>,
    count: usize,
    oldest: Option<MessageKey>,
    newest: Option<MessageKey>,
    has_more_hint: bool,
}
impl ContextPage {
    /// Validate an oldest-first provider page and preserve its requested direction.
    pub fn new(
        query: ContextQuery,
        messages: Vec<ContextObservation>,
        has_more_hint: bool,
    ) -> Result<Self, ContextError> {
        query.validate()?;
        let ContextQuery::History { room, limit, .. } = &query else {
            return Err(ContextError::new(ContextErrorKind::InvalidResponse));
        };
        if messages.len() > usize::from(limit.get()) {
            return Err(ContextError::new(ContextErrorKind::InvalidResponse));
        }
        let mut identities = BTreeSet::new();
        for message in &messages {
            message.validate()?;
            if message.reference().conversation() != room || !identities.insert(message.reference())
            {
                return Err(ContextError::new(ContextErrorKind::InvalidResponse));
            }
        }
        Ok(Self {
            count: messages.len(),
            oldest: messages.first().map(|m| m.reference().clone()),
            newest: messages.last().map(|m| m.reference().clone()),
            query,
            messages,
            has_more_hint,
        })
    }
    /// The operation and anchor requested, separate from returned bounds.
    pub fn query(&self) -> &ContextQuery {
        &self.query
    }
    /// Observations in provider-authoritative oldest-first order.
    pub fn messages(&self) -> &[ContextObservation] {
        &self.messages
    }
    /// Number of observations in this page.
    pub fn count(&self) -> usize {
        self.count
    }
    /// First observed message, if the page is nonempty.
    pub fn oldest(&self) -> Option<&MessageKey> {
        self.oldest.as_ref()
    }
    /// Last observed message, if the page is nonempty.
    pub fn newest(&self) -> Option<&MessageKey> {
        self.newest.as_ref()
    }
    /// Provider pagination hint, not a guarantee that another page exists.
    pub fn has_more_hint(&self) -> bool {
        self.has_more_hint
    }
}
/// Authoritative read results; a missing exact message is an error, not an empty result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", content = "result", rename_all = "snake_case")]
pub enum ContextResponse {
    /// One current original message observation.
    Exact(Box<ContextObservation>),
    /// One bounded history page.
    History(Box<ContextPage>),
}
impl ContextResponse {
    /// Construct one authoritative exact result without enlarging every response variant.
    pub fn exact(observation: ContextObservation) -> Self {
        Self::Exact(Box::new(observation))
    }
    /// Construct one bounded history result.
    pub fn history(page: ContextPage) -> Self {
        Self::History(Box::new(page))
    }
}
/// Capabilities explicitly advertised by a concrete configured provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct ContextCapabilities {
    /// Supports authoritative exact-message reads.
    pub exact: bool,
    /// Supports bounded room/thread history in both directions.
    pub history: bool,
    /// Largest history page accepted by this provider.
    pub max_page_size: u8,
}
/// Trusted provider boundary; adapters must enforce current native read access.
pub trait ContextProvider: Send + Sync + 'static {
    /// Advertise available operations without granting any recipient permission.
    fn capabilities(&self) -> ContextCapabilities;
    /// Read the exact requested source; never substitute event preview text.
    fn read<'a>(
        &'a self,
        query: &'a ContextQuery,
    ) -> BoxFuture<'a, Result<ContextResponse, ContextError>>;
}
pub(super) struct ReadPermit {
    pub(super) observation: RetainedObservation,
    pub(super) revision: AuthorizationRevision,
}
/// Recipient-scoped context access with owner revalidation before disclosure.
#[derive(Clone)]
pub struct ContextService {
    handle: RookHandle,
    provider: Arc<dyn ContextProvider>,
    reads: Arc<tokio::sync::Semaphore>,
}
impl ContextService {
    /// Bind the resident owner to a trusted provider that enforces current native access.
    pub fn new(handle: RookHandle, provider: Arc<dyn ContextProvider>) -> Self {
        Self {
            handle,
            provider,
            reads: Arc::new(tokio::sync::Semaphore::new(4)),
        }
    }
    /// Discover available read operations; this does not authorize a room.
    pub fn capabilities(&self) -> ContextCapabilities {
        self.provider.capabilities()
    }
    /// Read within one admitted conversation and recheck authorization before disclosure.
    ///
    /// Reads need no active claim. Sender filters determine which retained event may
    /// authorize the room; they do not hide other speakers in permitted room history.
    pub async fn read(
        &self,
        recipient: AuthenticatedRecipient,
        request: ContextRequest,
    ) -> Result<ContextResponse, ContextError> {
        request.query.validate()?;
        if request.query.room() != request.event.message().conversation() {
            return Err(ContextError::new(ContextErrorKind::InvalidReference));
        }
        let capabilities = self.capabilities();
        match &request.query {
            ContextQuery::Exact(_) if !capabilities.exact => {
                return Err(ContextError::new(ContextErrorKind::Unsupported));
            }
            ContextQuery::History { .. } if !capabilities.history => {
                return Err(ContextError::new(ContextErrorKind::Unsupported));
            }
            ContextQuery::History { limit, .. } if limit.get() > capabilities.max_page_size => {
                return Err(ContextError::new(ContextErrorKind::InvalidLimit));
            }
            _ => (),
        }
        let _read_slot = self
            .reads
            .acquire()
            .await
            .map_err(|error| ContextError::with_cause(ContextErrorKind::Provider, error))?;
        let permit = self
            .handle
            .context_permit(recipient.clone(), request.event.clone(), None)
            .await?;
        self.check_access(&recipient, &permit).await?;
        let response =
            tokio::time::timeout(Duration::from_secs(10), self.provider.read(&request.query))
                .await
                .map_err(|_| ContextError::new(ContextErrorKind::Timeout))??;
        match (&request.query, &response) {
            (ContextQuery::Exact(target), ContextResponse::Exact(message))
                if target == message.reference() =>
            {
                message.validate()?
            }
            (ContextQuery::History { .. }, ContextResponse::History(page))
                if page.query() == &request.query => {}
            _ => return Err(ContextError::new(ContextErrorKind::InvalidResponse)),
        }
        let bytes = serde_json::to_vec(&response)
            .map_err(|error| ContextError::with_cause(ContextErrorKind::Provider, error))?;
        if bytes.len() > 4 * 1024 * 1024 {
            return Err(ContextError::new(ContextErrorKind::ResponseTooLarge));
        }
        self.check_access(&recipient, &permit).await?;
        self.handle
            .context_permit(recipient, request.event, Some(permit.revision))
            .await?;
        Ok(response)
    }
    async fn check_access(
        &self,
        recipient: &AuthenticatedRecipient,
        permit: &ReadPermit,
    ) -> Result<(), ContextError> {
        match self
            .handle
            .context_access(recipient, &permit.observation)
            .await
        {
            AccessDecision::Allowed => Ok(()),
            AccessDecision::Denied => Err(ContextError::new(ContextErrorKind::AccessDenied)),
            AccessDecision::Unavailable => {
                Err(ContextError::new(ContextErrorKind::AccessUnavailable))
            }
        }
    }
}
