mod publication;

use super::{
    model::*,
    observation::{EventKey, RetainedObservation},
    private_dir::StorageErrorKind,
    store::Disk,
};
use chrono::{DateTime, Utc};
use std::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    future::Future,
    path::PathBuf,
    pin::Pin,
    sync::Arc,
    time::Duration,
};
use tokio::{
    sync::{mpsc, oneshot},
    task::{JoinHandle, JoinSet},
};

// Derived from retained receipts, counters and current binding leases. All scheduler
// decisions use this population; a verified but exhausted link cannot mask refresh.
#[derive(Clone, Copy)]
enum BindingProgress {
    Usable {
        generation: BindingGeneration,
        due: DateTime<Utc>,
    },
    Refreshable,
    InFlight,
    Exhausted,
}
impl BindingProgress {
    fn can_progress(self) -> bool {
        matches!(self, Self::Usable { .. } | Self::InFlight)
    }
}

fn binding_progress<'a>(
    event: &EventRecord,
    subscriptions: &'a [Subscription],
    attempt_budget: u8,
    now: DateTime<Utc>,
) -> Vec<(&'a Subscription, BindingProgress)> {
    let mut progress: Vec<_> = event
        .links
        .iter()
        .filter_map(|link| {
            let subscription = subscriptions.iter().find(|subscription| {
                subscription.id == link.subscription && subscription.admits(&event.observation)
            })?;
            let state = match link.wake {
                WakeState::Dispatching { .. } | WakeState::Accepted => BindingProgress::InFlight,
                WakeState::Exhausted { .. } | WakeState::Cancelled => BindingProgress::Exhausted,
                WakeState::Pending { .. } | WakeState::Retry { .. }
                    if link.attempts >= attempt_budget =>
                {
                    BindingProgress::Exhausted
                }
                WakeState::Pending { due } | WakeState::Retry { due, .. } => {
                    match subscription.binding {
                        BindingState::Verified {
                            generation,
                            refresh_before,
                        } if refresh_before > now => BindingProgress::Usable { generation, due },
                        _ => BindingProgress::Refreshable,
                    }
                }
            };
            Some((subscription, state))
        })
        .collect();
    progress.sort_by_key(|(subscription, _)| (subscription.priority, &subscription.id));
    progress
}

/// Clock supplied by the resident application; persisted time never moves backward.
pub trait Clock: Send + Sync + 'static {
    /// Read current wall time; the owner clamps it to the retained high-water mark.
    fn now(&self) -> DateTime<Utc>;
}
/// Current provider and recipient access, independent of retained message content.
pub trait Authorizer: Send + Sync + 'static {
    /// Verify this recipient's current access using authoritative provider identity and policy.
    ///
    /// Return `Unavailable` when verification fails; event content cannot authorize access.
    fn check<'a>(
        &'a self,
        recipient: &'a RecipientId,
        event: &'a RetainedObservation,
    ) -> Pin<Box<dyn Future<Output = AccessDecision> + Send + 'a>>;
}
/// Verified callback delivery owned by the resident application.
pub trait WakeTransport: Send + Sync + 'static {
    /// Deliver one durably reserved attempt through the specified verified binding.
    ///
    /// Acceptance confirms callback receipt only, never successful handling or settlement.
    fn deliver(
        &self,
        request: WakeRequest,
    ) -> Pin<Box<dyn Future<Output = CallbackOutcome> + Send + '_>>;
}

#[derive(Clone, Debug, Eq, PartialEq)]
/// Shared handling disposition, excluding claim secrets and message bodies.
pub enum EventDisposition {
    /// Handling is unclaimed and may be awaiting delivery or recovery.
    Waiting,
    /// A recipient claim currently owns handling.
    Claimed {
        /// Absolute expiry of the current claim.
        expires_at: DateTime<Utc>,
    },
    /// Handling completed with an explicit settlement.
    Settled(Settlement),
    /// Automatic handling stopped for the recorded reason.
    Parked(ParkReason),
}
#[derive(Clone, Debug, Eq, PartialEq)]
/// Suggested operator action for a retained event's current delivery or handling state.
pub enum RequiredAction {
    /// No operator action is currently required.
    None,
    /// Renew verification of a callback binding before delivery can resume.
    RefreshBinding,
    /// Restore or verify current recipient access before handling resumes.
    RestoreAccess,
    /// Inspect the recorded callback failure before choosing a recovery action.
    InspectDelivery,
    /// Request an authorized replay to start another bounded recovery cycle.
    Replay,
    /// Increase capacity or arrange reviewed administrative retention.
    IncreaseCapacity,
}
#[derive(Clone, Debug)]
/// Inspectable event progress without message content or usable claim capabilities.
pub struct EventStatus {
    /// Stable source occurrence identity.
    pub event: EventKey,
    /// Current shared handling state across matching subscriptions.
    pub disposition: EventDisposition,
    /// Current replay cycle identity.
    pub cycle: u64,
    /// Automatic recovery round within the current cycle.
    pub recovery_round: u8,
    /// Per-subscription spent attempts and most recent delivery failure.
    pub attempts: Vec<(SubscriptionId, u8, Option<DeliveryFailure>)>,
    /// Suggested next action based on the current retained state.
    pub required_action: RequiredAction,
}
#[derive(Clone, Debug)]
/// Recipient-wide progress and storage health, without callback secrets or message bodies.
pub struct RecipientStatus {
    /// Revision of the current authorization configuration.
    pub authorization_revision: AuthorizationRevision,
    /// A visible snapshot still needs directory synchronization before authority is granted.
    pub durability_pending: bool,
    /// Number of subscriptions enabled by operator configuration.
    pub enabled_subscriptions: usize,
    /// Status of each retained occurrence for this recipient.
    pub events: Vec<EventStatus>,
    /// A retained obligation cannot advance within configured storage limits.
    pub capacity_blocked: bool,
}

enum Command {
    Admit(Box<RetainedObservation>),
    Publication(Box<publication::PublicationCommand>),
    Claim(AuthenticatedRecipient, EventKey, RequestId),
    Settle(
        AuthenticatedRecipient,
        EventKey,
        GrantToken,
        RequestId,
        Settlement,
    ),
    Replay(AuthorizedOperator, EventKey, RequestId),
    Configure(AuthorizedOperator, Vec<Subscription>),
    Limits(AuthorizedOperator, Limits),
    Status(AuthenticatedRecipient),
    ContextPermit(
        AuthenticatedRecipient,
        EventKey,
        Option<AuthorizationRevision>,
    ),
    Tick,
    Stop,
    #[cfg(all(test, unix))]
    Crash,
    #[cfg(all(test, unix))]
    FailStorage(Option<super::private_dir::Failure>),
}
enum Response {
    Admission(AdmissionOutcome),
    Publication(Option<super::publication::PublicationStatus>),
    Claim(ClaimOutcome),
    Replay(ReplayOutcome),
    Status(RecipientStatus),
    ContextPermit(Box<super::context::ReadPermit>),
    Done,
}
struct Envelope {
    command: Command,
    reply: oneshot::Sender<Result<Response, OwnerError>>,
}
#[derive(Clone)]
/// Bounded command interface; cancelling a caller does not cancel accepted commits.
pub struct RookHandle {
    sender: mpsc::Sender<Envelope>,
    read_authorizer: Arc<dyn Authorizer>,
    publication_capabilities: super::publication::PublicationCapabilities,
}
/// Supervised recipient owner and its command handle.
pub struct RookService {
    /// Cloneable bounded command interface for the supervised recipient owner.
    pub handle: RookHandle,
    task: JoinHandle<Result<(), OwnerError>>,
}

impl RookHandle {
    async fn request(&self, command: Command) -> Result<Response, OwnerError> {
        let (reply, response) = oneshot::channel();
        self.sender
            .send(Envelope { command, reply })
            .await
            .map_err(|_| OwnerError::new(OwnerErrorKind::Unavailable))?;
        response
            .await
            .map_err(|_| OwnerError::new(OwnerErrorKind::Unavailable))?
    }
    #[cfg(test)]
    pub(super) async fn wait_until_stopped_for_test(&self) {
        self.sender.closed().await;
    }
    pub(super) async fn context_permit(
        &self,
        recipient: AuthenticatedRecipient,
        event: EventKey,
        revision: Option<AuthorizationRevision>,
    ) -> Result<super::context::ReadPermit, OwnerError> {
        match self
            .request(Command::ContextPermit(recipient, event, revision))
            .await?
        {
            Response::ContextPermit(permit) => Ok(*permit),
            _ => Err(OwnerError::invalid()),
        }
    }
    pub(super) async fn context_access(
        &self,
        recipient: &AuthenticatedRecipient,
        observation: &RetainedObservation,
    ) -> AccessDecision {
        tokio::time::timeout(
            NETWORK_TIMEOUT,
            self.read_authorizer.check(recipient.id(), observation),
        )
        .await
        .unwrap_or(AccessDecision::Unavailable)
    }
    /// Admit a verified source observation under current policy in one snapshot.
    pub async fn admit(&self, event: RetainedObservation) -> Result<AdmissionOutcome, OwnerError> {
        match self.request(Command::Admit(Box::new(event))).await? {
            Response::Admission(value) => Ok(value),
            _ => Err(OwnerError::invalid()),
        }
    }
    /// Obtain or recover one claim shared across matching subscriptions.
    pub async fn claim(
        &self,
        recipient: AuthenticatedRecipient,
        event: EventKey,
        id: RequestId,
    ) -> Result<ClaimOutcome, OwnerError> {
        match self.request(Command::Claim(recipient, event, id)).await? {
            Response::Claim(value) => Ok(value),
            _ => Err(OwnerError::invalid()),
        }
    }
    /// Explicitly settle a current claim without disabling any subscription.
    pub async fn settle(
        &self,
        recipient: AuthenticatedRecipient,
        event: EventKey,
        token: GrantToken,
        id: RequestId,
        kind: Settlement,
    ) -> Result<(), OwnerError> {
        self.request(Command::Settle(recipient, event, token, id, kind))
            .await
            .map(|_| ())
    }
    /// Start an operator-authorized recovery cycle while retaining previous history.
    pub async fn replay(
        &self,
        operator: AuthorizedOperator,
        event: EventKey,
        id: RequestId,
    ) -> Result<ReplayOutcome, OwnerError> {
        match self.request(Command::Replay(operator, event, id)).await? {
            Response::Replay(value) => Ok(value),
            _ => Err(OwnerError::invalid()),
        }
    }
    /// Replace operator configuration, preserving retained handling identities.
    pub async fn configure(
        &self,
        operator: AuthorizedOperator,
        subscriptions: Vec<Subscription>,
    ) -> Result<(), OwnerError> {
        self.request(Command::Configure(operator, subscriptions))
            .await
            .map(|_| ())
    }
    /// Inspect disposition and actionable failures without returning message bodies.
    pub async fn status(
        &self,
        recipient: AuthenticatedRecipient,
    ) -> Result<RecipientStatus, OwnerError> {
        match self.request(Command::Status(recipient)).await? {
            Response::Status(value) => Ok(value),
            _ => Err(OwnerError::invalid()),
        }
    }
    /// Evaluate due work using the injected clock; the resident timer also does this.
    pub async fn tick(&self) -> Result<(), OwnerError> {
        self.request(Command::Tick).await.map(|_| ())
    }
    /// Change capacity and timing after operator authorization.
    /// Existing records must fit; rejected changes preserve the previous policy.
    pub async fn set_limits(
        &self,
        operator: AuthorizedOperator,
        limits: Limits,
    ) -> Result<(), OwnerError> {
        self.request(Command::Limits(operator, limits))
            .await
            .map(|_| ())
    }
    #[cfg(all(test, unix))]
    pub(super) async fn fail_storage(
        &self,
        failure: Option<super::private_dir::Failure>,
    ) -> Result<(), OwnerError> {
        self.request(Command::FailStorage(failure))
            .await
            .map(|_| ())
    }
}

#[derive(Default)]
struct PublicationSetup {
    provider: Option<Arc<dyn super::publication::PublicationProvider>>,
    #[cfg(all(test, unix))]
    migration_failure: Option<super::private_dir::Failure>,
}

impl RookService {
    /// Open private storage and start an owner using trusted application boundaries.
    ///
    /// Existing state must match this recipient and its configured limits. No
    /// production authentication or webhook registration is performed here.
    pub async fn open(
        root: PathBuf,
        recipient: RecipientId,
        limits: Limits,
        clock: Arc<dyn Clock>,
        authorizer: Arc<dyn Authorizer>,
        transport: Arc<dyn WakeTransport>,
    ) -> Result<Self, OwnerError> {
        Self::open_inner(
            root,
            recipient,
            limits,
            clock,
            authorizer,
            transport,
            PublicationSetup::default(),
        )
        .await
    }
    async fn open_inner(
        root: PathBuf,
        recipient: RecipientId,
        limits: Limits,
        clock: Arc<dyn Clock>,
        authorizer: Arc<dyn Authorizer>,
        transport: Arc<dyn WakeTransport>,
        publication_setup: PublicationSetup,
    ) -> Result<Self, OwnerError> {
        if !limits.valid() {
            return Err(OwnerError::invalid());
        }
        let (disk, bytes) = Disk::open(root, recipient.clone(), limits.snapshot_bytes).await?;
        #[cfg(all(test, unix))]
        if let Some(failure) = publication_setup.migration_failure {
            disk.fail(Some(failure)).await?;
        }
        let publication_provider = publication_setup.provider;
        let existing = bytes.is_some();
        let mut migrated = false;
        let loaded = (|| -> Result<RecipientState, OwnerError> {
            Ok(match bytes {
                Some(bytes) => {
                    let (state, migration) = RecipientState::decode(&bytes)?;
                    migrated = migration;
                    if state.recipient != recipient || state.limits != limits {
                        return Err(OwnerError::invalid());
                    }
                    state
                }
                None => RecipientState {
                    version: 2,
                    recipient,
                    revision: AuthorizationRevision::new(0),
                    high_water: clock.now(),
                    limits,
                    subscriptions: Vec::new(),
                    events: Vec::new(),
                    claims: Vec::new(),
                    replays: Vec::new(),
                    publications: Vec::new(),
                },
            })
        })();
        let mut state = match loaded {
            Ok(state) => state,
            Err(error) => {
                disk.close().await?;
                return Err(error);
            }
        };
        for record in &mut state.publications {
            if matches!(
                record.state,
                super::publication::PublicationState::Dispatching
            ) {
                record.state = super::publication::PublicationState::Unknown {
                    reason: super::publication::PublicationUncertainty::Restarted,
                };
                migrated = true;
            }
        }
        let publication_capabilities = publication_provider.as_ref().map_or(
            super::publication::PublicationCapabilities {
                text: false,
                max_text_bytes: 0,
                attachments: false,
            },
            |provider| provider.capabilities(),
        );
        let read_authorizer = authorizer.clone();
        let mut owner = Owner {
            state,
            disk,
            pending: false,
            clock,
            authorizer,
            transport,
            sends: JoinSet::new(),
            publication_provider,
            publication_tasks: JoinSet::new(),
            publication_followups: BTreeMap::new(),
            publication_bookkeeping: JoinSet::new(),
            publication_active: BTreeSet::new(),
            publication_completions: VecDeque::new(),
            undispatched: None,
            retry_completions: VecDeque::new(),
            cursor: 0,
            capacity_blocked: BTreeSet::new(),
            recovery_capacity_blocked: false,
            active_event: None,
            recovering: existing,
        };
        if (!existing || migrated)
            && let Err(error) = owner.commit(owner.state.clone()).await
        {
            owner.disk.close().await?;
            return Err(error);
        }
        let (sender, receiver) = mpsc::channel(64);
        let task = tokio::spawn(owner.run(receiver));
        Ok(Self {
            handle: RookHandle {
                sender,
                read_authorizer,
                publication_capabilities,
            },
            task,
        })
    }
    /// Open with a trusted text-publication adapter and the ordinary durable owner.
    pub async fn open_with_publication(
        root: PathBuf,
        recipient: RecipientId,
        limits: Limits,
        clock: Arc<dyn Clock>,
        authorizer: Arc<dyn Authorizer>,
        transport: Arc<dyn WakeTransport>,
        publication: Arc<dyn super::publication::PublicationProvider>,
    ) -> Result<Self, OwnerError> {
        Self::open_inner(
            root,
            recipient,
            limits,
            clock,
            authorizer,
            transport,
            PublicationSetup {
                provider: Some(publication),
                #[cfg(all(test, unix))]
                migration_failure: None,
            },
        )
        .await
    }
    #[cfg(all(test, unix))]
    pub(super) async fn open_with_publication_fault(
        root: PathBuf,
        recipient: RecipientId,
        limits: Limits,
        clock: Arc<dyn Clock>,
        authorizer: Arc<dyn Authorizer>,
        transport: Arc<dyn WakeTransport>,
        publication: (
            Arc<dyn super::publication::PublicationProvider>,
            super::private_dir::Failure,
        ),
    ) -> Result<Self, OwnerError> {
        Self::open_inner(
            root,
            recipient,
            limits,
            clock,
            authorizer,
            transport,
            PublicationSetup {
                provider: Some(publication.0),
                migration_failure: Some(publication.1),
            },
        )
        .await
    }
    #[cfg(all(test, unix))]
    pub(super) async fn crash(self) {
        self.handle.request(Command::Crash).await.unwrap();
        self.task.await.unwrap().unwrap();
    }
    /// Close intake, drain accepted commands, and join the owner and disk worker.
    pub async fn shutdown(self) -> Result<(), OwnerError> {
        let _ = self.handle.request(Command::Stop).await;
        self.task.await.map_err(OwnerError::worker_join)?
    }
}

#[derive(Clone)]
struct Completion {
    request: WakeRequest,
    outcome: CallbackOutcome,
}
struct Owner {
    state: RecipientState,
    disk: Disk,
    pending: bool,
    clock: Arc<dyn Clock>,
    authorizer: Arc<dyn Authorizer>,
    transport: Arc<dyn WakeTransport>,
    sends: JoinSet<Completion>,
    publication_provider: Option<Arc<dyn super::publication::PublicationProvider>>,
    publication_tasks: JoinSet<publication::PublicationTaskResult>,
    publication_followups: BTreeMap<EventKey, oneshot::Sender<()>>,
    publication_bookkeeping: JoinSet<()>,
    publication_active: BTreeSet<EventKey>,
    publication_completions: VecDeque<publication::PendingPublicationCompletion>,
    undispatched: Option<WakeRequest>,
    retry_completions: VecDeque<Completion>,
    cursor: usize,
    capacity_blocked: BTreeSet<EventKey>,
    recovery_capacity_blocked: bool,
    active_event: Option<EventKey>,
    recovering: bool,
}
impl Owner {
    fn record_capacity_block(&mut self) {
        if let Some(event) = &self.active_event {
            self.capacity_blocked.insert(event.clone());
        } else {
            self.recovery_capacity_blocked = true;
        }
    }
    fn now(&self) -> DateTime<Utc> {
        self.clock.now().max(self.state.high_water)
    }
    async fn sync(&mut self) -> Result<(), OwnerError> {
        self.disk.sync().await?;
        self.pending = false;
        Ok(())
    }
    async fn commit(&mut self, next: RecipientState) -> Result<(), OwnerError> {
        self.sync().await?;
        next.validate()?;
        let bytes = serde_json::to_vec(&next).map_err(|_| OwnerError::invalid())?;
        if bytes.len().saturating_add(next.publication_headroom()) > next.limits.snapshot_bytes {
            return Err(OwnerError::new(OwnerErrorKind::Capacity));
        }
        match self.disk.write(bytes).await {
            Ok(()) => {
                self.state = next;
                Ok(())
            }
            Err(error) => {
                if matches!(&error.kind, OwnerErrorKind::Storage(storage) if storage.kind == StorageErrorKind::VisibleDurabilityUncertain)
                {
                    self.state = next;
                    self.pending = true;
                }
                Err(error)
            }
        }
    }
    fn authenticate(&self, recipient: &AuthenticatedRecipient) -> Result<(), OwnerError> {
        if recipient.id() == &self.state.recipient {
            Ok(())
        } else {
            Err(OwnerError::new(OwnerErrorKind::Denied))
        }
    }
    fn event_index(&self, key: &EventKey) -> Result<usize, OwnerError> {
        self.state
            .events
            .iter()
            .position(|e| e.observation.key() == key)
            .ok_or_else(|| OwnerError::new(OwnerErrorKind::UnknownEvent))
    }
    async fn access(&self, observation: &RetainedObservation) -> AccessDecision {
        tokio::time::timeout(
            NETWORK_TIMEOUT,
            self.authorizer.check(&self.state.recipient, observation),
        )
        .await
        .unwrap_or(AccessDecision::Unavailable)
    }
    fn eligible(&self, event: &RetainedObservation) -> bool {
        self.state.subscriptions.iter().any(|s| s.admits(event))
    }
    async fn require_access(&self, event: &RetainedObservation) -> Result<(), OwnerError> {
        if !self.eligible(event) {
            return Err(OwnerError::new(OwnerErrorKind::Denied));
        }
        match self.access(event).await {
            AccessDecision::Allowed => Ok(()),
            AccessDecision::Denied => Err(OwnerError::new(OwnerErrorKind::Denied)),
            AccessDecision::Unavailable => Err(OwnerError::new(OwnerErrorKind::AccessUnavailable)),
        }
    }
    async fn run(mut self, mut receiver: mpsc::Receiver<Envelope>) -> Result<(), OwnerError> {
        let mut interval = tokio::time::interval(Duration::from_millis(100));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut result = Ok(());
        let mut stopping = false;
        #[cfg(all(test, unix))]
        let mut crashed = false;
        loop {
            tokio::select! {
                command = receiver.recv() => {
                    let Some(command) = command else { break; };
                    #[cfg(all(test, unix))]
                    if matches!(&command.command, Command::Crash) {
                        crashed = true;
                        let _ = command.reply.send(Ok(Response::Done));
                        break;
                    }
                    if matches!(&command.command, Command::Stop) {
                        receiver.close();
                        stopping = true;
                        let _ = command.reply.send(Ok(Response::Done));
                        continue;
                    }
                    let response = self.command(command.command).await;
                    let _ = command.reply.send(response);
                }
                completion = self.publication_bookkeeping.join_next(), if !self.publication_bookkeeping.is_empty() => {
                    if let Some(Err(error)) = completion {
                        tracing::warn!(panicked = error.is_panic(), cancelled = error.is_cancelled(), "publication bookkeeping worker failed after durable receipt");
                    }
                }
                completion = self.publication_tasks.join_next(), if !self.publication_tasks.is_empty() => {
                    match completion {
                        Some(Ok(completion)) => self.retain_publication_task(completion).await,
                        Some(Err(error)) => { result = Err(OwnerError::worker_join(error)); break; }
                        None => {}
                    }
                }
                completion = self.sends.join_next(), if !self.sends.is_empty() => {
                    match completion {
                        Some(Ok(completion)) => if let Err(error) = self.complete(completion.clone()).await {
                            if matches!(&error.kind, OwnerErrorKind::Storage(_) | OwnerErrorKind::Capacity) {
                                if matches!(error.kind, OwnerErrorKind::Capacity) { self.record_capacity_block(); }
                                self.retry_completions.push_back(completion);
                            }
                            else { result = Err(error); break; }
                        },
                        Some(Err(error)) => { result = Err(OwnerError::worker_join(error)); break; }
                        None => {}
                    }
                }
                _ = interval.tick(), if !stopping => {
                    if let Err(error) = self.drive().await {
                        // An uncertain snapshot remains recoverable through status and the next sync.
                        if matches!(error.kind, OwnerErrorKind::Capacity) { self.record_capacity_block(); }
                        else if !matches!(&error.kind, OwnerErrorKind::Storage(_)) { result = Err(error); break; }
                    }
                }
            }
        }
        #[cfg(all(test, unix))]
        if crashed {
            self.publication_bookkeeping.abort_all();
            self.publication_tasks.abort_all();
            while self.publication_tasks.join_next().await.is_some() {}
            self.sends.abort_all();
            while self.sends.join_next().await.is_some() {}
            return self.disk.close().await;
        }
        loop {
            while let Some(completion) = self.publication_tasks.join_next().await {
                match completion {
                    Ok(completion) => self.retain_publication_task(completion).await,
                    Err(error) => {
                        if result.is_ok() {
                            result = Err(OwnerError::worker_join(error));
                        } else {
                            tracing::warn!(
                                panicked = error.is_panic(),
                                cancelled = error.is_cancelled(),
                                "publication worker failed while draining an earlier error"
                            );
                        }
                    }
                }
            }
            if let Err(error) = self.retry_publication_completions().await {
                if result.is_ok() {
                    result = Err(error);
                } else {
                    tracing::warn!(
                        "publication completion retry failed while draining an earlier error"
                    );
                }
                break;
            }
            if self.publication_tasks.is_empty() {
                break;
            }
        }
        self.publication_followups.clear();
        while let Some(completion) = self.publication_bookkeeping.join_next().await {
            if let Err(error) = completion {
                tracing::warn!(
                    panicked = error.is_panic(),
                    cancelled = error.is_cancelled(),
                    "publication bookkeeping worker failed during shutdown"
                );
            }
        }
        self.sends.abort_all();
        while let Some(completion) = self.sends.join_next().await {
            if let Err(error) = completion {
                if error.is_cancelled() {
                    continue;
                }
                if result.is_ok() {
                    result = Err(OwnerError::worker_join(error));
                } else {
                    tracing::warn!(
                        panicked = error.is_panic(),
                        cancelled = error.is_cancelled(),
                        "wake worker failed while draining an earlier error"
                    );
                }
            }
        }
        // Dispatching records intentionally survive shutdown as uncertain deliveries.
        let close = self.disk.close().await;
        result.and(close)
    }
    async fn command(&mut self, command: Command) -> Result<Response, OwnerError> {
        if let Command::Status(recipient) = command {
            self.authenticate(&recipient)?;
            return Ok(Response::Status(self.status()));
        }
        #[cfg(all(test, unix))]
        if let Command::FailStorage(failure) = command {
            self.disk.fail(failure).await?;
            return Ok(Response::Done);
        }
        if let Command::Publication(command) = command {
            return self
                .publication_command(*command)
                .await
                .map(Response::Publication);
        }
        self.sync().await?;
        match command {
            Command::ContextPermit(recipient, key, revision) => {
                self.authenticate(&recipient)?;
                if revision.is_some_and(|revision| revision != self.state.revision) {
                    return Err(OwnerError::new(OwnerErrorKind::Denied));
                }
                let observation = &self.state.events[self.event_index(&key)?].observation;
                if !self.eligible(observation) {
                    return Err(OwnerError::new(OwnerErrorKind::Denied));
                }
                Ok(Response::ContextPermit(Box::new(
                    super::context::ReadPermit {
                        observation: observation.clone(),
                        revision: self.state.revision,
                    },
                )))
            }
            Command::Admit(event) => self.admit(*event).await.map(Response::Admission),
            Command::Claim(recipient, event, id) => {
                self.authenticate(&recipient)?;
                self.claim(event, id).await.map(Response::Claim)
            }
            Command::Settle(recipient, event, token, id, kind) => {
                self.authenticate(&recipient)?;
                self.settle(event, token, id, kind).await?;
                Ok(Response::Done)
            }
            Command::Replay(_, event, id) => self.replay(event, id).await.map(Response::Replay),
            Command::Configure(_, subscriptions) => {
                self.configure(subscriptions).await?;
                Ok(Response::Done)
            }
            Command::Limits(_, limits) => {
                if !limits.valid() {
                    return Err(OwnerError::invalid());
                }
                let mut next = self.state.clone();
                next.limits = limits;
                self.commit(next).await?;
                self.capacity_blocked.clear();
                self.recovery_capacity_blocked = false;
                Ok(Response::Done)
            }
            Command::Tick => {
                if let Err(error) = self.drive().await {
                    if matches!(error.kind, OwnerErrorKind::Capacity) {
                        self.record_capacity_block();
                    }
                    return Err(error);
                }
                Ok(Response::Done)
            }
            Command::Publication(_) | Command::Status(_) | Command::Stop => {
                Err(OwnerError::invalid())
            }
            #[cfg(all(test, unix))]
            Command::FailStorage(_) | Command::Crash => Err(OwnerError::invalid()),
        }
    }
    async fn admit(
        &mut self,
        observation: RetainedObservation,
    ) -> Result<AdmissionOutcome, OwnerError> {
        observation.validate().map_err(|_| OwnerError::invalid())?;
        if let Some(existing) = self
            .state
            .events
            .iter()
            .find(|e| e.observation.key() == observation.key())
        {
            return if existing.observation == observation {
                Ok(AdmissionOutcome::Duplicate)
            } else {
                Err(OwnerError::new(OwnerErrorKind::ConflictingRevision))
            };
        }
        if !self.eligible(&observation) {
            return Ok(AdmissionOutcome::Excluded);
        }
        self.require_access(&observation).await?;
        let now = self.now();
        let links: Vec<_> = self
            .state
            .subscriptions
            .iter()
            .filter(|s| s.admits(&observation))
            .map(|s| Link {
                subscription: s.id.clone(),
                attempts: 0,
                last_failure: None,
                wake: WakeState::Pending { due: now },
            })
            .collect();
        let count = links.len();
        let mut next = self.state.clone();
        next.high_water = now;
        next.events.push(EventRecord {
            observation,
            handling: Handling::Unclaimed,
            links,
            claim_generation: ClaimGeneration::new(0),
            cycle: 0,
            round: 0,
            recovery_generation: RecoveryGeneration::new(0),
            accepted_deadline: None,
            recovery_binding: None,
            access_failures: 0,
            access_due: None,
            history: Vec::new(),
        });
        self.commit(next).await?;
        Ok(AdmissionOutcome::Inserted(count))
    }
    async fn claim(&mut self, key: EventKey, id: RequestId) -> Result<ClaimOutcome, OwnerError> {
        let index = self.event_index(&key)?;
        self.require_access(&self.state.events[index].observation)
            .await?;
        let now = self.now();
        if let Some(old) = self.state.claims.iter().find(|c| c.id == id) {
            if old.event != key {
                return Err(OwnerError::new(OwnerErrorKind::RequestConflict));
            }
            return Ok(match &self.state.events[index].handling {
                Handling::Claimed { grant, .. }
                    if old.grant.as_ref() == Some(grant) && grant.expires_at > now =>
                {
                    ClaimOutcome::Granted(grant.clone())
                }
                Handling::Settled { kind, .. } => ClaimOutcome::Settled(kind.clone()),
                Handling::Claimed { grant, .. }
                    if old.grant.is_none() && grant.expires_at > now =>
                {
                    ClaimOutcome::Busy {
                        expires_at: grant.expires_at,
                    }
                }
                _ => ClaimOutcome::RecoveryRequired,
            });
        }
        let unavailable = match &self.state.events[index].handling {
            Handling::Claimed { grant, .. } => Some(if grant.expires_at > now {
                ClaimOutcome::Busy {
                    expires_at: grant.expires_at,
                }
            } else {
                ClaimOutcome::RecoveryRequired
            }),
            Handling::Settled { kind, .. } => Some(ClaimOutcome::Settled(kind.clone())),
            Handling::Parked { .. } => Some(ClaimOutcome::RecoveryRequired),
            Handling::Unclaimed => self.state.events[index]
                .accepted_deadline
                .is_some_and(|due| due <= now)
                .then_some(ClaimOutcome::RecoveryRequired),
        };
        if let Some(outcome) = unavailable {
            let mut next = self.state.clone();
            next.high_water = now;
            next.claims.push(ClaimHistory {
                id,
                event: key,
                grant: None,
            });
            self.commit(next).await?;
            return Ok(outcome);
        }
        let mut next = self.state.clone();
        let generation = next.events[index].claim_generation.checked_next()?;
        let grant = ClaimGrant {
            token: GrantToken::new(),
            generation,
            authorization_revision: next.revision,
            expires_at: deadline(now, next.limits.claim_seconds)?,
        };
        next.high_water = now;
        next.events[index].claim_generation = generation;
        next.events[index].handling = Handling::Claimed {
            request: id.clone(),
            grant: grant.clone(),
        };
        next.claims.push(ClaimHistory {
            id,
            event: key,
            grant: Some(grant.clone()),
        });
        self.commit(next).await?;
        Ok(ClaimOutcome::Granted(grant))
    }
    async fn settle(
        &mut self,
        key: EventKey,
        token: GrantToken,
        id: RequestId,
        kind: Settlement,
    ) -> Result<(), OwnerError> {
        let index = self.event_index(&key)?;
        self.require_access(&self.state.events[index].observation)
            .await?;
        let now = self.now();
        if self.state.events.iter().any(|event| {
            matches!(&event.handling, Handling::Settled { request, .. } if request == &id)
                && event.observation.key() != &key
        }) {
            return Err(OwnerError::new(OwnerErrorKind::RequestConflict));
        }
        if let Handling::Settled {
            kind: previous,
            request,
            generation,
        } = &self.state.events[index].handling
        {
            if request == &id
                && previous == &kind
                && self.state.claims.iter().any(|c| {
                    c.event == key
                        && c.grant.as_ref().is_some_and(|grant| {
                            grant.generation == *generation && grant.token == token
                        })
                })
            {
                return Ok(());
            }
            return Err(OwnerError::new(OwnerErrorKind::RequestConflict));
        }
        let Handling::Claimed { grant, .. } = &self.state.events[index].handling else {
            return Err(OwnerError::new(OwnerErrorKind::InvalidClaim));
        };
        if grant.token != token
            || grant.expires_at <= now
            || grant.authorization_revision != self.state.revision
        {
            return Err(OwnerError::new(OwnerErrorKind::InvalidClaim));
        }
        let generation = grant.generation;
        let mut next = self.state.clone();
        next.high_water = now;
        let event = &mut next.events[index];
        event.handling = Handling::Settled {
            kind,
            request: id,
            generation,
        };
        event.accepted_deadline = None;
        event.recovery_generation = event.recovery_generation.checked_next()?;
        for link in &mut event.links {
            link.wake = WakeState::Cancelled;
        }
        self.commit(next).await
    }
    async fn replay(&mut self, key: EventKey, id: RequestId) -> Result<ReplayOutcome, OwnerError> {
        let index = self.event_index(&key)?;
        self.require_access(&self.state.events[index].observation)
            .await?;
        if let Some(old) = self.state.replays.iter().find(|r| r.id == id) {
            return if old.event == key {
                Ok(ReplayOutcome::Scheduled { cycle: old.cycle })
            } else {
                Err(OwnerError::new(OwnerErrorKind::RequestConflict))
            };
        }
        if let Handling::Settled { kind, .. } = &self.state.events[index].handling {
            return Ok(ReplayOutcome::Settled(kind.clone()));
        }
        if matches!(self.state.events[index].handling, Handling::Claimed { .. }) {
            return Err(OwnerError::new(OwnerErrorKind::InvalidClaim));
        }
        let now = self.now();
        let mut next = self.state.clone();
        next.high_water = now;
        let event = &mut next.events[index];
        event.history.push(RecoveryHistory {
            cycle: event.cycle,
            round: event.round,
            reason: ParkReason::DeliveryAttemptsExhausted,
            attempts: event
                .links
                .iter()
                .map(|l| (l.subscription.clone(), l.attempts))
                .collect(),
        });
        event.cycle = increment(event.cycle)?;
        event.recovery_generation = event.recovery_generation.checked_next()?;
        event.round = 0;
        event.handling = Handling::Unclaimed;
        event.accepted_deadline = None;
        event.recovery_binding = None;
        event.access_due = None;
        event.access_failures = 0;
        for link in &mut event.links {
            link.attempts = 0;
            link.wake = WakeState::Pending { due: now };
        }
        let cycle = event.cycle;
        next.replays.push(ReplayHistory {
            id,
            event: key,
            cycle,
        });
        self.commit(next).await?;
        Ok(ReplayOutcome::Scheduled { cycle })
    }
    async fn configure(&mut self, subscriptions: Vec<Subscription>) -> Result<(), OwnerError> {
        if subscriptions == self.state.subscriptions {
            return Ok(());
        }
        let now = self.now();
        let mut next = self.state.clone();
        // Binding renewal changes availability, not authorization or handling identity.
        let changed_policy = next.subscriptions.len() != subscriptions.len()
            || next.subscriptions.iter().any(|old| {
                !subscriptions.iter().any(|new| {
                    old.id == new.id
                        && old.enabled == new.enabled
                        && old.rooms == new.rooms
                        && old.senders == new.senders
                })
            });
        if changed_policy {
            next.revision = next.revision.checked_next()?;
        }
        next.subscriptions = subscriptions;
        next.high_water = now;
        for event in &mut next.events {
            if changed_policy && matches!(event.handling, Handling::Claimed { .. }) {
                event.handling = Handling::Unclaimed;
                event.accepted_deadline = Some(now);
            }
            for link in &mut event.links {
                if let WakeState::Dispatching { generation, .. } = link.wake {
                    let still_bound = next.subscriptions.iter().any(|s| s.id == link.subscription
                        && s.admits(&event.observation) && matches!(s.binding, BindingState::Verified { generation: current, .. } if current == generation));
                    if changed_policy || !still_bound {
                        link.retry_after_interruption(
                            DeliveryFailure::Unavailable,
                            now,
                            next.limits.attempts_per_binding,
                        );
                    }
                }
            }
            if event.recovery_binding.as_ref().is_some_and(|id| !next.subscriptions.iter().any(|s| &s.id == id && s.admits(&event.observation)
                && matches!(s.binding, BindingState::Verified { refresh_before, .. } if refresh_before > now))) { event.recovery_binding = None; }
            for subscription in &next.subscriptions {
                if subscription.admits(&event.observation)
                    && !event
                        .links
                        .iter()
                        .any(|l| l.subscription == subscription.id)
                {
                    event.links.push(Link {
                        subscription: subscription.id.clone(),
                        attempts: 0,
                        last_failure: None,
                        wake: WakeState::Pending { due: now },
                    });
                }
            }
        }
        self.commit(next).await
    }
    async fn complete(&mut self, completion: Completion) -> Result<(), OwnerError> {
        self.active_event = Some(completion.request.event.clone());
        self.sync().await?;
        let index = self.event_index(&completion.request.event)?;
        let now = self.now();
        let mut next = self.state.clone();
        next.high_water = now;
        let event = &mut next.events[index];
        if event.recovery_generation != completion.request.recovery_generation
            || matches!(event.handling, Handling::Settled { .. })
        {
            return Ok(());
        }
        if !next.subscriptions.iter().any(|s| s.id == completion.request.subscription && s.admits(&event.observation)
            && matches!(s.binding, BindingState::Verified { generation, .. } if generation == completion.request.binding_generation)) { return Ok(()); }
        let Some(link) = event
            .links
            .iter_mut()
            .find(|l| l.subscription == completion.request.subscription)
        else {
            return Ok(());
        };
        if !matches!(&link.wake, WakeState::Dispatching { attempt, generation, authorization_revision } if attempt == &completion.request.attempt && *generation == completion.request.binding_generation && *authorization_revision == next.revision)
        {
            return Ok(());
        }
        match completion.outcome {
            CallbackOutcome::Accepted => {
                link.wake = WakeState::Accepted;
                if event.accepted_deadline.is_none() {
                    event.accepted_deadline = Some(deadline(now, next.limits.claim_wait_seconds)?);
                }
            }
            CallbackOutcome::Failed(failure) => {
                link.last_failure = Some(failure.clone());
                link.wake = if link.attempts >= next.limits.attempts_per_binding {
                    WakeState::Exhausted { failure }
                } else {
                    WakeState::Retry {
                        due: deadline(now, next.limits.retry_seconds)?,
                        failure,
                    }
                };
            }
        }
        self.commit(next).await
    }
    async fn drive(&mut self) -> Result<(), OwnerError> {
        self.sync().await?;
        self.retry_publication_completions().await?;
        self.active_event = None;
        if self.recovery_capacity_blocked {
            return Ok(());
        }
        if self.recovering {
            let mut next = self.state.clone();
            let now = self.now();
            for event in &mut next.events {
                for link in &mut event.links {
                    if matches!(link.wake, WakeState::Dispatching { .. }) {
                        link.retry_after_interruption(
                            DeliveryFailure::Restarted,
                            now,
                            next.limits.attempts_per_binding,
                        );
                    }
                }
            }
            next.high_water = now;
            self.commit(next).await?;
            self.recovering = false;
        }
        if let Some(index) = self
            .retry_completions
            .iter()
            .position(|completion| !self.capacity_blocked.contains(&completion.request.event))
        {
            let completion = self
                .retry_completions
                .remove(index)
                .ok_or_else(OwnerError::invalid)?;
            if let Err(error) = self.complete(completion.clone()).await {
                self.retry_completions.push_back(completion);
                return Err(error);
            }
        }
        let now = self.now();
        if let Some(request) = self
            .undispatched
            .as_ref()
            .filter(|request| !self.capacity_blocked.contains(&request.event))
        {
            self.active_event = Some(request.event.clone());
            let mut next = self.state.clone();
            if let Some(event) = next
                .events
                .iter_mut()
                .find(|event| event.observation.key() == &request.event)
                && let Some(link) = event
                    .links
                    .iter_mut()
                    .find(|link| link.subscription == request.subscription)
                && matches!(&link.wake, WakeState::Dispatching { attempt, .. } if attempt == &request.attempt)
            {
                link.retry_after_interruption(
                    DeliveryFailure::Unavailable,
                    now,
                    next.limits.attempts_per_binding,
                );
                next.high_water = now;
                self.commit(next).await?;
            }
            self.undispatched = None;
        }
        let count = self.state.events.len();
        let start = self.cursor;
        for offset in 0..count {
            let index = (start + offset) % count;
            self.cursor = (index + 1) % count;
            if self
                .capacity_blocked
                .contains(self.state.events[index].observation.key())
            {
                continue;
            }
            self.active_event = Some(self.state.events[index].observation.key().clone());
            let reason = match &self.state.events[index].handling {
                Handling::Claimed { grant, .. } if grant.expires_at <= now => {
                    Some(ParkReason::ClaimExpired)
                }
                Handling::Unclaimed
                    if self.state.events[index]
                        .accepted_deadline
                        .is_some_and(|due| due <= now) =>
                {
                    Some(ParkReason::ClaimNotStarted)
                }
                _ => None,
            };
            if let Some(reason) = reason {
                let mut next = self.state.clone();
                next.high_water = now;
                let event = &mut next.events[index];
                event.history.push(RecoveryHistory {
                    cycle: event.cycle,
                    round: event.round,
                    reason: reason.clone(),
                    attempts: event
                        .links
                        .iter()
                        .map(|l| (l.subscription.clone(), l.attempts))
                        .collect(),
                });
                event.recovery_generation = event.recovery_generation.checked_next()?;
                event.accepted_deadline = None;
                event.recovery_binding = None;
                if event.round >= next.limits.recovery_rounds {
                    event.handling = Handling::Parked { reason };
                } else {
                    event.round += 1;
                    event.handling = Handling::Unclaimed;
                    for link in &mut event.links {
                        link.wake = if link.attempts >= next.limits.attempts_per_binding {
                            WakeState::Exhausted {
                                failure: link
                                    .last_failure
                                    .clone()
                                    .unwrap_or(DeliveryFailure::Unavailable),
                            }
                        } else {
                            WakeState::Pending { due: now }
                        };
                    }
                }
                self.commit(next).await?;
            }
            if !matches!(self.state.events[index].handling, Handling::Unclaimed)
                || self.sends.len() >= 4
            {
                continue;
            }
            if self.state.events[index]
                .access_due
                .is_some_and(|due| due > now)
            {
                continue;
            }
            if !self.eligible(&self.state.events[index].observation) {
                self.park(index, ParkReason::AccessDenied, now).await?;
                continue;
            }
            let decision = self.access(&self.state.events[index].observation).await;
            match decision {
                AccessDecision::Denied => {
                    self.park(index, ParkReason::AccessDenied, now).await?;
                    break;
                }
                AccessDecision::Unavailable => {
                    let mut next = self.state.clone();
                    next.high_water = now;
                    let event = &mut next.events[index];
                    event.access_failures = event.access_failures.saturating_add(1);
                    if event.access_failures >= next.limits.access_retries {
                        event.handling = Handling::Parked {
                            reason: ParkReason::AccessUnavailable,
                        };
                    } else {
                        event.access_due = Some(deadline(now, next.limits.retry_seconds)?);
                    }
                    self.commit(next).await?;
                    break;
                }
                AccessDecision::Allowed => {}
            }
            let now = self.now();
            let event = &self.state.events[index];
            let progress = binding_progress(
                event,
                &self.state.subscriptions,
                self.state.limits.attempts_per_binding,
                now,
            );
            let selected_still_usable = event.recovery_binding.as_ref().is_some_and(|chosen| {
                progress
                    .iter()
                    .any(|(subscription, state)| &subscription.id == chosen && state.can_progress())
            });
            let selected = progress.iter().find_map(|(subscription, state)| {
                if event.round > 0
                    && selected_still_usable
                    && event
                        .recovery_binding
                        .as_ref()
                        .is_some_and(|chosen| chosen != &subscription.id)
                {
                    return None;
                }
                match state {
                    BindingProgress::Usable { generation, due } if *due <= now => {
                        Some((subscription.id.clone(), *generation))
                    }
                    _ => None,
                }
            });
            let Some((subscription, binding_generation)) = selected else {
                let all_exhausted = progress
                    .iter()
                    .all(|(_, state)| matches!(state, BindingProgress::Exhausted));
                if all_exhausted && event.accepted_deadline.is_none() {
                    self.park(index, ParkReason::DeliveryAttemptsExhausted, now)
                        .await?;
                }
                break;
            };
            let mut next = self.state.clone();
            next.high_water = now;
            let event = &mut next.events[index];
            event.access_due = None;
            let request = WakeRequest {
                recipient: next.recipient.clone(),
                event: event.observation.key().clone(),
                subscription: subscription.clone(),
                binding_generation,
                attempt: AttemptId::new(),
                recovery_generation: event.recovery_generation,
                observation: event.observation.clone(),
            };
            if event.round > 0 {
                event.recovery_binding = Some(subscription.clone());
            }
            let link = event
                .links
                .iter_mut()
                .find(|l| l.subscription == subscription)
                .ok_or_else(OwnerError::invalid)?;
            link.attempts += 1;
            link.wake = WakeState::Dispatching {
                attempt: request.attempt.clone(),
                generation: binding_generation,
                authorization_revision: next.revision,
            };
            if let Err(error) = self.commit(next).await {
                if self.pending {
                    self.undispatched = Some(request);
                }
                return Err(error);
            }
            let transport = self.transport.clone();
            self.sends.spawn(async move {
                let outcome =
                    tokio::time::timeout(NETWORK_TIMEOUT, transport.deliver(request.clone()))
                        .await
                        .unwrap_or(CallbackOutcome::Failed(DeliveryFailure::Timeout));
                Completion { request, outcome }
            });
            break;
        }
        Ok(())
    }
    async fn park(
        &mut self,
        index: usize,
        reason: ParkReason,
        now: DateTime<Utc>,
    ) -> Result<(), OwnerError> {
        let mut next = self.state.clone();
        next.high_water = now;
        next.events[index].handling = Handling::Parked { reason };
        next.events[index].recovery_generation =
            next.events[index].recovery_generation.checked_next()?;
        for link in &mut next.events[index].links {
            if !matches!(link.wake, WakeState::Accepted) {
                link.wake = WakeState::Cancelled;
            }
        }
        self.commit(next).await
    }
    fn status(&self) -> RecipientStatus {
        let now = self.now();
        RecipientStatus {
            authorization_revision: self.state.revision,
            durability_pending: self.pending,
            capacity_blocked: self.recovery_capacity_blocked || !self.capacity_blocked.is_empty(),
            enabled_subscriptions: self
                .state
                .subscriptions
                .iter()
                .filter(|s| s.enabled)
                .count(),
            events: self
                .state
                .events
                .iter()
                .map(|event| {
                    let disposition = match &event.handling {
                        Handling::Unclaimed => EventDisposition::Waiting,
                        Handling::Claimed { grant, .. } => EventDisposition::Claimed {
                            expires_at: grant.expires_at,
                        },
                        Handling::Settled { kind, .. } => EventDisposition::Settled(kind.clone()),
                        Handling::Parked { reason } => EventDisposition::Parked(reason.clone()),
                    };
                    let progress = binding_progress(
                        event,
                        &self.state.subscriptions,
                        self.state.limits.attempts_per_binding,
                        now,
                    );
                    let needs_refresh = progress
                        .iter()
                        .any(|(_, state)| matches!(state, BindingProgress::Refreshable))
                        && !progress.iter().any(|(_, state)| state.can_progress());
                    let required_action = match &event.handling {
                        _ if self.recovery_capacity_blocked
                            || self.capacity_blocked.contains(event.observation.key()) =>
                        {
                            RequiredAction::IncreaseCapacity
                        }
                        Handling::Settled { .. } | Handling::Claimed { .. } => RequiredAction::None,
                        Handling::Parked {
                            reason: ParkReason::AccessDenied | ParkReason::AccessUnavailable,
                        } => RequiredAction::RestoreAccess,
                        Handling::Parked { .. } => RequiredAction::Replay,
                        _ if event.access_due.is_some() => RequiredAction::RestoreAccess,
                        _ if needs_refresh => RequiredAction::RefreshBinding,
                        _ if event.links.iter().any(|l| l.last_failure.is_some()) => {
                            RequiredAction::InspectDelivery
                        }
                        _ => RequiredAction::None,
                    };
                    EventStatus {
                        event: event.observation.key().clone(),
                        disposition,
                        cycle: event.cycle,
                        recovery_round: event.round,
                        attempts: event
                            .links
                            .iter()
                            .map(|l| (l.subscription.clone(), l.attempts, l.last_failure.clone()))
                            .collect(),
                        required_action,
                    }
                })
                .collect(),
        }
    }
}
