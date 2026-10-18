//! Requested Discord presence, acknowledgement, and reconnect replay.

use chrono::{DateTime, Utc};
use std::{fmt, str::FromStr, sync::Arc};
use thiserror::Error;
use tokio::sync::{Mutex, RwLock, mpsc};

/// Bot online status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnlineStatus {
    /// Available.
    Online,
    /// Away.
    Idle,
    /// Do not disturb.
    Dnd,
    /// Hidden from others.
    Invisible,
}

impl OnlineStatus {
    /// All accepted protocol values.
    pub const ALL: &[OnlineStatus] = &[
        OnlineStatus::Online,
        OnlineStatus::Idle,
        OnlineStatus::Dnd,
        OnlineStatus::Invisible,
    ];
}

impl fmt::Display for OnlineStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Online => write!(f, "online"),
            Self::Idle => write!(f, "idle"),
            Self::Dnd => write!(f, "dnd"),
            Self::Invisible => write!(f, "invisible"),
        }
    }
}

impl FromStr for OnlineStatus {
    type Err = InvalidOnlineStatus;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "online" => Ok(Self::Online),
            "idle" => Ok(Self::Idle),
            "dnd" => Ok(Self::Dnd),
            "invisible" => Ok(Self::Invisible),
            other => Err(InvalidOnlineStatus {
                input: other.to_owned(),
            }),
        }
    }
}

/// Bot activity type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivityType {
    /// Playing a game.
    Playing,
    /// Watching an activity.
    Watching,
    /// Listening to an activity.
    Listening,
    /// Competing in an activity.
    Competing,
    /// Custom status text.
    Custom,
}

impl ActivityType {
    /// All accepted protocol values.
    pub const ALL: &[ActivityType] = &[
        ActivityType::Playing,
        ActivityType::Watching,
        ActivityType::Listening,
        ActivityType::Competing,
        ActivityType::Custom,
    ];
}

impl fmt::Display for ActivityType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Playing => write!(f, "playing"),
            Self::Watching => write!(f, "watching"),
            Self::Listening => write!(f, "listening"),
            Self::Competing => write!(f, "competing"),
            Self::Custom => write!(f, "custom"),
        }
    }
}

impl FromStr for ActivityType {
    type Err = InvalidActivityType;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "playing" => Ok(Self::Playing),
            "watching" => Ok(Self::Watching),
            "listening" => Ok(Self::Listening),
            "competing" => Ok(Self::Competing),
            "custom" => Ok(Self::Custom),
            other => Err(InvalidActivityType {
                input: other.to_owned(),
            }),
        }
    }
}

/// An unsupported online status token.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("invalid online_status: {input}; must be one of: online, idle, dnd, invisible")]
pub struct InvalidOnlineStatus {
    input: String,
}
/// An unsupported activity kind token.
#[derive(Debug, Error, PartialEq, Eq)]
#[error(
    "invalid activity_type: {input}; must be one of: playing, watching, listening, competing, custom"
)]
pub struct InvalidActivityType {
    input: String,
}

/// An activity kind paired with unrestricted display text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceActivity {
    kind: ActivityType,
    name: String,
}
impl PresenceActivity {
    /// Pairs an activity kind with its text, including empty text.
    pub fn new(kind: ActivityType, name: String) -> Self {
        Self { kind, name }
    }
    /// Returns the activity kind.
    pub fn kind(&self) -> ActivityType {
        self.kind
    }
    /// Returns the original display text.
    pub fn name(&self) -> &str {
        &self.name
    }
}

/// One complete presence request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceRequest {
    status: OnlineStatus,
    activity: Option<PresenceActivity>,
}
impl PresenceRequest {
    /// Creates a request from domain values.
    pub fn new(status: OnlineStatus, activity: Option<PresenceActivity>) -> Self {
        Self { status, activity }
    }
    /// Returns the requested online status.
    pub fn status(&self) -> OnlineStatus {
        self.status
    }
    /// Returns the requested activity, if any.
    pub fn activity(&self) -> Option<&PresenceActivity> {
        self.activity.as_ref()
    }
}

/// Gateway delivery of a complete domain request.
pub trait PresenceSink: Send + Sync {
    /// Sends the request to this gateway connection.
    fn set_presence(&self, request: &PresenceRequest);
}

/// The last request and its original decision timestamp.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesiredPresence {
    request: PresenceRequest,
    set_at: DateTime<Utc>,
}
impl DesiredPresence {
    /// Returns the original request.
    pub fn request(&self) -> &PresenceRequest {
        &self.request
    }
    /// Returns when the request was stored, unchanged by replay.
    pub fn set_at(&self) -> DateTime<Utc> {
        self.set_at
    }
}

/// A coherent observation of requested state and sink installation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PresenceSnapshot {
    desired: Option<DesiredPresence>,
    sink_installed: bool,
}
impl PresenceSnapshot {
    /// Returns the last request, if one was made during this process.
    pub fn desired(&self) -> Option<&DesiredPresence> {
        self.desired.as_ref()
    }
    /// Reports installation since gateway ready, without probing the connection.
    pub fn sink_installed(&self) -> bool {
        self.sink_installed
    }
}
struct PresenceState {
    sink: Option<Arc<dyn PresenceSink>>,
    desired: Option<DesiredPresence>,
}

/// The shared authority for requested presence and reconnect replay.
///
/// Mutations lock effects before state. Sink callbacks run outside the state
/// lock, allowing snapshots during delivery. The effects lock orders delivery
/// with writes and replay; a sink must not wait for another mutation.
#[derive(Clone)]
pub struct SharedPresence {
    state: Arc<RwLock<PresenceState>>,
    effects: Arc<Mutex<()>>,
}
impl SharedPresence {
    /// Creates an authority with no request or sink.
    pub fn new() -> Self {
        Self {
            state: Arc::new(RwLock::new(PresenceState {
                sink: None,
                desired: None,
            })),
            effects: Arc::new(Mutex::new(())),
        }
    }
    /// Tests whether two handles refer to the same authority.
    pub fn is_same_authority(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state)
    }
    /// Replaces the sink and replays the stored request without restamping it.
    pub async fn install(&self, new_sink: Arc<dyn PresenceSink>) {
        let _effects = self.effects.lock().await;
        let replay = {
            let mut state = self.state.write().await;
            let replay = state.desired.clone();
            state.sink = Some(Arc::clone(&new_sink));
            replay
        };
        if let Some(desired) = replay {
            tracing::info!("replaying desired presence on reconnect");
            new_sink.set_presence(&desired.request);
        }
    }
    /// Stores a request before delivering it to the currently installed sink.
    pub async fn set_presence(&self, request: PresenceRequest) {
        let _effects = self.effects.lock().await;
        let sink = {
            let mut state = self.state.write().await;
            state.desired = Some(DesiredPresence {
                request: request.clone(),
                set_at: Utc::now(),
            });
            state.sink.clone()
        };
        if let Some(sink) = sink {
            sink.set_presence(&request);
        } else {
            tracing::warn!(
                "presence command accepted but shard messenger unavailable (reconnecting); will replay on next ready"
            );
        }
    }
    /// Reads requested state and sink installation under one lock.
    pub async fn snapshot(&self) -> PresenceSnapshot {
        let state = self.state.read().await;
        PresenceSnapshot {
            desired: state.desired.clone(),
            sink_installed: state.sink.is_some(),
        }
    }
}
impl Default for SharedPresence {
    fn default() -> Self {
        Self::new()
    }
}

/// The presence operation available to a transport.
#[derive(Clone, Default)]
pub enum PresenceCapability {
    /// This transport has no gateway presence connection.
    #[default]
    Unavailable,
    /// Immediate requested-state updates and reads.
    Direct(SharedPresence),
    /// Queue acceptance without a synchronous requested-state read.
    Queued(mpsc::Sender<PresenceRequest>),
}
/// How far a presence request has progressed locally.
#[derive(Debug, PartialEq, Eq)]
pub enum PresenceAcknowledgement {
    /// Stored in the shared authority, without asserting Discord delivery.
    Applied,
    /// Accepted into a queue, without asserting application.
    Accepted,
}
/// Failure to submit a presence request.
#[derive(Debug, Error)]
pub enum PresenceSetError {
    /// No direct or queued connection is available.
    #[error("presence updates require the discord gateway command channel")]
    Unavailable,
    /// The queue consumer is gone.
    #[error("failed to send presence command: {source}")]
    QueueClosed {
        /// The failed queue send, retaining the original request.
        #[source]
        source: mpsc::error::SendError<PresenceRequest>,
    },
}
/// Requested state cannot be read through this transport.
#[derive(Debug, Error)]
#[error("presence state is not wired on this transport")]
pub struct PresenceUnavailable;
impl PresenceCapability {
    /// Applies or queues a valid request through the configured connection.
    pub async fn set(
        &self,
        request: PresenceRequest,
    ) -> Result<PresenceAcknowledgement, PresenceSetError> {
        match self {
            Self::Unavailable => Err(PresenceSetError::Unavailable),
            Self::Direct(authority) => {
                authority.set_presence(request).await;
                Ok(PresenceAcknowledgement::Applied)
            }
            Self::Queued(tx) => {
                tx.send(request)
                    .await
                    .map_err(|source| PresenceSetError::QueueClosed { source })?;
                Ok(PresenceAcknowledgement::Accepted)
            }
        }
    }
    /// Reads the direct authority, rejecting queued and unavailable connections.
    pub async fn snapshot(&self) -> Result<PresenceSnapshot, PresenceUnavailable> {
        match self {
            Self::Direct(authority) => Ok(authority.snapshot().await),
            Self::Unavailable | Self::Queued(_) => Err(PresenceUnavailable),
        }
    }
}
/// Gives the gateway and tool transport handles onto one authority.
pub fn wire_shared_presence() -> (SharedPresence, PresenceCapability) {
    let authority = SharedPresence::new();
    (authority.clone(), PresenceCapability::Direct(authority))
}
/// Drains queued requests through the authority's current sink until closure.
pub async fn run_presence_commands(
    presence: SharedPresence,
    mut rx: mpsc::Receiver<PresenceRequest>,
) {
    while let Some(request) = rx.recv().await {
        presence.set_presence(request).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        error::Error as _,
        slice,
        sync::{Mutex as StdMutex, mpsc as std_mpsc},
    };

    #[derive(Default)]
    struct RecordingSink {
        calls: StdMutex<Vec<PresenceRequest>>,
    }
    impl PresenceSink for RecordingSink {
        fn set_presence(&self, request: &PresenceRequest) {
            self.calls.lock().unwrap().push(request.clone());
        }
    }
    fn request(status: OnlineStatus, name: &str) -> PresenceRequest {
        PresenceRequest::new(
            status,
            Some(PresenceActivity::new(
                ActivityType::Playing,
                name.to_owned(),
            )),
        )
    }

    #[tokio::test]
    async fn wired_presence_handles_share_one_authority() {
        let (gateway, capability) = wire_shared_presence();
        let PresenceCapability::Direct(server) = &capability else {
            panic!("direct production authority");
        };
        assert!(gateway.is_same_authority(server));
        assert!(!gateway.is_same_authority(&SharedPresence::new()));
        let expected = request(OnlineStatus::Dnd, "shared");
        assert_eq!(
            capability.set(expected.clone()).await.unwrap(),
            PresenceAcknowledgement::Applied
        );
        assert_eq!(
            gateway.snapshot().await.desired().unwrap().request(),
            &expected
        );
        let sink = Arc::new(RecordingSink::default());
        gateway.install(sink.clone()).await;
        assert_eq!(*sink.calls.lock().unwrap(), [expected]);
    }

    #[tokio::test]
    async fn presence_queue_drain_last_write_wins() {
        let authority = SharedPresence::new();
        let sink = Arc::new(RecordingSink::default());
        authority.install(sink.clone()).await;
        let (tx, rx) = mpsc::channel(3);
        let handle = tokio::spawn(run_presence_commands(authority.clone(), rx));
        let expected = [
            request(OnlineStatus::Online, "first"),
            request(OnlineStatus::Idle, "second"),
            request(OnlineStatus::Dnd, "third"),
        ];
        for value in &expected {
            tx.send(value.clone()).await.unwrap();
        }
        drop(tx);
        handle.await.unwrap();
        assert_eq!(*sink.calls.lock().unwrap(), expected);
        assert_eq!(
            authority.snapshot().await.desired().unwrap().request(),
            &request(OnlineStatus::Dnd, "third")
        );
    }

    #[tokio::test]
    async fn presence_reconnect_replays_to_new_sink_not_old() {
        let authority = SharedPresence::new();
        let sink_a = Arc::new(RecordingSink::default());
        authority.install(sink_a.clone()).await;
        let first = request(OnlineStatus::Idle, "first");
        authority.set_presence(first.clone()).await;
        let original = authority.snapshot().await;
        let sink_b = Arc::new(RecordingSink::default());
        authority.install(sink_b.clone()).await;
        assert_eq!(authority.snapshot().await.desired(), original.desired());
        assert_eq!(
            sink_b.calls.lock().unwrap().as_slice(),
            slice::from_ref(&first)
        );
        let (tx, rx) = mpsc::channel(1);
        let later = request(OnlineStatus::Dnd, "after reconnect");
        tx.send(later.clone()).await.unwrap();
        drop(tx);
        run_presence_commands(authority.clone(), rx).await;
        assert_eq!(
            sink_a.calls.lock().unwrap().as_slice(),
            slice::from_ref(&first)
        );
        assert_eq!(*sink_b.calls.lock().unwrap(), [first, later.clone()]);
        assert_eq!(
            authority.snapshot().await.desired().unwrap().request(),
            &later
        );
    }

    #[tokio::test]
    async fn direct_presence_is_immediate_while_queue_is_unconsumed() {
        let authority = SharedPresence::new();
        let direct = PresenceCapability::Direct(authority.clone());
        let (tx, _blocked_receiver) = mpsc::channel(1);
        let queued = PresenceCapability::Queued(tx);
        assert_eq!(
            queued
                .set(request(OnlineStatus::Idle, "queued"))
                .await
                .unwrap(),
            PresenceAcknowledgement::Accepted
        );
        let value = request(OnlineStatus::Dnd, "direct");
        assert_eq!(
            direct.set(value.clone()).await.unwrap(),
            PresenceAcknowledgement::Applied
        );
        assert_eq!(
            direct
                .snapshot()
                .await
                .unwrap()
                .desired()
                .unwrap()
                .request(),
            &value
        );
        assert!(queued.snapshot().await.is_err());
    }

    #[tokio::test]
    async fn closed_presence_queue_preserves_error_source_and_request() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let value = request(OnlineStatus::Online, "unsent");
        let error = PresenceCapability::Queued(tx)
            .set(value.clone())
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            "failed to send presence command: channel closed"
        );
        assert_eq!(error.source().unwrap().to_string(), "channel closed");
        let PresenceSetError::QueueClosed { source } = error else {
            panic!("queue error");
        };
        assert_eq!(source.0, value);
    }

    struct BlockingSink {
        calls: StdMutex<Vec<PresenceRequest>>,
        entered: StdMutex<Option<tokio::sync::oneshot::Sender<()>>>,
        release: StdMutex<std_mpsc::Receiver<()>>,
    }
    impl PresenceSink for BlockingSink {
        fn set_presence(&self, request: &PresenceRequest) {
            self.calls.lock().unwrap().push(request.clone());
            if let Some(entered) = self.entered.lock().unwrap().take() {
                entered.send(()).unwrap();
                self.release.lock().unwrap().recv().unwrap();
            }
        }
    }
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn presence_snapshots_remain_readable_during_delivery() {
        let authority = SharedPresence::new();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let sink = Arc::new(BlockingSink {
            calls: StdMutex::new(Vec::new()),
            entered: StdMutex::new(Some(entered_tx)),
            release: StdMutex::new(release_rx),
        });
        authority.install(sink.clone()).await;
        let first = request(OnlineStatus::Idle, "first");
        let writer = {
            let authority = authority.clone();
            let first = first.clone();
            tokio::spawn(async move { authority.set_presence(first).await })
        };
        entered_rx.await.unwrap();
        let snapshot =
            tokio::time::timeout(std::time::Duration::from_secs(2), authority.snapshot()).await;
        release_tx.send(()).unwrap();
        assert_eq!(snapshot.unwrap().desired().unwrap().request(), &first);
        writer.await.unwrap();
        let second = request(OnlineStatus::Dnd, "second");
        authority.set_presence(second.clone()).await;
        assert_eq!(*sink.calls.lock().unwrap(), [first, second]);
    }
}
