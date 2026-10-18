//! Legacy payload validation and projection over one shared durable owner.
use super::{
    AttentionInvalidation, AttentionInvalidationResult, CodexQueueError, CodexThreadId, ConsumerId,
    ConsumerRegistrationResult, DeliveryToken, HandoffResult, LeasedEvent, LegacyDiscordScope,
    LiveQueueError, MAX_ATTENTION_DEFER, MAX_CONSUMER_TTL, MAX_LEASE, MAX_WAIT,
    MIN_ATTENTION_DEFER, QueueStatus,
    queue_codec::{LegacyPayload, LegacyQueueCodec, LegacyReplayIdentity},
};
use crate::{
    delivery::ProviderEventKey,
    durable::{
        self, Admission, AdmissionOutcome, DurableQueue, EndpointId, QueueErrorKind, QueueOptions,
        ResidentHandle,
    },
};
use camino::Utf8Path;
use chrono::TimeDelta;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::sync::watch;

/// The legacy Codex facade; all mutable queue state belongs to the common owner.
#[derive(Clone)]
pub struct CodexEventQueue {
    scope: LegacyDiscordScope,
    inner: DurableQueue<LegacyPayload, LegacyReplayIdentity>,
}
/// Owner-issued capability for the prepared resident worker.
#[derive(Clone)]
pub(crate) struct CodexResidentHandle {
    inner: ResidentHandle<LegacyPayload, LegacyReplayIdentity>,
}
/// Owner-issued binding control; obtaining it does not register a consumer.
#[derive(Clone)]
pub(crate) struct CodexBindingController {
    inner: durable::BindingController<LegacyPayload, LegacyReplayIdentity>,
}
/// Admission facts remain distinct from known rejection and lost completion.
#[derive(Debug)]
pub(crate) enum LiveAdmissionOutcome {
    Committed,
    CommittedDurabilityUncertain(CodexQueueError),
    Duplicate,
    CompletionUnknown(tokio::task::JoinError),
}
fn options() -> QueueOptions<LegacyReplayIdentity> {
    QueueOptions {
        file_name: "codex-inbox.json",
        lock_name: "codex-inbox.lock",
        consumer_prefix: "codex-consumer-",
        resident_label: "dione-live-app-server",
        replay_class: LegacyReplayIdentity::class,
        rolling_limit: 10_000,
        timed_limit: 10_000,
        timed_retention: TimeDelta::days(30),
    }
}
fn leased(value: durable::LeasedEvent<LegacyPayload>) -> LeasedEvent {
    let attention = value.payload.scheduling();
    LeasedEvent {
        event_id: value.event_id,
        delivery_token: value.delivery_token,
        lease_expires_at: value.lease_expires_at,
        consumer_id: value.consumer_id,
        event: value.payload.into_value(),
        attention,
    }
}
fn endpoint(value: CodexThreadId) -> Result<EndpointId, CodexQueueError> {
    EndpointId::new(value.as_str()).map_err(|error| {
        durable::QueueError::new("validate Codex endpoint", QueueErrorKind::Identity(error)).into()
    })
}
impl CodexEventQueue {
    /// Opens the legacy format synchronously. Async callers should use open.
    pub fn load(state_dir: &Utf8Path, scope: LegacyDiscordScope) -> Result<Self, CodexQueueError> {
        let inner = DurableQueue::load(
            state_dir,
            Arc::new(LegacyQueueCodec::new(scope.clone())),
            options(),
        )?;
        Ok(Self { scope, inner })
    }
    /// Opens and decodes the store outside async runtime workers.
    pub async fn open(
        state_dir: &Utf8Path,
        scope: LegacyDiscordScope,
    ) -> Result<Self, CodexQueueError> {
        let inner = DurableQueue::open(
            state_dir.to_owned(),
            Arc::new(LegacyQueueCodec::new(scope.clone())),
            options(),
        )
        .await?;
        Ok(Self { scope, inner })
    }
    pub(crate) fn binding_controller(&self) -> CodexBindingController {
        CodexBindingController {
            inner: self.inner.binding_controller(),
        }
    }
    pub(crate) fn legacy_scope(&self) -> &LegacyDiscordScope {
        &self.scope
    }
    /// Persists public work; false is a known duplicate, while an error may retain a commit.
    pub async fn enqueue(&self, value: Value) -> Result<bool, CodexQueueError> {
        let scope = self.scope.clone();
        self.inner
            .enqueue_with::<CodexQueueError>(move || {
                let replay_key = LegacyReplayIdentity::from_payload(&value);
                let payload = LegacyPayload::new(value, &scope)?;
                let deferrable = payload.attention().is_some();
                Ok(Admission {
                    payload,
                    replay_key,
                    deferrable,
                })
            })
            .await
    }
    pub(crate) async fn enqueue_live(
        &self,
        value: Value,
        key: ProviderEventKey,
    ) -> Result<LiveAdmissionOutcome, LiveQueueError> {
        let scope = self.scope.clone();
        match self
            .inner
            .enqueue_resident_with::<LiveQueueError>(
                Some(LegacyReplayIdentity::Provider(key)),
                move || {
                    let payload = LegacyPayload::new(value, &scope)?;
                    let deferrable = payload.attention().is_some();
                    Ok((payload, deferrable))
                },
            )
            .await
        {
            Ok(AdmissionOutcome::Committed) => Ok(LiveAdmissionOutcome::Committed),
            Ok(AdmissionOutcome::Duplicate) => Ok(LiveAdmissionOutcome::Duplicate),
            Ok(AdmissionOutcome::CommittedDurabilityUncertain(error)) => Ok(
                LiveAdmissionOutcome::CommittedDurabilityUncertain(error.into()),
            ),
            Ok(AdmissionOutcome::CompletionUnknown(error)) => {
                Ok(LiveAdmissionOutcome::CompletionUnknown(error))
            }
            Err(error) => Err(error),
        }
    }
    /// Leases only work available to this public pull registration.
    pub async fn next_event(
        &self,
        consumer: &ConsumerId,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent>, CodexQueueError> {
        self.inner
            .next_event(consumer, wait.min(MAX_WAIT), lease.min(MAX_LEASE))
            .await
            .map(|event| event.map(leased))
            .map_err(Into::into)
    }
    /// Acknowledges a public lease using its exact token.
    pub async fn acknowledge(
        &self,
        consumer: &ConsumerId,
        token: &DeliveryToken,
    ) -> Result<(), CodexQueueError> {
        self.inner
            .ack(consumer, token)
            .await
            .map(|_| ())
            .map_err(Into::into)
    }
    /// Registers a public pull consumer without granting resident authority.
    pub async fn register_consumer(
        &self,
        label: String,
        ttl: Duration,
        primary: bool,
        claim: bool,
    ) -> Result<ConsumerRegistrationResult, CodexQueueError> {
        self.inner
            .register_consumer(label, ttl.min(MAX_CONSUMER_TTL), primary, claim)
            .await
            .map_err(Into::into)
    }
    pub(crate) async fn register_live_consumer(
        &self,
    ) -> Result<CodexResidentHandle, CodexQueueError> {
        Ok(CodexResidentHandle {
            inner: self.inner.register_resident().await?,
        })
    }
    /// Reads queue diagnostics without granting capabilities.
    pub async fn status(&self) -> QueueStatus {
        self.inner.status().await
    }
    /// Atomically transfers primary pull ownership and optionally its pending work.
    pub async fn handoff(
        &self,
        from: &ConsumerId,
        to: &ConsumerId,
        pending: bool,
    ) -> Result<HandoffResult, CodexQueueError> {
        self.inner
            .handoff(from, to, pending)
            .await
            .map_err(Into::into)
    }
    /// Claims an expired primary and returns the pending records reassigned.
    pub async fn claim_primary(
        &self,
        consumer: &ConsumerId,
        orphaned: bool,
    ) -> Result<usize, CodexQueueError> {
        self.inner
            .claim_primary(consumer, orphaned)
            .await
            .map_err(Into::into)
    }
    /// Invalidates already-validated managed records in the common transaction.
    pub async fn invalidate_attention(
        &self,
        selector: AttentionInvalidation<'_>,
    ) -> Result<AttentionInvalidationResult, CodexQueueError> {
        if let AttentionInvalidation::Source(source) = selector
            && (source.conversation().module() != self.scope.module()
                || source.conversation().local().as_numeric().is_none()
                || source.local().as_numeric().is_none())
        {
            return Ok(AttentionInvalidationResult {
                removed: 0,
                invalidated_leases: 0,
            });
        }
        match selector {
            AttentionInvalidation::Source(source) => {
                let source = source.clone();
                self.inner
                    .invalidate(move |payload| {
                        payload
                            .attention()
                            .is_some_and(|owned| owned.descriptor().source() == &source)
                    })
                    .await
                    .map_err(Into::into)
            }
            AttentionInvalidation::Record(record) => {
                let record = record.clone();
                self.inner
                    .invalidate(move |payload| {
                        payload
                            .attention()
                            .is_some_and(|owned| owned.descriptor().record() == &record)
                    })
                    .await
                    .map_err(Into::into)
            }
        }
    }
    pub(crate) async fn drain_owned(&self) {
        self.inner.drain_owned().await;
    }
    #[cfg(test)]
    pub(crate) fn test_set_now(&self, now: chrono::DateTime<chrono::Utc>) {
        self.inner.test_set_now(now)
    }
    #[cfg(test)]
    pub(crate) fn test_changed(&self) -> Arc<tokio::sync::Notify> {
        self.inner.test_changed()
    }
    #[cfg(test)]
    pub(crate) fn pause_owned_completion_for_test(
        &self,
        lose: bool,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.inner.test_pause_owned_completion(lose)
    }
    #[cfg(test)]
    pub(crate) fn pause_binding_publish_for_test(
        &self,
    ) -> (
        tokio::sync::oneshot::Receiver<()>,
        tokio::sync::oneshot::Sender<()>,
    ) {
        self.inner.test_pause_binding_publish()
    }
    #[cfg(test)]
    pub(crate) async fn test_snapshot(
        &self,
    ) -> durable::QueueSnapshot<LegacyPayload, LegacyReplayIdentity> {
        self.inner.test_snapshot().await
    }
    #[cfg(test)]
    pub(crate) async fn test_mutate_snapshot(
        &self,
        mutate: impl FnOnce(&mut durable::QueueSnapshot<LegacyPayload, LegacyReplayIdentity>),
    ) {
        self.inner.test_mutate_snapshot(mutate).await;
    }
    #[cfg(test)]
    pub(crate) async fn test_set_persist_failure(&self, failure: Option<durable::PersistFailure>) {
        self.inner.test_set_persist_failure(failure).await;
    }
    #[cfg(test)]
    pub(crate) async fn test_state(
        &self,
    ) -> (
        durable::QueueSnapshot<LegacyPayload, LegacyReplayIdentity>,
        bool,
        usize,
    ) {
        self.inner.test_state().await
    }
    #[cfg(test)]
    pub(crate) async fn test_directory_sync_attempts(&self) -> usize {
        self.inner.test_directory_sync_attempts().await
    }
}
impl CodexResidentHandle {
    #[cfg(test)]
    pub(crate) fn consumer_id(&self) -> &ConsumerId {
        self.inner.consumer_id()
    }
    pub(crate) async fn next_event(
        &self,
        thread: &CodexThreadId,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent>, CodexQueueError> {
        self.inner
            .next_event(
                &endpoint(thread.clone())?,
                wait.min(MAX_WAIT),
                lease.min(MAX_LEASE),
            )
            .await
            .map(|event| event.map(leased))
            .map_err(Into::into)
    }
    pub(crate) async fn acknowledge(&self, token: &DeliveryToken) -> Result<(), CodexQueueError> {
        self.inner.ack(token).await.map(|_| ()).map_err(Into::into)
    }
    pub(crate) async fn defer_attention(
        &self,
        token: &DeliveryToken,
        delay: Duration,
    ) -> Result<(), CodexQueueError> {
        self.inner
            .defer(token, delay.clamp(MIN_ATTENTION_DEFER, MAX_ATTENTION_DEFER))
            .await
            .map(|_| ())
            .map_err(Into::into)
    }
    #[cfg(test)]
    pub(crate) async fn bind_live_thread(
        &self,
        thread: Option<CodexThreadId>,
    ) -> Result<(), CodexQueueError> {
        self.inner
            .bind(thread.map(endpoint).transpose()?)
            .await
            .map_err(Into::into)
    }
}

impl CodexBindingController {
    #[cfg(test)]
    pub(crate) async fn bind_live_thread(
        &self,
        thread: Option<CodexThreadId>,
    ) -> Result<(), CodexQueueError> {
        self.inner
            .bind(thread.map(endpoint).transpose()?)
            .await
            .map_err(Into::into)
    }
    pub(crate) async fn bind_live_thread_and_publish(
        &self,
        thread: Option<CodexThreadId>,
        binding: &watch::Sender<Option<EndpointId>>,
    ) -> Result<(), CodexQueueError> {
        self.inner
            .bind_and_publish(thread.map(endpoint).transpose()?, binding.clone())
            .await
            .map_err(Into::into)
    }
}
