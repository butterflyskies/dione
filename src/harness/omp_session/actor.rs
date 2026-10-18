//! One command owner fences binding, persistence, and lease settlement.
use super::{
    codec::OmpCodec,
    types::{
        BindingId, BindingRecovery, BindingStatus, OmpBinding, OmpPayload, OmpSessionError,
        OmpSessionErrorKind, SavedSessionId,
    },
};
#[cfg(test)]
use crate::durable::PersistFailure;
use crate::{
    delivery::ProviderEventKey,
    durable::{
        AdmissionOutcome, BindingController, DeliveryToken, DurableQueue, LeasedEvent,
        QueueErrorKind, QueueOptions, ReplayClass, ResidentHandle,
    },
    harness::ProtocolOutput,
    teams::delivery::TeamsResidentEvent,
};
use camino::Utf8Path;
use serde_json::{Value, json};
#[cfg(test)]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::{sync::Arc, time::Duration};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::interval,
};
use tokio_util::sync::CancellationToken;
#[cfg(test)]
#[path = "tests.rs"]
mod tests;

const COMMAND_CAPACITY: usize = 64;
const FRAME_CAPACITY: usize = 1;
const LEASE_DURATION: Duration = Duration::from_secs(120);
const PUMP_INTERVAL: Duration = Duration::from_secs(1);
type Inbox = DurableQueue<OmpPayload, ProviderEventKey>;
type Reply<T> = oneshot::Sender<Result<T, OmpSessionError>>;

enum Command {
    Initialized(Reply<()>),
    Bind(SavedSessionId, Reply<OmpBinding>),
    Check(SavedSessionId, BindingId, Reply<BindingStatus>),
    Recover(SavedSessionId, BindingId, Reply<BindingRecovery>),
    Disconnect(Option<BindingId>, Reply<()>),
    Admit(Box<TeamsResidentEvent>, Reply<AdmissionOutcome>),
    Ack(BindingId, DeliveryToken, Reply<bool>),
    #[cfg(test)]
    PausedAdmit(Box<TeamsResidentEvent>, Reply<AdmissionOutcome>, Pause),
    #[cfg(test)]
    PausedBind(SavedSessionId, Reply<OmpBinding>, Pause),
    #[cfg(test)]
    FaultedBind(SavedSessionId, PersistFailure, Reply<OmpBinding>),
    #[cfg(test)]
    FaultedRenew(PersistFailure, Reply<()>),
    #[cfg(test)]
    KeepBusy(mpsc::Sender<Command>, Arc<AtomicUsize>),
}
#[cfg(test)]
struct Pause {
    lose_response: bool,
    controls: oneshot::Sender<(oneshot::Receiver<()>, oneshot::Sender<()>)>,
}

#[derive(Clone)]
pub(crate) struct OmpAdmission {
    sender: mpsc::Sender<Command>,
}
#[derive(Clone)]
pub(crate) struct OmpControl {
    sender: mpsc::Sender<Command>,
}

async fn request<T>(
    sender: &mpsc::Sender<Command>,
    make: impl FnOnce(Reply<T>) -> Command,
) -> Result<T, OmpSessionError> {
    let (reply, received) = oneshot::channel();
    sender
        .send(make(reply))
        .await
        .map_err(|_| OmpSessionErrorKind::Unavailable)?;
    received
        .await
        .map_err(OmpSessionErrorKind::CompletionUnknown)?
}
impl OmpAdmission {
    pub(crate) async fn enqueue(
        &self,
        event: TeamsResidentEvent,
    ) -> Result<AdmissionOutcome, OmpSessionError> {
        request(&self.sender, |reply| Command::Admit(Box::new(event), reply)).await
    }
}
impl OmpControl {
    pub(crate) async fn initialized(&self) -> Result<(), OmpSessionError> {
        request(&self.sender, Command::Initialized).await
    }
    pub(crate) async fn bind(
        &self,
        session: SavedSessionId,
    ) -> Result<OmpBinding, OmpSessionError> {
        request(&self.sender, |reply| Command::Bind(session, reply)).await
    }
    pub(crate) async fn check(
        &self,
        session: SavedSessionId,
        predecessor: BindingId,
    ) -> Result<BindingStatus, OmpSessionError> {
        request(&self.sender, |reply| {
            Command::Check(session, predecessor, reply)
        })
        .await
    }
    pub(crate) async fn recover(
        &self,
        session: SavedSessionId,
        predecessor: BindingId,
    ) -> Result<BindingRecovery, OmpSessionError> {
        request(&self.sender, |reply| {
            Command::Recover(session, predecessor, reply)
        })
        .await
    }
    pub(crate) async fn disconnect(
        &self,
        binding: Option<BindingId>,
    ) -> Result<(), OmpSessionError> {
        request(&self.sender, |reply| Command::Disconnect(binding, reply)).await
    }
    pub(crate) async fn ack(
        &self,
        binding: BindingId,
        token: DeliveryToken,
    ) -> Result<bool, OmpSessionError> {
        request(&self.sender, |reply| Command::Ack(binding, token, reply)).await
    }
}

pub(crate) struct OmpSession {
    queue: Inbox,
    sender: mpsc::Sender<Command>,
    receiver: mpsc::Receiver<Command>,
}
pub(crate) struct RunningOmpSession {
    stop: CancellationToken,
    actor: JoinHandle<Result<(), OmpSessionError>>,
    forwarder: JoinHandle<Result<(), OmpSessionError>>,
}
struct WorkerFailureGuard(Option<CancellationToken>);
impl Drop for WorkerFailureGuard {
    fn drop(&mut self) {
        if let Some(shutdown) = &self.0 {
            shutdown.cancel();
        }
    }
}
impl OmpSession {
    pub(crate) async fn open(directory: &Utf8Path) -> Result<Self, OmpSessionError> {
        let queue = Inbox::open(
            directory.to_owned(),
            Arc::new(OmpCodec),
            QueueOptions {
                file_name: "omp-inbox.json",
                lock_name: "omp-inbox.lock",
                consumer_prefix: "omp",
                resident_label: "omp-resident",
                replay_class: |_| ReplayClass::Timed,
                rolling_limit: 0,
                timed_limit: 8192,
                timed_retention: chrono::Duration::days(7),
            },
        )
        .await?;
        let (sender, receiver) = mpsc::channel(COMMAND_CAPACITY);
        Ok(Self {
            queue,
            sender,
            receiver,
        })
    }
    pub(crate) fn admission(&self) -> OmpAdmission {
        OmpAdmission {
            sender: self.sender.clone(),
        }
    }
    pub(crate) fn control(&self) -> OmpControl {
        OmpControl {
            sender: self.sender.clone(),
        }
    }
    pub(crate) fn start(
        self,
        output: ProtocolOutput,
        process_shutdown: CancellationToken,
    ) -> RunningOmpSession {
        let stop = CancellationToken::new();
        let (published, binding) = watch::channel(None);
        let (frames, received_frames) = mpsc::channel(FRAME_CAPACITY);
        let owner = Owner {
            controller: self.queue.binding_controller(),
            queue: self.queue,
            resident: None,
            state: State::Disconnected,
            recovery_predecessor: None,
            recovery_receipt: None,
            published,
            receiver: self.receiver,
            frames,
            #[cfg(test)]
            admission_pause: None,
        };
        let actor_stop = stop.clone();
        let actor_shutdown = process_shutdown.clone();
        let actor = tokio::spawn(async move {
            let mut failure = WorkerFailureGuard(Some(actor_shutdown.clone()));
            let result = owner.run(actor_stop.clone(), actor_shutdown.clone()).await;
            actor_stop.cancel();
            if result.is_err() {
                actor_shutdown.cancel();
            }
            failure.0 = None;
            result
        });
        let forward_stop = stop.clone();
        let forwarder = tokio::spawn(async move {
            let mut failure = WorkerFailureGuard(Some(process_shutdown.clone()));
            let result = forward(output, binding, received_frames, forward_stop.clone()).await;
            if result.is_err() {
                process_shutdown.cancel();
                forward_stop.cancel();
            }
            failure.0 = None;
            result
        });
        RunningOmpSession {
            stop,
            actor,
            forwarder,
        }
    }
}
impl RunningOmpSession {
    pub(crate) async fn shutdown(self) -> Result<(), OmpSessionError> {
        self.stop.cancel();
        let actor = self.actor.await.map_err(OmpSessionErrorKind::Worker);
        let forwarder = self.forwarder.await.map_err(OmpSessionErrorKind::Worker);
        actor??;
        forwarder??;
        Ok(())
    }
}

enum State {
    Disconnected,
    ReadyUnbound,
    Bound(OmpBinding),
}
impl State {
    fn binding(&self) -> Option<&OmpBinding> {
        match self {
            Self::Bound(binding) => Some(binding),
            Self::Disconnected | Self::ReadyUnbound => None,
        }
    }
}
struct Frame {
    binding: OmpBinding,
    event: LeasedEvent<OmpPayload>,
}
struct RecoveryReceipt {
    predecessor: OmpBinding,
    result: OmpBinding,
}

struct Owner {
    queue: Inbox,
    controller: BindingController<OmpPayload, ProviderEventKey>,
    resident: Option<ResidentHandle<OmpPayload, ProviderEventKey>>,
    state: State,
    recovery_predecessor: Option<OmpBinding>,
    recovery_receipt: Option<RecoveryReceipt>,
    published: watch::Sender<Option<OmpBinding>>,
    receiver: mpsc::Receiver<Command>,
    frames: mpsc::Sender<Frame>,
    #[cfg(test)]
    admission_pause: Option<Pause>,
}
impl Owner {
    fn publish(&mut self, state: State) {
        self.state = state;
        self.published.send_replace(self.state.binding().cloned());
    }
    fn revoke_binding(&mut self) {
        if let Some(binding) = self.state.binding() {
            self.recovery_predecessor = Some(binding.clone());
        }
        let state = match self.state {
            State::Disconnected => State::Disconnected,
            State::ReadyUnbound | State::Bound(_) => State::ReadyUnbound,
        };
        self.publish(state);
    }
    fn revoke_if_uncertain(&mut self, error: &OmpSessionError) {
        if matches!(&error.kind, OmpSessionErrorKind::Queue(error) if matches!(error.kind, QueueErrorKind::DurabilityUncertain(_) | QueueErrorKind::CompletionUnknown(_) | QueueErrorKind::PriorSync(_)))
        {
            self.revoke_binding();
        }
    }
    async fn renew(&mut self) -> Result<(), OmpSessionError> {
        match self.controller.renew_resident().await {
            Ok(resident) => {
                self.resident = Some(resident);
                Ok(())
            }
            Err(error) => {
                let error = error.into();
                self.revoke_if_uncertain(&error);
                Err(error)
            }
        }
    }
    async fn bind(&mut self, session_id: SavedSessionId) -> Result<OmpBinding, OmpSessionError> {
        if matches!(self.state, State::Disconnected) {
            return Err(OmpSessionErrorKind::Unavailable.into());
        }
        // An explicit lifecycle request is distinct from uncertainty recovery.
        if self.state.binding().is_none() {
            self.recovery_predecessor = None;
            self.recovery_receipt = None;
        }
        match self
            .controller
            .rebind_resident(session_id.endpoint().clone())
            .await
        {
            Ok(resident) => {
                self.resident = Some(resident);
                let binding = OmpBinding {
                    session_id,
                    binding_id: BindingId::generate(),
                };
                self.recovery_predecessor = None;
                self.recovery_receipt = None;
                self.publish(State::Bound(binding.clone()));
                Ok(binding)
            }
            Err(error) => {
                let error = error.into();
                self.revoke_if_uncertain(&error);
                if self.state.binding().is_none() {
                    self.recovery_predecessor = None;
                    self.recovery_receipt = None;
                }
                Err(error)
            }
        }
    }
    fn check(&self, requested: &OmpBinding) -> BindingStatus {
        match &self.state {
            State::Disconnected => BindingStatus::Unavailable,
            State::Bound(binding) if binding == requested => BindingStatus::Active,
            State::ReadyUnbound if self.recovery_predecessor.as_ref() == Some(requested) => {
                BindingStatus::Inactive
            }
            State::ReadyUnbound | State::Bound(_) => BindingStatus::Stale,
        }
    }
    async fn recover(
        &mut self,
        predecessor: OmpBinding,
    ) -> Result<BindingRecovery, OmpSessionError> {
        if self.state.binding() == Some(&predecessor) {
            return Ok(BindingRecovery::Active(predecessor));
        }
        if let Some(receipt) = &self.recovery_receipt
            && receipt.predecessor == predecessor
        {
            if self.state.binding() == Some(&receipt.result) {
                return Ok(BindingRecovery::Active(receipt.result.clone()));
            }
            if matches!(self.state, State::ReadyUnbound)
                && self.recovery_predecessor.as_ref() == Some(&receipt.result)
            {
                return Ok(BindingRecovery::Inactive(receipt.result.clone()));
            }
        }
        match self.check(&predecessor) {
            BindingStatus::Unavailable => return Ok(BindingRecovery::Unavailable),
            BindingStatus::Stale => return Ok(BindingRecovery::Stale),
            BindingStatus::Active => return Ok(BindingRecovery::Active(predecessor)),
            BindingStatus::Inactive => {}
        }
        // This actor owns the fence check and barrier together; no later command
        // can insert an explicit bind between eligibility and publication.
        let resident = self
            .controller
            .rebind_resident(predecessor.session_id.endpoint().clone())
            .await?;
        let binding = OmpBinding {
            session_id: predecessor.session_id.clone(),
            binding_id: BindingId::generate(),
        };
        self.resident = Some(resident);
        self.recovery_predecessor = None;
        // Cache before publication/reply so cancellation cannot lose its outcome.
        self.recovery_receipt = Some(RecoveryReceipt {
            predecessor,
            result: binding.clone(),
        });
        self.publish(State::Bound(binding.clone()));
        Ok(BindingRecovery::Active(binding))
    }
    async fn admit(
        &mut self,
        event: TeamsResidentEvent,
    ) -> Result<AdmissionOutcome, OmpSessionError> {
        if self.state.binding().is_some() {
            self.renew().await?;
        }
        let bound = self.state.binding().is_some();
        let key = event.provider_key.clone();
        #[cfg(test)]
        if let Some(pause) = self.admission_pause.take() {
            let controls = self.queue.test_pause_owned_completion(pause.lose_response);
            let _ = pause.controls.send(controls);
        }
        let result = self
            .queue
            .enqueue_resident_with(Some(key), move || {
                if !bound {
                    return Err(OmpSessionErrorKind::Unavailable.into());
                }
                OmpPayload::from_event(event).map(|payload| (payload, false))
            })
            .await;
        match &result {
            Ok(
                AdmissionOutcome::CommittedDurabilityUncertain(_)
                | AdmissionOutcome::CompletionUnknown(_),
            ) => self.revoke_binding(),
            Err(error) => self.revoke_if_uncertain(error),
            Ok(AdmissionOutcome::Committed | AdmissionOutcome::Duplicate) => {}
        }
        result
    }
    async fn ack(
        &mut self,
        generation: BindingId,
        token: DeliveryToken,
    ) -> Result<bool, OmpSessionError> {
        let binding = self
            .state
            .binding()
            .filter(|binding| binding.binding_id == generation)
            .cloned()
            .ok_or(OmpSessionErrorKind::StaleBinding)?;
        self.renew().await?;
        let resident = self
            .resident
            .as_ref()
            .ok_or(OmpSessionErrorKind::Unavailable)?;
        let result = resident
            .ack_checked(binding.session_id.endpoint(), &token)
            .await
            .map_err(OmpSessionError::from);
        if let Err(error) = &result {
            self.revoke_if_uncertain(error);
        }
        result
    }
    async fn handle(&mut self, command: Command) {
        match command {
            Command::Initialized(reply) => {
                if matches!(self.state, State::Disconnected) {
                    self.publish(State::ReadyUnbound);
                }
                let _ = reply.send(Ok(()));
            }
            Command::Bind(session, reply) => {
                let result = self.bind(session).await;
                let _ = reply.send(result);
            }
            Command::Check(session_id, binding_id, reply) => {
                let _ = reply.send(Ok(self.check(&OmpBinding {
                    session_id,
                    binding_id,
                })));
            }
            Command::Recover(session_id, binding_id, reply) => {
                let result = self
                    .recover(OmpBinding {
                        session_id,
                        binding_id,
                    })
                    .await;
                let _ = reply.send(result);
            }
            Command::Admit(event, reply) => {
                let result = self.admit(*event).await;
                let _ = reply.send(result);
            }
            Command::Ack(binding, token, reply) => {
                let result = self.ack(binding, token).await;
                let _ = reply.send(result);
            }
            Command::Disconnect(generation, reply) => {
                let result = match generation {
                    None => {
                        self.recovery_predecessor = None;
                        self.recovery_receipt = None;
                        self.publish(State::Disconnected);
                        Ok(())
                    }
                    Some(generation)
                        if self
                            .state
                            .binding()
                            .is_some_and(|binding| binding.binding_id == generation) =>
                    {
                        self.recovery_predecessor = None;
                        self.recovery_receipt = None;
                        self.publish(State::ReadyUnbound);
                        Ok(())
                    }
                    Some(generation)
                        if matches!(self.state, State::ReadyUnbound)
                            && self
                                .recovery_predecessor
                                .as_ref()
                                .is_some_and(|binding| binding.binding_id == generation) =>
                    {
                        self.recovery_predecessor = None;
                        self.recovery_receipt = None;
                        Ok(())
                    }
                    Some(_) => Err(OmpSessionErrorKind::StaleBinding.into()),
                };
                let _ = reply.send(result);
            }
            #[cfg(test)]
            Command::PausedAdmit(event, reply, pause) => {
                self.admission_pause = Some(pause);
                let result = self.admit(*event).await;
                let _ = reply.send(result);
            }
            #[cfg(test)]
            Command::PausedBind(session, reply, pause) => {
                let controls = self.queue.test_pause_owned_completion(pause.lose_response);
                let _ = pause.controls.send(controls);
                let result = self.bind(session).await;
                let _ = reply.send(result);
            }
            #[cfg(test)]
            Command::FaultedBind(session, failure, reply) => {
                self.queue.test_set_persist_failure(Some(failure)).await;
                let result = self.bind(session).await;
                self.queue.test_set_persist_failure(None).await;
                let _ = reply.send(result);
            }
            #[cfg(test)]
            Command::FaultedRenew(failure, reply) => {
                self.queue.test_set_persist_failure(Some(failure)).await;
                let result = self.renew().await;
                self.queue.test_set_persist_failure(None).await;
                let _ = reply.send(result);
            }
            #[cfg(test)]
            Command::KeepBusy(sender, handled) => {
                handled.fetch_add(1, Ordering::Relaxed);
                let _ = sender.try_send(Command::KeepBusy(sender.clone(), handled));
            }
        }
    }
    async fn poll(&mut self) -> Result<(), OmpSessionError> {
        let Some(binding) = self.state.binding().cloned() else {
            return Ok(());
        };
        let frames = self.frames.clone();
        let Ok(permit) = frames.try_reserve() else {
            return Ok(());
        };
        self.renew().await?;
        let resident = self
            .resident
            .as_ref()
            .ok_or(OmpSessionErrorKind::Unavailable)?;
        if let Some(event) = resident
            .next_event(
                binding.session_id.endpoint(),
                Duration::ZERO,
                LEASE_DURATION,
            )
            .await?
        {
            permit.send(Frame { binding, event });
        }
        Ok(())
    }
    async fn run(
        mut self,
        stop: CancellationToken,
        process_shutdown: CancellationToken,
    ) -> Result<(), OmpSessionError> {
        let mut pump = interval(PUMP_INTERVAL);
        loop {
            tokio::select! {
                biased;
                _ = stop.cancelled() => break,
                _ = process_shutdown.cancelled() => break,
                _ = pump.tick() => {
                    if let Err(error) = self.poll().await {
                        self.revoke_if_uncertain(&error);
                        tracing::warn!(error = %error, "OMP inbox delivery remains pending");
                    }
                    pump.reset();
                },
                command = self.receiver.recv() => match command { Some(command) => self.handle(command).await, None => break },
            }
        }
        self.receiver.close();
        while let Some(command) = self.receiver.recv().await {
            self.handle(command).await;
        }
        self.publish(State::Disconnected);
        self.queue.drain_owned().await;
        Ok(())
    }
}

fn notification(frame: Frame) -> Value {
    let OmpPayload::Teams {
        content,
        message_id,
        conversation_id,
        channel_id,
        user_id,
        bot_id,
        tenant_id,
        reply_handle,
        timestamp,
    } = frame.event.payload;
    json!({ "jsonrpc": "2.0", "method": "notifications/dione/event", "params": {
        "version": 1, "content": content, "meta": {
            "provider": "teams", "message_id": message_id, "conversation_id": conversation_id,
            "channel_id": channel_id, "user_id": user_id, "bot_id": bot_id, "tenant_id": tenant_id,
            "reply_handle": reply_handle, "timestamp": timestamp,
            "mcp_session_id": frame.binding.session_id, "binding_id": frame.binding.binding_id,
            "event_id": frame.event.event_id.to_string(), "delivery_token": frame.event.delivery_token,
        }
    }})
}
async fn forward(
    output: ProtocolOutput,
    mut binding: watch::Receiver<Option<OmpBinding>>,
    mut frames: mpsc::Receiver<Frame>,
    stop: CancellationToken,
) -> Result<(), OmpSessionError> {
    loop {
        let frame = tokio::select! {
            biased;
            _ = stop.cancelled() => return Ok(()),
            _ = output.failed() => return Err(OmpSessionErrorKind::OutputUnavailable.into()),
            frame = frames.recv() => match frame { Some(frame) => frame, None => return Ok(()) },
        };
        if binding.borrow_and_update().as_ref() != Some(&frame.binding) {
            continue;
        }
        let value = notification(frame);
        tokio::select! {
            biased;
            _ = stop.cancelled() => return Ok(()),
            changed = binding.changed() => { if changed.is_err() { return Ok(()); } },
            result = output.write(&value) => result.map_err(OmpSessionErrorKind::Output)?,
        }
    }
}
