//! Publication commands share the recipient owner's snapshot commit boundary.

use super::{Command, Owner, Response, RookHandle};
use crate::rook::{
    AuthenticatedRecipient, ClaimGrant, EventKey, OwnerError, OwnerErrorKind,
    model::Handling,
    publication::{
        CanonicalPublication, DispatchOutcome, IdempotencyKey, MAX_ALIASES, PreparedPublication,
        PublicationBlock, PublicationCapabilities, PublicationDispatch, PublicationError,
        PublicationErrorKind, PublicationRecord, PublicationRejection, PublicationState,
        PublicationStatus, PublicationUncertainty, RejectionPhase, TextPublication,
    },
};
use std::{collections::BTreeSet, future::Future, pin::Pin, time::Duration};

pub(super) type PublicationFollowup = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;
pub(super) struct PublicationTaskResult {
    completion: PublicationCompletion,
    after_commit: Option<PublicationFollowup>,
}

pub(super) enum PublicationCommand {
    Publish(AuthenticatedRecipient, Box<PublishRequest>),
    Status(AuthenticatedRecipient, EventKey),
}

pub(super) struct PublishRequest {
    grant: ClaimGrant,
    request: TextPublication,
}

pub(super) struct PendingPublicationCompletion {
    completion: PublicationCompletion,
    blocked: PublicationBlock,
}
impl PendingPublicationCompletion {
    fn new(completion: PublicationCompletion, error: &OwnerError) -> Self {
        let blocked = match &error.kind {
            OwnerErrorKind::Storage(_) | OwnerErrorKind::Unavailable => PublicationBlock::Storage,
            OwnerErrorKind::Capacity => PublicationBlock::Capacity,
            OwnerErrorKind::Publication(_)
            | OwnerErrorKind::InvalidState
            | OwnerErrorKind::Decode(_)
            | OwnerErrorKind::TrailingInput(_)
            | OwnerErrorKind::Encoding(_)
            | OwnerErrorKind::Denied
            | OwnerErrorKind::AccessUnavailable
            | OwnerErrorKind::ConflictingRevision
            | OwnerErrorKind::RequestConflict
            | OwnerErrorKind::UnknownEvent
            | OwnerErrorKind::InvalidClaim
            | OwnerErrorKind::WorkerJoin(_)
            | OwnerErrorKind::Exhausted => PublicationBlock::InvalidState,
        };
        Self {
            completion,
            blocked,
        }
    }
}

#[derive(Clone)]
pub(super) enum PublicationCompletion {
    Prepared {
        event: EventKey,
        grant: ClaimGrant,
        result: Result<Box<PreparedPublication>, PublicationRejection>,
    },
    Dispatched {
        event: EventKey,
        outcome: DispatchOutcome,
    },
}

impl RookHandle {
    /// The installed adapter's text-only limits. No startup activation is implied.
    pub fn publication_capabilities(&self) -> PublicationCapabilities {
        self.publication_capabilities
    }

    /// Reserve the event's single default-reply slot or recover its existing outcome.
    ///
    /// Cancellation after mailbox acceptance does not cancel persistence or dispatch.
    /// Matching retries return the retained slot and cannot resend an uncertain attempt.
    pub async fn publish(
        &self,
        recipient: AuthenticatedRecipient,
        grant: ClaimGrant,
        request: TextPublication,
    ) -> Result<PublicationStatus, OwnerError> {
        match self
            .request(Command::Publication(Box::new(PublicationCommand::Publish(
                recipient,
                Box::new(PublishRequest { grant, request }),
            ))))
            .await?
        {
            Response::Publication(Some(status)) => Ok(status),
            _ => Err(OwnerError::invalid()),
        }
    }

    /// Inspect an authenticated recipient's retained slot without issuing a POST.
    pub async fn publication_status(
        &self,
        recipient: AuthenticatedRecipient,
        event: EventKey,
    ) -> Result<Option<PublicationStatus>, OwnerError> {
        match self
            .request(Command::Publication(Box::new(PublicationCommand::Status(
                recipient, event,
            ))))
            .await?
        {
            Response::Publication(status) => Ok(status),
            _ => Err(OwnerError::invalid()),
        }
    }
}

fn publication_error(kind: PublicationErrorKind) -> OwnerError {
    PublicationError::new(kind).into()
}

impl Owner {
    pub(super) async fn publication_command(
        &mut self,
        command: PublicationCommand,
    ) -> Result<Option<PublicationStatus>, OwnerError> {
        match command {
            PublicationCommand::Status(recipient, event) => {
                self.authenticate(&recipient)?;
                Ok(self.publication_status(&event))
            }
            PublicationCommand::Publish(recipient, request) => {
                let PublishRequest { grant, request } = *request;
                self.authenticate(&recipient)?;
                let (request, key) = CanonicalPublication::from_request(request)?;
                self.reserve_publication(request, key, grant)
                    .await
                    .map(Some)
            }
        }
    }

    fn publication_index(&self, event: &EventKey) -> Option<usize> {
        self.state
            .publications
            .iter()
            .position(|record| record.request.event() == event)
    }

    fn publication_status(&self, event: &EventKey) -> Option<PublicationStatus> {
        let record = &self.state.publications[self.publication_index(event)?];
        let mut outcome = record.state.clone();
        let mut pending = self.pending;
        let mut blocked = None;
        for completion in &self.publication_completions {
            let completed = match &completion.completion {
                PublicationCompletion::Prepared { event, .. }
                | PublicationCompletion::Dispatched { event, .. } => event,
            };
            if completed == event {
                pending = true;
                blocked = Some(completion.blocked);
            }
            if let PublicationCompletion::Dispatched {
                event: completed,
                outcome: reported,
            } = &completion.completion
                && completed == event
            {
                outcome = completion_state(&record.request, reported.clone());
                pending = true;
            }
        }
        Some(PublicationStatus {
            outcome,
            durability_pending: pending,
            blocked,
        })
    }

    async fn publication_authority(
        &self,
        request: &CanonicalPublication,
        grant: &ClaimGrant,
    ) -> Result<(), OwnerError> {
        let event = &self.state.events[self.event_index(request.event())?];
        if grant.authorization_revision != self.state.revision
            || grant.expires_at <= self.now()
            || !matches!(&event.handling, Handling::Claimed { grant: current, .. } if current == grant)
        {
            return Err(OwnerError::new(OwnerErrorKind::Denied));
        }
        self.require_access(&event.observation).await?;
        if grant.expires_at <= self.now() {
            return Err(OwnerError::new(OwnerErrorKind::Denied));
        }
        Ok(())
    }

    async fn reserve_publication(
        &mut self,
        request: CanonicalPublication,
        key: IdempotencyKey,
        grant: ClaimGrant,
    ) -> Result<PublicationStatus, OwnerError> {
        if self
            .state
            .publications
            .iter()
            .any(|record| record.aliases.contains(&key) && record.request != request)
        {
            return Err(publication_error(PublicationErrorKind::IdempotencyConflict));
        }
        if let Some(index) = self.publication_index(request.event()) {
            let record = &self.state.publications[index];
            if record.request != request {
                return Err(publication_error(PublicationErrorKind::PublicationConflict));
            }
            if !record.aliases.contains(&key) {
                if record.aliases.len() >= MAX_ALIASES {
                    return Err(OwnerError::new(OwnerErrorKind::Capacity));
                }
                let mut next = self.state.clone();
                next.publications[index].aliases.insert(key);
                self.commit(next).await?;
            }
            if matches!(
                self.state.publications[index].state,
                PublicationState::Reserved
            ) && !self.publication_active.contains(request.event())
            {
                self.publication_authority(&request, &grant).await?;
                self.start_preparation(request.clone(), grant)?;
            }
            return self
                .publication_status(request.event())
                .ok_or_else(OwnerError::invalid);
        }
        let provider = self
            .publication_provider
            .as_ref()
            .ok_or_else(|| publication_error(PublicationErrorKind::Unsupported))?;
        let capabilities = provider.capabilities();
        if !capabilities.text
            || capabilities.attachments
            || request.text().len() > capabilities.max_text_bytes
        {
            return Err(publication_error(PublicationErrorKind::Unsupported));
        }
        if self.publication_active.len() + self.publication_bookkeeping.len() >= 4 {
            return Err(OwnerError::new(OwnerErrorKind::Capacity));
        }
        self.publication_authority(&request, &grant).await?;
        let mut next = self.state.clone();
        next.publications.push(PublicationRecord {
            digest: request.digest()?,
            aliases: BTreeSet::from([key]),
            request: request.clone(),
            prepared: None,
            state: PublicationState::Reserved,
        });
        next.high_water = self.now();
        self.commit(next).await?;
        self.start_preparation(request.clone(), grant)?;
        self.publication_status(request.event())
            .ok_or_else(OwnerError::invalid)
    }

    fn start_preparation(
        &mut self,
        request: CanonicalPublication,
        grant: ClaimGrant,
    ) -> Result<(), OwnerError> {
        if self.publication_active.len() + self.publication_bookkeeping.len() >= 4 {
            return Err(OwnerError::new(OwnerErrorKind::Capacity));
        }
        let provider = self
            .publication_provider
            .clone()
            .ok_or_else(|| publication_error(PublicationErrorKind::Unsupported))?;
        self.publication_active.insert(request.event().clone());
        self.publication_tasks.spawn(async move {
            let result = tokio::time::timeout(Duration::from_secs(30), provider.prepare(&request))
                .await
                .unwrap_or(Err(PublicationRejection::PreparationTimeout));
            PublicationTaskResult {
                completion: PublicationCompletion::Prepared {
                    event: request.event().clone(),
                    grant,
                    result: result.map(Box::new),
                },
                after_commit: None,
            }
        });
        Ok(())
    }

    pub(super) async fn retain_publication_task(&mut self, result: PublicationTaskResult) {
        if let PublicationCompletion::Dispatched { event, outcome } = &result.completion
            && let Some(index) = self.publication_index(event)
        {
            let definite = match outcome {
                DispatchOutcome::Published(receipt) => {
                    receipt.valid_for(&self.state.publications[index].request)
                }
                DispatchOutcome::NotSent(_) | DispatchOutcome::Refused(_) => true,
                DispatchOutcome::Unknown(_) => false,
            };
            if definite && let Some(followup) = result.after_commit {
                let (release, committed) = tokio::sync::oneshot::channel();
                self.publication_followups.insert(event.clone(), release);
                self.publication_bookkeeping.spawn(async move {
                    if committed.await.is_ok()
                        && tokio::time::timeout(Duration::from_secs(5), followup)
                            .await
                            .is_err()
                    {
                        tracing::warn!("publication bookkeeping timed out after durable outcome");
                    }
                });
            }
        }
        self.retain_publication_completion(result.completion).await;
    }

    fn start_publication_followup(&mut self, event: &EventKey) {
        if let Some(release) = self.publication_followups.remove(event) {
            let _ = release.send(());
        }
    }

    pub(super) async fn retain_publication_completion(
        &mut self,
        completion: PublicationCompletion,
    ) {
        if let Err(error) = self.complete_publication(completion.clone()).await {
            tracing::warn!(error = %error, "publication completion awaits durable storage");
            self.publication_completions
                .push_back(PendingPublicationCompletion::new(completion, &error));
        }
    }

    pub(super) async fn retry_publication_completions(&mut self) -> Result<(), OwnerError> {
        let count = self.publication_completions.len();
        for _ in 0..count {
            let Some(completion) = self.publication_completions.pop_front() else {
                break;
            };
            if let Err(error) = self
                .complete_publication(completion.completion.clone())
                .await
            {
                self.publication_completions
                    .push_front(PendingPublicationCompletion::new(
                        completion.completion,
                        &error,
                    ));
                return Err(error);
            }
        }
        Ok(())
    }

    async fn complete_publication(
        &mut self,
        completion: PublicationCompletion,
    ) -> Result<(), OwnerError> {
        match completion {
            PublicationCompletion::Prepared {
                event,
                grant,
                result,
            } => {
                let index = self
                    .publication_index(&event)
                    .ok_or_else(OwnerError::invalid)?;
                let record = &self.state.publications[index];
                if !matches!(
                    record.state,
                    PublicationState::Reserved | PublicationState::Dispatching
                ) {
                    self.publication_active.remove(&event);
                    return Ok(());
                }
                let request = record.request.clone();
                let prepared = match result {
                    Ok(prepared)
                        if prepared.validate().is_ok() && prepared.request() == &request =>
                    {
                        *prepared
                    }
                    Ok(_) => {
                        return self
                            .reject_preparation(index, PublicationRejection::InvalidPreparation)
                            .await;
                    }
                    Err(reason) => return self.reject_preparation(index, reason).await,
                };
                if let Err(error) = self.publication_authority(&request, &grant).await {
                    let reason = match error.kind {
                        OwnerErrorKind::AccessUnavailable => {
                            PublicationRejection::AccessUnavailable
                        }
                        OwnerErrorKind::Publication(_)
                        | OwnerErrorKind::Storage(_)
                        | OwnerErrorKind::InvalidState
                        | OwnerErrorKind::Decode(_)
                        | OwnerErrorKind::TrailingInput(_)
                        | OwnerErrorKind::Encoding(_)
                        | OwnerErrorKind::Capacity
                        | OwnerErrorKind::Denied
                        | OwnerErrorKind::ConflictingRevision
                        | OwnerErrorKind::RequestConflict
                        | OwnerErrorKind::UnknownEvent
                        | OwnerErrorKind::InvalidClaim
                        | OwnerErrorKind::Unavailable
                        | OwnerErrorKind::WorkerJoin(_)
                        | OwnerErrorKind::Exhausted => PublicationRejection::AuthorityChanged,
                    };
                    return self.reject_preparation(index, reason).await;
                }
                let provider = self
                    .publication_provider
                    .clone()
                    .ok_or_else(|| publication_error(PublicationErrorKind::Unsupported))?;
                let mut next = self.state.clone();
                next.publications[index].prepared = Some(prepared.clone());
                next.publications[index].state = PublicationState::Dispatching;
                next.high_water = self.now();
                self.commit(next).await?;
                if grant.expires_at <= self.now() {
                    return self
                        .reject_preparation(index, PublicationRejection::AuthorityChanged)
                        .await;
                }
                self.publication_tasks.spawn(async move {
                    let report =
                        tokio::time::timeout(Duration::from_secs(45), provider.dispatch(&prepared))
                            .await
                            .unwrap_or_else(|_| {
                                PublicationDispatch::new(DispatchOutcome::Unknown(
                                    PublicationUncertainty::Timeout,
                                ))
                            });
                    let (outcome, after_commit) = report.into_parts();
                    PublicationTaskResult {
                        completion: PublicationCompletion::Dispatched { event, outcome },
                        after_commit,
                    }
                });
                Ok(())
            }
            PublicationCompletion::Dispatched { event, outcome } => {
                let index = self
                    .publication_index(&event)
                    .ok_or_else(OwnerError::invalid)?;
                if !matches!(
                    self.state.publications[index].state,
                    PublicationState::Dispatching
                ) {
                    self.sync().await?;
                    if matches!(
                        self.state.publications[index].state,
                        PublicationState::Published { .. } | PublicationState::Rejected { .. }
                    ) {
                        self.start_publication_followup(&event);
                    }
                    self.publication_active.remove(&event);
                    return Ok(());
                }
                let mut next = self.state.clone();
                next.publications[index].state =
                    completion_state(&next.publications[index].request, outcome);
                next.high_water = self.now();
                self.commit(next).await?;
                if matches!(
                    self.state.publications[index].state,
                    PublicationState::Published { .. } | PublicationState::Rejected { .. }
                ) {
                    self.start_publication_followup(&event);
                }
                self.publication_active.remove(&event);
                Ok(())
            }
        }
    }

    async fn reject_preparation(
        &mut self,
        index: usize,
        reason: PublicationRejection,
    ) -> Result<(), OwnerError> {
        let mut next = self.state.clone();
        next.publications[index].state = PublicationState::Rejected {
            phase: RejectionPhase::BeforeDispatch,
            reason,
        };
        let event = next.publications[index].request.event().clone();
        self.commit(next).await?;
        self.publication_active.remove(&event);
        Ok(())
    }
}

fn completion_state(request: &CanonicalPublication, outcome: DispatchOutcome) -> PublicationState {
    match outcome {
        DispatchOutcome::Published(receipt) if receipt.valid_for(request) => {
            PublicationState::Published { receipt }
        }
        DispatchOutcome::Published(_) => PublicationState::Unknown {
            reason: PublicationUncertainty::InvalidReceipt,
        },
        DispatchOutcome::NotSent(reason) => PublicationState::Rejected {
            phase: RejectionPhase::BeforeDispatch,
            reason,
        },
        DispatchOutcome::Refused(reason) => PublicationState::Rejected {
            phase: RejectionPhase::ProviderRefused,
            reason,
        },
        DispatchOutcome::Unknown(reason) => PublicationState::Unknown { reason },
    }
}
