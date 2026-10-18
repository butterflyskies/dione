//! Accepted operations retain the owner until blocking persistence finishes.
use super::{
    ConsumerId, ConsumerRegistrationResult, DeliveryToken, EndpointId, HandoffResult, LeasedEvent,
    QueueCodec, QueueError, QueueErrorKind, QueueOptions, QueueStatus, RemovalResult,
    engine::{self, Access, AdmissionRoute, RegistrationRequest},
    state::ConsumerKind,
    store::DurableStore,
};
#[cfg(test)]
use super::{QueueSnapshot, store::PersistFailure};
use camino::{Utf8Path, Utf8PathBuf};
use chrono::{DateTime, Utc};
#[cfg(test)]
use std::sync::Mutex as StdMutex;
use std::{hash::Hash, sync::Arc, time::Duration};
#[cfg(test)]
use tokio::sync::oneshot;
use tokio::{
    sync::{Mutex, Notify, watch},
    task::{JoinError, spawn_blocking},
    time::{Instant, timeout},
};

/// One fresh payload has at most one replay identity.
#[derive(Debug, Clone)]
pub struct Admission<P, K> {
    /// Adapter-validated immutable payload.
    pub payload: P,
    /// Exact optional identity used for replay suppression.
    pub replay_key: Option<K>,
    /// Whether the owner permits deferring this payload.
    pub deferrable: bool,
}
/// Resident admission distinguishes visibility from durability and response delivery.
#[derive(Debug)]
pub enum AdmissionOutcome {
    /// Snapshot and parent directory are synchronized.
    Committed,
    /// Snapshot is visible, but this commit's parent-directory sync failed.
    CommittedDurabilityUncertain(QueueError),
    /// The exact identity was already retained; no new event was inserted.
    Duplicate,
    /// An accepted operation lost its completion response.
    CompletionUnknown(JoinError),
}
#[cfg(test)]
struct CompletionGate {
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
    lose_response: bool,
}
#[cfg(test)]
struct BindingGate {
    reached: oneshot::Sender<()>,
    release: oneshot::Receiver<()>,
}
/// One generic queue owner serializes durable transitions and accepted operations.
pub struct DurableQueue<P, K> {
    store: Arc<Mutex<DurableStore<P, K>>>,
    changed: Arc<Notify>,
    #[cfg(test)]
    completion_gate: Arc<StdMutex<Option<CompletionGate>>>,
    #[cfg(test)]
    binding_gate: Arc<StdMutex<Option<BindingGate>>>,
    #[cfg(test)]
    clock: Arc<StdMutex<Option<DateTime<Utc>>>>,
}
impl<P, K> Clone for DurableQueue<P, K> {
    fn clone(&self) -> Self {
        Self {
            store: Arc::clone(&self.store),
            changed: Arc::clone(&self.changed),
            #[cfg(test)]
            completion_gate: Arc::clone(&self.completion_gate),
            #[cfg(test)]
            binding_gate: Arc::clone(&self.binding_gate),
            #[cfg(test)]
            clock: Arc::clone(&self.clock),
        }
    }
}
/// Owner-issued authority for resident consumption; a consumer identifier cannot construct it.
pub struct ResidentHandle<P, K> {
    queue: DurableQueue<P, K>,
    consumer: ConsumerId,
}
impl<P, K> Clone for ResidentHandle<P, K> {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
            consumer: self.consumer.clone(),
        }
    }
}
/// Owner-issued binding authority, independent of any consumer registration.
pub(crate) struct BindingController<P, K> {
    queue: DurableQueue<P, K>,
}
impl<P, K> Clone for BindingController<P, K> {
    fn clone(&self) -> Self {
        Self {
            queue: self.queue.clone(),
        }
    }
}
fn completion_error(source: JoinError) -> QueueError {
    QueueError::new(
        "await owned durable operation",
        QueueErrorKind::CompletionUnknown(source),
    )
}
fn wakes_commit<T>(result: &Result<T, QueueError>) -> bool {
    result.is_ok()
        || matches!(
            result,
            Err(QueueError {
                kind: QueueErrorKind::DurabilityUncertain(_),
                ..
            })
        )
}
impl<P: Clone + Send + 'static, K: Clone + Eq + Hash + Send + 'static> DurableQueue<P, K> {
    /// Opens synchronously for compatibility callers outside an async executor.
    pub fn load(
        directory: &Utf8Path,
        codec: Arc<dyn QueueCodec<Payload = P, Key = K>>,
        options: QueueOptions<K>,
    ) -> Result<Self, QueueError> {
        let store = DurableStore::load(directory, codec, options)?;
        Ok(Self {
            store: Arc::new(Mutex::new(store)),
            changed: Arc::new(Notify::new()),
            #[cfg(test)]
            completion_gate: Arc::new(StdMutex::new(None)),
            #[cfg(test)]
            binding_gate: Arc::new(StdMutex::new(None)),
            #[cfg(test)]
            clock: Arc::new(StdMutex::new(None)),
        })
    }
    /// Opens the store without blocking an async executor worker.
    pub async fn open(
        directory: Utf8PathBuf,
        codec: Arc<dyn QueueCodec<Payload = P, Key = K>>,
        options: QueueOptions<K>,
    ) -> Result<Self, QueueError> {
        spawn_blocking(move || Self::load(&directory, codec, options))
            .await
            .map_err(completion_error)?
    }
    async fn run_owned<R: Send + 'static>(
        &self,
        operation: impl FnOnce(&mut DurableStore<P, K>) -> R + Send + 'static,
        wakes: impl FnOnce(&R) -> bool + Send + 'static,
    ) -> Result<R, JoinError> {
        let mut store = Arc::clone(&self.store).lock_owned().await;
        let changed = Arc::clone(&self.changed);
        #[cfg(test)]
        let gate = self
            .completion_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        spawn_blocking(move || {
            let result = operation(&mut store);
            if wakes(&result) {
                changed.notify_waiters();
            }
            #[cfg(test)]
            if let Some(gate) = gate {
                let _ = gate.reached.send(());
                let _ = gate.release.blocking_recv();
                assert!(
                    !gate.lose_response,
                    "injected owned-operation response loss"
                );
            }
            result
        })
        .await
    }
    fn now(&self) -> DateTime<Utc> {
        #[cfg(test)]
        if let Some(now) = *self
            .clock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
        {
            return now;
        }
        Utc::now()
    }
    /// Waits until previously accepted operations release ownership.
    pub async fn drain_owned(&self) {
        drop(self.store.lock().await);
    }
    #[cfg(test)]
    /// Inserts a prevalidated test payload through the production preparation path.
    pub async fn enqueue(&self, admission: Admission<P, K>) -> Result<bool, QueueError> {
        self.enqueue_with(move || Ok(admission)).await
    }
    /// Validates an adapter payload under ownership after the prior durability barrier.
    pub(crate) async fn enqueue_with<E: From<QueueError> + Send + 'static>(
        &self,
        prepare: impl FnOnce() -> Result<Admission<P, K>, E> + Send + 'static,
    ) -> Result<bool, E> {
        let queue = self.clone();
        let (result, _) = self
            .run_owned(
                move |store| {
                    let mut wake = false;
                    let result = (|| {
                        store.sync_prior().map_err(E::from)?;
                        let admission = prepare()?;
                        let now = queue.now();
                        if admission.replay_key.as_ref().is_some_and(|key| {
                            engine::duplicate(&store.state, &store.options, key, now)
                        }) {
                            return Ok(false);
                        }
                        let result = store.transaction(move |state, options| {
                            engine::enqueue(state, options, admission, AdmissionRoute::Public, now)
                        });
                        wake = matches!(
                            &result,
                            Ok(true)
                                | Err(QueueError {
                                    kind: QueueErrorKind::DurabilityUncertain(_),
                                    ..
                                })
                        );
                        result.map_err(E::from)
                    })();
                    (result, wake)
                },
                |(_, wake)| *wake,
            )
            .await
            .map_err(|source| E::from(completion_error(source)))?;
        result
    }
    #[cfg(test)]
    /// Admits a prevalidated test payload through the production resident path.
    pub async fn enqueue_resident(
        &self,
        admission: Admission<P, K>,
    ) -> Result<AdmissionOutcome, QueueError> {
        self.enqueue_resident_with(admission.replay_key, move || {
            Ok((admission.payload, admission.deferrable))
        })
        .await
    }
    /// Suppresses a known replay before adapter validation, retaining legacy ordering.
    pub(crate) async fn enqueue_resident_with<E: From<QueueError> + Send + 'static>(
        &self,
        replay_key: Option<K>,
        prepare: impl FnOnce() -> Result<(P, bool), E> + Send + 'static,
    ) -> Result<AdmissionOutcome, E> {
        let queue = self.clone();
        let result = self
            .run_owned(
                move |store| {
                    let mut wake = false;
                    let result = (|| {
                        store.sync_prior().map_err(E::from)?;
                        let now = queue.now();
                        if replay_key.as_ref().is_some_and(|key| {
                            engine::duplicate(&store.state, &store.options, key, now)
                        }) {
                            return Ok(AdmissionOutcome::Duplicate);
                        }
                        let (payload, deferrable) = prepare()?;
                        let admission = Admission {
                            payload,
                            replay_key,
                            deferrable,
                        };
                        let result = store.transaction(move |state, options| {
                            engine::enqueue(
                                state,
                                options,
                                admission,
                                AdmissionRoute::Resident,
                                now,
                            )
                        });
                        wake = matches!(
                            &result,
                            Ok(true)
                                | Err(QueueError {
                                    kind: QueueErrorKind::DurabilityUncertain(_),
                                    ..
                                })
                        );
                        match result {
                            Ok(true) => Ok(AdmissionOutcome::Committed),
                            Ok(false) => Ok(AdmissionOutcome::Duplicate),
                            Err(
                                cause @ QueueError {
                                    kind: QueueErrorKind::DurabilityUncertain(_),
                                    ..
                                },
                            ) => Ok(AdmissionOutcome::CommittedDurabilityUncertain(cause)),
                            Err(cause) => Err(E::from(cause)),
                        }
                    })();
                    (result, wake)
                },
                |(_, wake)| *wake,
            )
            .await;
        match result {
            Ok((result, _)) => result,
            Err(cause) => Ok(AdmissionOutcome::CompletionUnknown(cause)),
        }
    }
    /// Leases one public event for an active pull consumer.
    pub async fn next_event(
        &self,
        consumer: &ConsumerId,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent<P>>, QueueError> {
        self.next_matching(consumer.clone(), None, wait, lease)
            .await
    }
    async fn next_matching(
        &self,
        consumer: ConsumerId,
        endpoint: Option<EndpointId>,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent<P>>, QueueError> {
        let deadline = Instant::now() + wait.min(Duration::from_secs(55));
        loop {
            let notification = self.changed.notified();
            tokio::pin!(notification);
            notification.as_mut().enable();
            let consumer = consumer.clone();
            let endpoint = endpoint.clone();
            let queue = self.clone();
            let poll = self
                .run_owned(
                    move |store| {
                        let mut retained = None;
                        let result = store.transaction(|state, _| {
                            let access = if endpoint.is_some() {
                                Access::Resident(&consumer)
                            } else {
                                Access::Pull(&consumer)
                            };
                            let poll = engine::lease(
                                state,
                                access,
                                endpoint.as_ref(),
                                queue.now(),
                                lease,
                            )?;
                            retained = Some(poll.clone());
                            Ok(poll)
                        });
                        match result {
                            Err(QueueError {
                                kind: QueueErrorKind::DurabilityUncertain(_),
                                ..
                            }) if retained.is_some() => retained.ok_or_else(|| {
                                QueueError::new(
                                    "lease retained event",
                                    QueueErrorKind::UnknownDeliveryToken,
                                )
                            }),
                            other => other,
                        }
                    },
                    |_| false,
                )
                .await
                .map_err(completion_error)??;
            if poll.event.is_some() {
                return Ok(poll.event);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let delay = poll
                .deferred_for
                .map_or(remaining, |deferred| deferred.min(remaining));
            let _ = timeout(delay, notification).await;
        }
    }
    /// Acknowledges an owned public lease and retains its replay receipt.
    pub async fn ack(
        &self,
        consumer: &ConsumerId,
        token: &DeliveryToken,
    ) -> Result<bool, QueueError> {
        let consumer = consumer.clone();
        let token = token.clone();
        let queue = self.clone();
        self.run_owned(
            move |store| {
                store.transaction(|state, options| {
                    engine::ack(state, options, Access::Pull(&consumer), &token, queue.now())
                })
            },
            Result::is_ok,
        )
        .await
        .map_err(completion_error)?
    }
    /// Removes each matching immutable payload once in a single transaction.
    pub async fn invalidate(
        &self,
        predicate: impl Fn(&P) -> bool + Send + 'static,
    ) -> Result<RemovalResult, QueueError> {
        self.run_owned(
            move |store| store.transaction(|state, _| Ok(engine::invalidate(state, predicate))),
            Result::is_ok,
        )
        .await
        .map_err(completion_error)?
    }
    /// Reads current queue and registration diagnostics.
    pub async fn status(&self) -> QueueStatus {
        let store = self.store.lock().await;
        engine::status(&store.state, self.now())
    }
    /// Registers a pull consumer with optional primary ownership and unassigned work.
    pub async fn register_consumer(
        &self,
        label: String,
        ttl: Duration,
        make_primary: bool,
        claim_unassigned: bool,
    ) -> Result<ConsumerRegistrationResult, QueueError> {
        let queue = self.clone();
        self.run_owned(
            move |store| {
                store.transaction(|state, options| {
                    engine::register(
                        state,
                        options,
                        RegistrationRequest {
                            label,
                            ttl,
                            make_primary,
                            claim_unassigned,
                            kind: ConsumerKind::Pull,
                        },
                        queue.now(),
                    )
                })
            },
            Result::is_ok,
        )
        .await
        .map_err(completion_error)?
    }
    /// Transfers primary pull ownership and optionally moves pending public records.
    pub async fn handoff(
        &self,
        from: &ConsumerId,
        to: &ConsumerId,
        move_pending: bool,
    ) -> Result<HandoffResult, QueueError> {
        let from = from.clone();
        let to = to.clone();
        let queue = self.clone();
        self.run_owned(
            move |store| {
                store.transaction(|state, _| {
                    engine::handoff(state, &from, &to, move_pending, queue.now())
                })
            },
            Result::is_ok,
        )
        .await
        .map_err(completion_error)?
    }
    /// Claims vacant primary ownership and optionally reassigns orphaned public records.
    pub async fn claim_primary(
        &self,
        consumer: &ConsumerId,
        claim_orphaned: bool,
    ) -> Result<usize, QueueError> {
        let consumer = consumer.clone();
        let queue = self.clone();
        self.run_owned(
            move |store| {
                store.transaction(|state, _| {
                    engine::claim(state, &consumer, claim_orphaned, queue.now())
                })
            },
            Result::is_ok,
        )
        .await
        .map_err(completion_error)?
    }
    /// Creates binding authority without claiming a consumer or changing stored state.
    pub(crate) fn binding_controller(&self) -> BindingController<P, K> {
        BindingController {
            queue: self.clone(),
        }
    }
    /// Creates resident authority for the prepared in-process session owner.
    pub(crate) async fn register_resident(&self) -> Result<ResidentHandle<P, K>, QueueError> {
        let queue = self.clone();
        let registration = self
            .run_owned(
                move |store| {
                    store.transaction(|state, options| {
                        engine::register(
                            state,
                            options,
                            RegistrationRequest {
                                label: options.resident_label.to_owned(),
                                ttl: Duration::from_secs(86_400),
                                make_primary: true,
                                claim_unassigned: false,
                                kind: ConsumerKind::Resident,
                            },
                            queue.now(),
                        )
                    })
                },
                Result::is_ok,
            )
            .await
            .map_err(completion_error)??;
        Ok(ResidentHandle {
            queue: self.clone(),
            consumer: registration.consumer_id,
        })
    }
    #[cfg(test)]
    pub(crate) fn test_pause_owned_completion(
        &self,
        lose_response: bool,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *self
            .completion_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(CompletionGate {
            reached: reached_tx,
            release: release_rx,
            lose_response,
        });
        (reached_rx, release_tx)
    }
    #[cfg(test)]
    pub(crate) fn test_pause_binding_publish(
        &self,
    ) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
        let (reached_tx, reached_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *self
            .binding_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(BindingGate {
            reached: reached_tx,
            release: release_rx,
        });
        (reached_rx, release_tx)
    }
    #[cfg(test)]
    pub(crate) async fn test_set_persist_failure(&self, failure: Option<PersistFailure>) {
        self.store.lock().await.persist_failure = failure;
    }
    #[cfg(test)]
    pub(crate) async fn test_directory_sync_attempts(&self) -> usize {
        self.store.lock().await.directory_sync_attempts
    }
    #[cfg(test)]
    pub(crate) async fn test_snapshot(&self) -> QueueSnapshot<P, K> {
        self.store.lock().await.state.clone()
    }
    #[cfg(test)]
    pub(crate) async fn test_mutate_snapshot(&self, mutate: impl FnOnce(&mut QueueSnapshot<P, K>)) {
        mutate(&mut self.store.lock().await.state);
    }
    #[cfg(test)]
    pub(crate) fn test_changed(&self) -> Arc<Notify> {
        Arc::clone(&self.changed)
    }
    #[cfg(test)]
    pub(crate) async fn test_state(&self) -> (QueueSnapshot<P, K>, bool, usize) {
        self.store.lock().await.test_state()
    }
    #[cfg(test)]
    pub(crate) fn test_set_now(&self, now: DateTime<Utc>) {
        *self
            .clock
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = Some(now);
    }
}
impl<P: Clone + Send + 'static, K: Clone + Eq + Hash + Send + 'static> ResidentHandle<P, K> {
    /// Registration identity for diagnostics; this identity alone grants no authority.
    #[cfg(test)]
    pub fn consumer_id(&self) -> &ConsumerId {
        &self.consumer
    }
    /// Leases only a record for the caller's captured endpoint.
    pub async fn next_event(
        &self,
        endpoint: &EndpointId,
        wait: Duration,
        lease: Duration,
    ) -> Result<Option<LeasedEvent<P>>, QueueError> {
        self.queue
            .next_matching(self.consumer.clone(), Some(endpoint.clone()), wait, lease)
            .await
    }
    /// Settles only an unexpired resident lease for the exact captured endpoint.
    pub(crate) async fn ack_checked(
        &self,
        endpoint: &EndpointId,
        token: &DeliveryToken,
    ) -> Result<bool, QueueError> {
        let consumer = self.consumer.clone();
        let endpoint = endpoint.clone();
        let token = token.clone();
        let queue = self.queue.clone();
        self.queue
            .run_owned(
                move |store| {
                    store.transaction(|state, options| {
                        engine::ack_checked(
                            state,
                            options,
                            &consumer,
                            &endpoint,
                            &token,
                            queue.now(),
                        )
                    })
                },
                wakes_commit,
            )
            .await
            .map_err(completion_error)?
    }
    /// Acknowledges a resident-owned lease.
    pub async fn ack(&self, token: &DeliveryToken) -> Result<bool, QueueError> {
        let consumer = self.consumer.clone();
        let token = token.clone();
        let queue = self.queue.clone();
        self.queue
            .run_owned(
                move |store| {
                    store.transaction(|state, options| {
                        engine::ack(
                            state,
                            options,
                            Access::Resident(&consumer),
                            &token,
                            queue.now(),
                        )
                    })
                },
                Result::is_ok,
            )
            .await
            .map_err(completion_error)?
    }
    /// Releases an eligible resident lease until the bounded deferral expires.
    pub async fn defer(&self, token: &DeliveryToken, delay: Duration) -> Result<bool, QueueError> {
        let consumer = self.consumer.clone();
        let token = token.clone();
        let queue = self.queue.clone();
        self.queue
            .run_owned(
                move |store| {
                    store.transaction(|state, _| {
                        engine::defer(state, &consumer, &token, queue.now(), delay)
                    })
                },
                Result::is_ok,
            )
            .await
            .map_err(completion_error)?
    }
    #[cfg(test)]
    /// Changes the exact endpoint retained for future resident admissions.
    pub async fn bind(&self, endpoint: Option<EndpointId>) -> Result<(), QueueError> {
        self.queue.binding_controller().bind(endpoint).await
    }
    #[cfg(test)]
    /// Publishes retained binding through the same accepted operation owner.
    pub async fn bind_and_publish(
        &self,
        endpoint: Option<EndpointId>,
        sender: watch::Sender<Option<EndpointId>>,
    ) -> Result<(), QueueError> {
        self.queue
            .binding_controller()
            .bind_and_publish(endpoint, sender)
            .await
    }
}
impl<P: Clone + Send + 'static, K: Clone + Eq + Hash + Send + 'static> BindingController<P, K> {
    /// Renews authority and recovers orphaned private work for the retained endpoint.
    pub(crate) async fn renew_resident(&self) -> Result<ResidentHandle<P, K>, QueueError> {
        self.transition_resident(None).await
    }

    /// Atomically renews authority, binds future work, and revokes this endpoint's leases.
    pub(crate) async fn rebind_resident(
        &self,
        endpoint: EndpointId,
    ) -> Result<ResidentHandle<P, K>, QueueError> {
        self.transition_resident(Some(endpoint)).await
    }

    async fn transition_resident(
        &self,
        endpoint: Option<EndpointId>,
    ) -> Result<ResidentHandle<P, K>, QueueError> {
        let queue = self.queue.clone();
        let registration = self
            .queue
            .run_owned(
                move |store| {
                    store.transaction(|state, options| match endpoint {
                        Some(endpoint) => {
                            engine::rebind_resident(state, options, endpoint, queue.now())
                        }
                        None => {
                            let endpoint = state.endpoint.clone();
                            engine::renew_resident(state, options, endpoint.as_ref(), queue.now())
                        }
                    })
                },
                wakes_commit,
            )
            .await
            .map_err(completion_error)??;
        Ok(ResidentHandle {
            queue: self.queue.clone(),
            consumer: registration.consumer_id,
        })
    }

    #[cfg(test)]
    /// Changes the exact endpoint retained for future resident admissions.
    pub async fn bind(&self, endpoint: Option<EndpointId>) -> Result<(), QueueError> {
        self.queue
            .run_owned(
                move |store| {
                    store.transaction(|state, _| {
                        state.endpoint = endpoint;
                        Ok(())
                    })
                },
                Result::is_ok,
            )
            .await
            .map_err(completion_error)?
    }
    /// Publishes the retained endpoint before accepted ownership is released.
    pub async fn bind_and_publish(
        &self,
        endpoint: Option<EndpointId>,
        sender: watch::Sender<Option<EndpointId>>,
    ) -> Result<(), QueueError> {
        #[cfg(test)]
        let gate = self
            .queue
            .binding_gate
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .take();
        self.queue
            .run_owned(
                move |store| {
                    let result = store.transaction(|state, _| {
                        state.endpoint = endpoint;
                        Ok(())
                    });
                    #[cfg(test)]
                    if let Some(gate) = gate {
                        let _ = gate.reached.send(());
                        let _ = gate.release.blocking_recv();
                    }
                    sender.send_if_modified(|published| {
                        if *published == store.state.endpoint {
                            return false;
                        }
                        *published = store.state.endpoint.clone();
                        true
                    });
                    result
                },
                wakes_commit,
            )
            .await
            .map_err(completion_error)?
    }
}
