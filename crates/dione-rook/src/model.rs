use super::observation::{ActorKey, ConversationKey, EventKey, RetainedObservation};
use chrono::{DateTime, Utc};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::{collections::BTreeSet, fmt, time::Duration};
use thiserror::Error;
use uuid::Uuid;

macro_rules! identifier {
    ($name:ident) => {
        #[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
        #[doc = concat!("Stable validated ", stringify!($name), " identity.")]
        pub struct $name(String);
        impl $name {
            /// Generate a fresh identity. Reuse the configured value across restarts.
            pub fn new() -> Self {
                Self(Uuid::new_v4().to_string())
            }
            /// Borrow the canonical UUID spelling.
            pub fn as_str(&self) -> &str {
                &self.0
            }
            fn valid(&self) -> bool {
                Uuid::parse_str(&self.0).is_ok()
            }
        }
        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
                let value = String::deserialize(decoder)?;
                let parsed = Uuid::parse_str(&value).map_err(serde::de::Error::custom)?;
                if parsed.to_string() != value {
                    return Err(serde::de::Error::custom(
                        "identifier must use canonical UUID spelling",
                    ));
                }
                Ok(Self(value))
            }
        }
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }
    };
}
identifier!(RecipientId);
identifier!(SubscriptionId);
identifier!(RequestId);
identifier!(AttemptId);

macro_rules! generation {
    ($name:ident) => {
        #[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
        #[doc = concat!("Monotonic ", stringify!($name), " counter, distinct from other generations.")]
        pub struct $name(u64);
        impl $name {
            /// Construct the counter supplied by trusted configuration or retained state.
            pub fn new(value: u64) -> Self { Self(value) }
            /// Advance once, refusing overflow rather than reusing a generation.
            pub fn checked_next(self) -> Result<Self, OwnerError> { increment(self.0).map(Self) }
        }
    };
}
generation!(AuthorizationRevision);
generation!(ClaimGeneration);
generation!(RecoveryGeneration);
generation!(BindingGeneration);

#[derive(Clone)]
/// Recipient identity authenticated by the trusted embedding application.
pub struct AuthenticatedRecipient(RecipientId);
impl AuthenticatedRecipient {
    /// Attest to authentication already performed by the application.
    /// Never call this directly from untrusted message fields.
    pub fn from_authenticated_identity(id: RecipientId) -> Self {
        Self(id)
    }
    pub(super) fn id(&self) -> &RecipientId {
        &self.0
    }
}
/// Operator authority authenticated by the trusted embedding application.
pub struct AuthorizedOperator {
    _private: (),
}
impl AuthorizedOperator {
    /// Attest to a completed operator authorization check, not a message request.
    pub fn from_authenticated_operator() -> Self {
        Self { _private: () }
    }
}

#[derive(Clone)]
/// Secret claim capability. Debug output is redacted; serialization discloses it.
pub struct GrantToken(SecretString);
impl GrantToken {
    pub(super) fn new() -> Self {
        Self(SecretString::new(format!(
            "{}{}",
            Uuid::new_v4().simple(),
            Uuid::new_v4().simple()
        )))
    }
    fn valid(&self) -> bool {
        self.0.expose_secret().len() == 64
            && self
                .0
                .expose_secret()
                .bytes()
                .all(|b| b.is_ascii_hexdigit())
    }
}
impl PartialEq for GrantToken {
    fn eq(&self, other: &Self) -> bool {
        self.0.expose_secret() == other.0.expose_secret()
    }
}
impl Eq for GrantToken {}
impl Serialize for GrantToken {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.0.expose_secret())
    }
}
impl<'de> Deserialize<'de> for GrantToken {
    fn deserialize<D: Deserializer<'de>>(decoder: D) -> Result<Self, D::Error> {
        let token = Self(SecretString::new(String::deserialize(decoder)?));
        if !token.valid() {
            return Err(serde::de::Error::custom("invalid claim token"));
        }
        Ok(token)
    }
}
impl fmt::Debug for GrantToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("GrantToken([redacted])")
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Requested capacity and timing policy, checked by service open and snapshot load.
pub struct Limits {
    /// Maximum retained source occurrences for this recipient.
    pub events: usize,
    /// Maximum total event-to-subscription links.
    pub links: usize,
    /// Maximum combined claim, replay, and recovery history records.
    pub history: usize,
    /// Maximum encoded snapshot size, also enforced when loading it.
    pub snapshot_bytes: usize,
    /// Time allowed to claim after the first accepted callback.
    pub claim_wait_seconds: u32,
    /// Lifetime of a granted claim before recovery is required.
    pub claim_seconds: u32,
    /// Maximum automatic recovery rounds within one replay cycle.
    pub recovery_rounds: u8,
    /// Maximum callback attempts for each binding within one replay cycle.
    pub attempts_per_binding: u8,
    /// Maximum retries when current access cannot be verified.
    pub access_retries: u8,
    /// Delay between retry attempts, in seconds.
    pub retry_seconds: u32,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            events: 1024,
            links: 8192,
            history: 16384,
            snapshot_bytes: 16 * 1024 * 1024,
            claim_wait_seconds: 300,
            claim_seconds: 600,
            recovery_rounds: 2,
            attempts_per_binding: 6,
            access_retries: 6,
            retry_seconds: 5,
        }
    }
}
impl Limits {
    pub(super) fn valid(&self) -> bool {
        (1..=4096).contains(&self.events)
            && (1..=32768).contains(&self.links)
            && (1..=65536).contains(&self.history)
            && (1024..=64 * 1024 * 1024).contains(&self.snapshot_bytes)
            && (1..=3600).contains(&self.claim_wait_seconds)
            && (1..=3600).contains(&self.claim_seconds)
            && self.recovery_rounds <= 8
            && (1..=16).contains(&self.attempts_per_binding)
            && (1..=16).contains(&self.access_retries)
            && (1..=300).contains(&self.retry_seconds)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(
    tag = "mode",
    content = "actors",
    rename_all = "snake_case",
    deny_unknown_fields
)]
/// Sender admission policy within explicitly configured rooms.
pub enum SenderFilter {
    /// Admit every sender in an allowed room, without requiring a mention.
    EveryoneInRoom,
    /// Admit only the listed native or represented sender identities.
    Only(BTreeSet<ActorKey>),
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
/// Callback binding readiness, independent of subscription enablement.
pub enum BindingState {
    /// The trusted transport boundary has verified this binding.
    Verified {
        /// Version of the verified binding, used to fence obsolete completions.
        generation: BindingGeneration,
        /// Deadline after which verification must be renewed before delivery.
        refresh_before: DateTime<Utc>,
    },
    /// Delivery waits for renewed binding verification.
    NeedsRefresh,
    /// The configured binding cannot currently deliver callbacks.
    Unavailable,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Trusted recipient routing configuration, never derived from message instructions.
pub struct Subscription {
    /// Stable subscription identity retained across assistant runs.
    pub id: SubscriptionId,
    /// Operator-controlled enablement; silence and tool errors do not change it.
    pub enabled: bool,
    /// Explicit set of conversations eligible for delivery.
    pub rooms: BTreeSet<ConversationKey>,
    /// Sender eligibility applied within the allowed conversations.
    pub senders: SenderFilter,
    /// Recovery selection priority; lower values are selected first.
    pub priority: u32,
    /// Current transport verification state.
    pub binding: BindingState,
}
impl Subscription {
    pub(super) fn admits(&self, observation: &RetainedObservation) -> bool {
        self.enabled
            && self.rooms.contains(observation.conversation())
            && match &self.senders {
                SenderFilter::EveryoneInRoom => true,
                SenderFilter::Only(actors) => {
                    actors.contains(observation.sender())
                        || observation
                            .represented_sender()
                            .is_some_and(|actor| actors.contains(actor))
                }
            }
    }
    pub(super) fn valid(&self) -> bool {
        self.id.valid()
            && !self.rooms.is_empty()
            && self.rooms.iter().all(|room| room.validate().is_ok())
            && match &self.senders {
                SenderFilter::EveryoneInRoom => true,
                SenderFilter::Only(actors) => {
                    !actors.is_empty() && actors.iter().all(|actor| actor.validate().is_ok())
                }
            }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Explicit completion of shared recipient handling for an occurrence.
pub enum Settlement {
    /// The claimant completed handling the occurrence.
    Handled,
    /// The claimant deliberately chose not to reply.
    NoReply,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Reason automatic handling stopped and requires inspection or authorized replay.
pub enum ParkReason {
    /// An accepted callback did not lead to a claim within its deadline.
    ClaimNotStarted,
    /// A granted claim expired without settlement.
    ClaimExpired,
    /// Callback delivery consumed its allowed attempts.
    DeliveryAttemptsExhausted,
    /// Current authorization no longer permits handling.
    AccessDenied,
    /// Current authorization could not be verified within the retry budget.
    AccessUnavailable,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
/// Callback failure retained for status and bounded recovery.
pub enum DeliveryFailure {
    /// The callback did not complete before its timeout.
    Timeout,
    /// The callback destination rejected delivery.
    Rejected,
    /// The callback destination was unavailable.
    Unavailable,
    /// Restart interrupted a reserved callback whose outcome is unknown.
    Restarted,
    /// The delivery worker failed before reporting an outcome.
    WorkerFailed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Durable, expiring authority to handle one retained occurrence.
pub struct ClaimGrant {
    /// Secret capability required for settlement; never include it in logs or status.
    pub token: GrantToken,
    /// Claim generation used to reject superseded grants.
    pub generation: ClaimGeneration,
    /// Authorization revision under which the grant was issued.
    pub authorization_revision: AuthorizationRevision,
    /// Absolute expiry; retries of the same request do not extend it.
    pub expires_at: DateTime<Utc>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum Handling {
    Unclaimed,
    Claimed {
        request: RequestId,
        grant: ClaimGrant,
    },
    Settled {
        kind: Settlement,
        request: RequestId,
        generation: ClaimGeneration,
    },
    Parked {
        reason: ParkReason,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum WakeState {
    Pending {
        due: DateTime<Utc>,
    },
    Dispatching {
        attempt: AttemptId,
        generation: BindingGeneration,
        authorization_revision: AuthorizationRevision,
    },
    Accepted,
    Retry {
        due: DateTime<Utc>,
        failure: DeliveryFailure,
    },
    Cancelled,
    Exhausted {
        failure: DeliveryFailure,
    },
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Link {
    pub(super) subscription: SubscriptionId,
    pub(super) attempts: u8,
    pub(super) last_failure: Option<DeliveryFailure>,
    pub(super) wake: WakeState,
}
impl Link {
    pub(super) fn retry_after_interruption(
        &mut self,
        failure: DeliveryFailure,
        due: DateTime<Utc>,
        budget: u8,
    ) {
        self.last_failure = Some(failure.clone());
        self.wake = if self.attempts >= budget {
            WakeState::Exhausted { failure }
        } else {
            WakeState::Retry { due, failure }
        };
    }
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecoveryHistory {
    pub(super) cycle: u64,
    pub(super) round: u8,
    pub(super) reason: ParkReason,
    pub(super) attempts: Vec<(SubscriptionId, u8)>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct EventRecord {
    pub(super) observation: RetainedObservation,
    pub(super) handling: Handling,
    pub(super) links: Vec<Link>,
    pub(super) claim_generation: ClaimGeneration,
    pub(super) cycle: u64,
    pub(super) round: u8,
    pub(super) recovery_generation: RecoveryGeneration,
    pub(super) accepted_deadline: Option<DateTime<Utc>>,
    pub(super) recovery_binding: Option<SubscriptionId>,
    pub(super) access_failures: u8,
    pub(super) access_due: Option<DateTime<Utc>>,
    pub(super) history: Vec<RecoveryHistory>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ClaimHistory {
    pub(super) id: RequestId,
    pub(super) event: EventKey,
    pub(super) grant: Option<ClaimGrant>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReplayHistory {
    pub(super) id: RequestId,
    pub(super) event: EventKey,
    pub(super) cycle: u64,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RecipientState {
    pub(super) version: u8,
    pub(super) recipient: RecipientId,
    pub(super) revision: AuthorizationRevision,
    pub(super) high_water: DateTime<Utc>,
    pub(super) limits: Limits,
    pub(super) subscriptions: Vec<Subscription>,
    pub(super) events: Vec<EventRecord>,
    pub(super) claims: Vec<ClaimHistory>,
    pub(super) replays: Vec<ReplayHistory>,
    pub(super) publications: Vec<super::publication::PublicationRecord>,
}
impl RecipientState {
    pub(super) fn validate(&self) -> Result<(), OwnerError> {
        if self.version != 2
            || !self.recipient.valid()
            || !self.limits.valid()
            || self.subscriptions.iter().any(|s| !s.valid())
            || self
                .subscriptions
                .iter()
                .map(|s| &s.id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.subscriptions.len()
            || self
                .events
                .iter()
                .map(|e| e.observation.key())
                .collect::<BTreeSet<_>>()
                .len()
                != self.events.len()
            || self
                .claims
                .iter()
                .map(|c| &c.id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.claims.len()
            || self
                .replays
                .iter()
                .map(|r| &r.id)
                .collect::<BTreeSet<_>>()
                .len()
                != self.replays.len()
        {
            return Err(OwnerError::invalid());
        }
        if self.events.len() > self.limits.events
            || self.events.iter().map(|e| e.links.len()).sum::<usize>() > self.limits.links
            || self.claims.len()
                + self.replays.len()
                + self.events.iter().map(|e| e.history.len()).sum::<usize>()
                + self
                    .publications
                    .iter()
                    .map(|record| record.aliases.len())
                    .sum::<usize>()
                > self.limits.history
        {
            return Err(OwnerError::new(OwnerErrorKind::Capacity));
        }
        let mut publication_events = BTreeSet::new();
        let mut publication_aliases = BTreeSet::new();
        for record in &self.publications {
            record.validate().map_err(OwnerError::from)?;
            if !publication_events.insert(record.request.event())
                || record
                    .aliases
                    .iter()
                    .any(|alias| !publication_aliases.insert(alias))
                || !self
                    .events
                    .iter()
                    .any(|event| event.observation.key() == record.request.event())
            {
                return Err(OwnerError::invalid());
            }
        }
        for event in &self.events {
            event
                .observation
                .validate()
                .map_err(|_| OwnerError::invalid())?;
            if event.round > self.limits.recovery_rounds
                || event
                    .links
                    .iter()
                    .map(|l| &l.subscription)
                    .collect::<BTreeSet<_>>()
                    .len()
                    != event.links.len()
                || event.links.iter().any(|l| {
                    !l.subscription.valid() || l.attempts > self.limits.attempts_per_binding
                })
            {
                return Err(OwnerError::invalid());
            }
            if let Handling::Claimed { grant, .. } = &event.handling
                && (!grant.token.valid()
                    || grant.generation != event.claim_generation
                    || grant.authorization_revision != self.revision)
            {
                return Err(OwnerError::invalid());
            }
            match &event.handling {
                Handling::Claimed { request, grant }
                    if !request.valid()
                        || !self.claims.iter().any(|claim| {
                            claim.id == *request
                                && claim.event == *event.observation.key()
                                && claim.grant.as_ref() == Some(grant)
                        }) =>
                {
                    return Err(OwnerError::invalid());
                }
                Handling::Settled {
                    request,
                    generation,
                    ..
                } if !request.valid()
                    || !self.claims.iter().any(|claim| {
                        claim.event == *event.observation.key()
                            && claim
                                .grant
                                .as_ref()
                                .is_some_and(|grant| grant.generation == *generation)
                    }) =>
                {
                    return Err(OwnerError::invalid());
                }
                _ => {}
            }
            if event.access_failures > self.limits.access_retries
                || event.recovery_binding.as_ref().is_some_and(|id| !event.links.iter().any(|link| &link.subscription == id))
                || event.links.iter().any(|link| matches!(&link.wake, WakeState::Dispatching { attempt, authorization_revision, .. } if !attempt.valid() || *authorization_revision > self.revision || link.attempts == 0))
                || event.history.iter().any(|history| history.cycle > event.cycle || history.round > self.limits.recovery_rounds || history.attempts.iter().any(|(id, attempts)| !id.valid() || *attempts > self.limits.attempts_per_binding))
            { return Err(OwnerError::invalid()); }
        }
        for claim in &self.claims {
            if !claim.id.valid()
                || claim
                    .grant
                    .as_ref()
                    .is_some_and(|grant| !grant.token.valid())
                || !self
                    .events
                    .iter()
                    .any(|e| e.observation.key() == &claim.event)
            {
                return Err(OwnerError::invalid());
            }
        }
        for replay in &self.replays {
            if !replay.id.valid()
                || !self
                    .events
                    .iter()
                    .any(|e| e.observation.key() == &replay.event)
            {
                return Err(OwnerError::invalid());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Error)]
#[error("rook owner operation failed: {kind}")]
/// Typed owner refusal, including storage errors whose visible commit may be uncertain.
pub struct OwnerError {
    /// Classification and underlying cause of the failed operation.
    #[source]
    pub kind: OwnerErrorKind,
}
impl OwnerError {
    pub(super) fn new(kind: OwnerErrorKind) -> Self {
        Self { kind }
    }
    pub(super) fn worker_join(error: tokio::task::JoinError) -> Self {
        Self::new(OwnerErrorKind::WorkerJoin(error))
    }
    pub(super) fn invalid() -> Self {
        Self::new(OwnerErrorKind::InvalidState)
    }
}
#[derive(Debug, Error)]
/// Actionable owner failures; none implicitly disable a subscription.
pub enum OwnerErrorKind {
    /// The publication request is unsupported, invalid, or conflicts with a retained slot.
    #[error("publication request failed")]
    Publication(#[source] super::publication::PublicationError),
    /// Storage refused the operation; inspect its kind before treating it as rolled back.
    #[error("private storage failed")]
    Storage(#[source] super::private_dir::StorageError),
    /// Retained or supplied state violates the supported schema or invariants.
    #[error("invalid or unsupported retained state")]
    InvalidState,
    /// Retained JSON failed decoding; the source retains its typed field path.
    #[error("retained state decoding failed")]
    Decode(#[source] serde_path_to_error::Error<serde_json::Error>),
    /// Retained JSON contained trailing input.
    #[error("retained state trailing input rejected")]
    TrailingInput(#[source] serde_json::Error),
    /// Retained state could not be encoded for its size check.
    #[error("retained state encoding failed")]
    Encoding(#[source] serde_json::Error),
    /// A configured retention or snapshot-size bound would be exceeded.
    #[error("retained state capacity reached")]
    Capacity,
    /// The authenticated identity or current policy does not permit the operation.
    #[error("access denied")]
    Denied,
    /// The trusted authorizer could not establish current access.
    #[error("access verification unavailable")]
    AccessUnavailable,
    /// An existing occurrence identity was supplied with conflicting immutable evidence.
    #[error("source evidence conflicts with retained occurrence")]
    ConflictingRevision,
    /// A retained request identity was reused for a different operation's parameters.
    #[error("request identity was reused with different parameters")]
    RequestConflict,
    /// The requested occurrence is not retained by this owner.
    #[error("event is unknown")]
    UnknownEvent,
    /// The supplied claim no longer grants authority for this occurrence.
    #[error("claim is expired, invalidated, or belongs to another event")]
    InvalidClaim,
    /// The owner task cannot receive or complete the request.
    #[error("owner task is unavailable")]
    Unavailable,
    /// A supervised worker failed; its typed join cause stays private.
    #[error("owner worker failed")]
    WorkerJoin(#[source] tokio::task::JoinError),
    /// A checked generation increment or deadline calculation would overflow.
    #[error("monotonic generation or timestamp exhausted")]
    Exhausted,
}
impl From<super::private_dir::StorageError> for OwnerError {
    fn from(value: super::private_dir::StorageError) -> Self {
        Self::new(OwnerErrorKind::Storage(value))
    }
}
#[derive(Clone, Debug, Eq, PartialEq)]
/// Result of acquiring shared handling authority for one occurrence.
pub enum ClaimOutcome {
    /// A durable grant, including recovery of an identical still-valid request.
    Granted(ClaimGrant),
    /// Another claim remains live until the recorded deadline.
    ///
    /// The expiry is informational and does not grant permission to handle the event.
    Busy {
        /// Absolute deadline of the live claim.
        expires_at: DateTime<Utc>,
    },
    /// Handling already has an explicit terminal settlement.
    Settled(Settlement),
    /// The prior claim cannot be reused; handling requires recovery.
    RecoveryRequired,
}
#[derive(Clone, Debug, Eq, PartialEq)]
/// Admission result after evaluating the recipient's configured subscriptions.
pub enum AdmissionOutcome {
    /// A new occurrence was retained with this many matching subscription links.
    Inserted(usize),
    /// The same occurrence and immutable evidence were already retained.
    Duplicate,
    /// No enabled subscription admits the supplied observation.
    Excluded,
}
#[derive(Clone, Debug, Eq, PartialEq)]
/// Result of an operator-authorized replay request.
pub enum ReplayOutcome {
    /// Recovery is scheduled in this cycle, reused for an identical replay request.
    Scheduled {
        /// Durable replay cycle identity returned again for the same request.
        cycle: u64,
    },
    /// Handling is already settled; replay does not reopen it.
    Settled(Settlement),
}
#[derive(Clone, Debug, Eq, PartialEq)]
/// Current access decision supplied by the trusted embedding application's authorizer.
///
/// Derive this decision from authoritative identity and policy, never message instructions.
pub enum AccessDecision {
    /// Current policy permits the recipient to handle this observation.
    Allowed,
    /// Current policy forbids handling.
    Denied,
    /// Access could not be verified; this is not permission to proceed.
    Unavailable,
}
#[derive(Clone, Debug, Eq, PartialEq)]
/// Transport outcome for one callback attempt, separate from event settlement.
pub enum CallbackOutcome {
    /// The destination accepted the callback; handling is still outstanding.
    Accepted,
    /// The callback did not produce a confirmed acceptance.
    Failed(DeliveryFailure),
}
#[derive(Clone, Debug)]
/// Callback attempt whose dispatch reservation has been committed durably.
pub struct WakeRequest {
    /// Authenticated recipient whose shared handling state owns the occurrence.
    pub recipient: RecipientId,
    /// Stable source occurrence identity, preserved across retries and replay.
    pub event: EventKey,
    /// Subscription selected for this callback attempt.
    pub subscription: SubscriptionId,
    /// Verified binding version captured when dispatch was reserved.
    pub binding_generation: BindingGeneration,
    /// Unique identity of this reserved callback attempt.
    pub attempt: AttemptId,
    /// Recovery generation used to reject obsolete callback completions.
    pub recovery_generation: RecoveryGeneration,
    /// Retained source evidence for the callback, treated as untrusted conversation.
    pub observation: RetainedObservation,
}
pub(super) fn deadline(now: DateTime<Utc>, seconds: u32) -> Result<DateTime<Utc>, OwnerError> {
    now.checked_add_signed(chrono::Duration::seconds(i64::from(seconds)))
        .ok_or_else(|| OwnerError::new(OwnerErrorKind::Exhausted))
}
pub(super) fn increment(value: u64) -> Result<u64, OwnerError> {
    value
        .checked_add(1)
        .ok_or_else(|| OwnerError::new(OwnerErrorKind::Exhausted))
}
pub(super) const NETWORK_TIMEOUT: Duration = Duration::from_secs(10);

impl From<super::publication::PublicationError> for OwnerError {
    fn from(error: super::publication::PublicationError) -> Self {
        Self::new(OwnerErrorKind::Publication(error))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RecipientStateV1 {
    version: u8,
    recipient: RecipientId,
    revision: AuthorizationRevision,
    high_water: DateTime<Utc>,
    limits: Limits,
    subscriptions: Vec<Subscription>,
    events: Vec<EventRecord>,
    claims: Vec<ClaimHistory>,
    replays: Vec<ReplayHistory>,
}
impl RecipientState {
    pub(super) fn decode(bytes: &[u8]) -> Result<(Self, bool), OwnerError> {
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let header: serde_json::Value = serde_path_to_error::deserialize(&mut decoder)
            .map_err(|source| OwnerError::new(OwnerErrorKind::Decode(source)))?;
        decoder
            .end()
            .map_err(|source| OwnerError::new(OwnerErrorKind::TrailingInput(source)))?;
        let mut decoder = serde_json::Deserializer::from_slice(bytes);
        let (state, migrated) = match header.get("version").and_then(serde_json::Value::as_u64) {
            Some(1) => {
                let old: RecipientStateV1 = serde_path_to_error::deserialize(&mut decoder)
                    .map_err(|source| OwnerError::new(OwnerErrorKind::Decode(source)))?;
                if old.version != 1 {
                    return Err(OwnerError::invalid());
                }
                (
                    Self {
                        version: 2,
                        recipient: old.recipient,
                        revision: old.revision,
                        high_water: old.high_water,
                        limits: old.limits,
                        subscriptions: old.subscriptions,
                        events: old.events,
                        claims: old.claims,
                        replays: old.replays,
                        publications: Vec::new(),
                    },
                    true,
                )
            }
            Some(2) => (
                serde_path_to_error::deserialize(&mut decoder)
                    .map_err(|source| OwnerError::new(OwnerErrorKind::Decode(source)))?,
                false,
            ),
            _ => return Err(OwnerError::invalid()),
        };
        decoder
            .end()
            .map_err(|source| OwnerError::new(OwnerErrorKind::TrailingInput(source)))?;
        state.validate()?;
        if serde_json::to_vec(&state)
            .map_err(|source| OwnerError::new(OwnerErrorKind::Encoding(source)))?
            .len()
            .saturating_add(state.publication_headroom())
            > state.limits.snapshot_bytes
        {
            return Err(OwnerError::new(OwnerErrorKind::Capacity));
        }
        Ok((state, migrated))
    }

    pub(super) fn publication_headroom(&self) -> usize {
        self.publications
            .iter()
            .map(super::publication::PublicationRecord::headroom)
            .sum()
    }
}

#[cfg(all(test, unix))]
impl RecipientState {
    pub(super) fn v1_fixture_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(&RecipientStateV1 {
            version: 1,
            recipient: self.recipient.clone(),
            revision: self.revision,
            high_water: self.high_water,
            limits: self.limits.clone(),
            subscriptions: self.subscriptions.clone(),
            events: self.events.clone(),
            claims: self.claims.clone(),
            replays: self.replays.clone(),
        })
        .unwrap()
    }
}
