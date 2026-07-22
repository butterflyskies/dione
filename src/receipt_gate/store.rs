use std::collections::{HashMap, hash_map::Entry};
use std::fmt;
use std::str::FromStr;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serenity::model::id::{ChannelId, MessageId, UserId};
use sha2::{Digest as _, Sha256};
use uuid::Uuid;

const DEFAULT_TTL: Duration = Duration::from_secs(3600);
const MAX_AUTHORITY_ID_ATTEMPTS: usize = 8;
const DEFAULT_MAX_ACTIVE_RECEIPTS: usize = 4_096;
const DEFAULT_MAX_ACTIVE_INVOCATIONS: usize = 65_536;

/// Opaque, cryptographically random identifier for an action authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct AuthorityId(Uuid);

impl AuthorityId {
    /// Returns the underlying UUID value.
    pub(crate) fn as_uuid(&self) -> Uuid {
        self.0
    }
}

impl FromStr for AuthorityId {
    type Err = AuthorityIdParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(AuthorityIdParseError)
    }
}

impl fmt::Display for AuthorityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// Error returned when untrusted text is not a canonical UUID authority ID.
#[derive(Debug, thiserror::Error)]
#[error("invalid authority identifier")]
pub(crate) struct AuthorityIdParseError(#[source] uuid::Error);

/// Opaque invocation identifier created by trusted harness code.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct InvocationId(Uuid);

impl InvocationId {
    /// Returns the underlying UUID value.
    pub(crate) fn as_uuid(&self) -> Uuid {
        self.0
    }
}

/// Evidence that a source event was received by an authenticated ingress adapter.
///
/// The fields and constructor are private so model-originated data cannot be
/// converted into trusted evidence by ordinary receipt-gate callers. The future
/// Dione gateway adapter will own construction when the fixture is runtime-wired.
#[derive(Debug, Clone)]
pub(crate) struct AuthenticatedSourceEvent {
    event_id: MessageId,
    principal: UserId,
    channel: ChannelId,
    content: String,
    verifier: VerifierKind,
    assurance: SourceAssurance,
    content_sha256: [u8; 32],
}

impl AuthenticatedSourceEvent {
    fn from_dione_gateway(
        event_id: MessageId,
        principal: UserId,
        channel: ChannelId,
        content: String,
    ) -> Self {
        let content_sha256 = Sha256::digest(content.as_bytes()).into();
        Self {
            event_id,
            principal,
            channel,
            content,
            verifier: VerifierKind::DioneGateway,
            assurance: SourceAssurance::AuthenticatedGatewayEvent,
            content_sha256,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VerifierKind {
    DioneGateway,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SourceAssurance {
    AuthenticatedGatewayEvent,
}

/// Immutable evidence that policy authorized a principal to react in a channel.
///
/// Construction is deliberately private. A future policy adapter must produce
/// this evidence after evaluating the configured role policy.
#[derive(Debug, Clone)]
pub(crate) struct PolicyAuthorization {
    principal: UserId,
    channel: ChannelId,
    snapshot: PolicySnapshot,
}

impl PolicyAuthorization {
    fn allow_react(
        principal: UserId,
        channel: ChannelId,
        policy_id: impl Into<String>,
        policy_version: u64,
    ) -> Self {
        Self {
            principal,
            channel,
            snapshot: PolicySnapshot {
                policy_id: policy_id.into(),
                version: policy_version,
            },
        }
    }
}

#[derive(Debug, Clone)]
struct PolicySnapshot {
    policy_id: String,
    version: u64,
}

/// Complete scope for the only action supported in v1: a Discord reaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ReactScope {
    /// Channel containing the target message.
    pub(crate) channel: ChannelId,
    /// Message that will receive the reaction.
    pub(crate) target_msg: MessageId,
    /// Exact Unicode or Discord custom-emoji token to apply.
    pub(crate) emoji: String,
}

#[derive(Debug, Clone)]
struct ActionReceipt {
    source_event_id: MessageId,
    principal: UserId,
    scope: ReactScope,
    expires: Instant,
    verifier: VerifierKind,
    source_assurance: SourceAssurance,
    source_content_sha256: [u8; 32],
    policy_snapshot: PolicySnapshot,
}

/// Receipt-gate decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateVerdict {
    /// Every authority, scope, expiry, and deduplication predicate passed.
    Allow,
    /// At least one predicate failed; no effect may be dispatched.
    Deny,
}

/// Typed reason for a receipt-gate decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GateReason {
    /// Every gate predicate passed.
    PredicatesPassed,
    /// The authority identifier is unknown.
    AuthorityNotFound,
    /// The requested channel differs from the authorized channel.
    ChannelMismatch,
    /// The requested target differs from the authorized target.
    TargetMismatch,
    /// The requested emoji differs from the authorized emoji.
    EmojiMismatch,
    /// The authority has expired.
    AuthorityExpired,
    /// The harness invocation was already claimed.
    DuplicateInvocation,
    /// The bounded invocation-claim store is at capacity.
    InvocationCapacityExceeded,
    /// Internal gate state was unavailable; the gate failed closed.
    StoreUnavailable,
}

/// Auditable result returned by the receipt gate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct GateResult {
    /// Allow or deny decision.
    pub(crate) verdict: GateVerdict,
    /// Typed decision reason.
    pub(crate) reason: GateReason,
    /// Authority evaluated by the gate.
    pub(crate) authority_id: AuthorityId,
    /// Harness invocation atomically claimed by an allow decision.
    pub(crate) invocation_id: InvocationId,
}

/// A model-proposed reaction paired with harness-controlled identifiers.
#[derive(Debug, Clone)]
pub(crate) struct VerifyRequest {
    authority_id: AuthorityId,
    scope: ReactScope,
    invocation_id: InvocationId,
}

impl VerifyRequest {
    /// Builds a verification request from opaque IDs and the proposed effect.
    pub(crate) fn react(
        authority_id: AuthorityId,
        scope: ReactScope,
        invocation_id: InvocationId,
    ) -> Self {
        Self {
            authority_id,
            scope,
            invocation_id,
        }
    }
}

/// Errors returned while minting or maintaining receipt-gate state.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub(crate) enum ReceiptStoreError {
    /// Internal mutex state was poisoned by a prior panic.
    #[error("receipt store unavailable")]
    StoreUnavailable,
    /// Source content is not an exact v1 structured reaction command.
    #[error("source event does not contain a valid structured react command")]
    InvalidCommand,
    /// Policy evidence does not authorize this source principal and channel.
    #[error("policy authorization does not match source event")]
    PolicyMismatch,
    /// Repeated cryptographic ID collisions prevented safe minting.
    #[error("could not allocate a unique authority identifier")]
    AuthorityIdCollision,
    /// The bounded active-receipt store is at capacity.
    #[error("active receipt capacity exceeded")]
    ReceiptCapacityExceeded,
}

/// In-memory v1 authority receipt and invocation-claim store.
///
/// This is a lab fixture. Its state is intentionally fail-stop across process
/// restarts until durable storage and recovery are designed.
pub(crate) struct ReceiptStore {
    receipts: Mutex<HashMap<AuthorityId, ActionReceipt>>,
    claimed_invocations: Mutex<HashMap<InvocationId, Instant>>,
    max_active_receipts: usize,
    max_active_invocations: usize,
}

impl Default for ReceiptStore {
    fn default() -> Self {
        Self {
            receipts: Mutex::new(HashMap::new()),
            claimed_invocations: Mutex::new(HashMap::new()),
            max_active_receipts: DEFAULT_MAX_ACTIVE_RECEIPTS,
            max_active_invocations: DEFAULT_MAX_ACTIVE_INVOCATIONS,
        }
    }
}

impl ReceiptStore {
    /// Creates an empty receipt store.
    pub(crate) fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn with_capacity_limits(max_active_receipts: usize, max_active_invocations: usize) -> Self {
        Self {
            receipts: Mutex::new(HashMap::new()),
            claimed_invocations: Mutex::new(HashMap::new()),
            max_active_receipts,
            max_active_invocations,
        }
    }

    /// Creates a fresh invocation identifier on the trusted harness side.
    ///
    /// The caller cannot choose the identifier, so model-controlled request
    /// data cannot bypass deduplication by supplying arbitrary strings.
    pub(crate) fn begin_invocation(&self) -> InvocationId {
        InvocationId(Uuid::new_v4())
    }

    /// Mints an authority from independently verified source and policy evidence.
    ///
    /// The reaction scope is derived from authenticated source content rather
    /// than accepted from the caller. Evidence constructors are sealed inside
    /// this module until their trusted adapters are runtime-wired.
    pub(crate) fn mint(
        &self,
        source: &AuthenticatedSourceEvent,
        policy: &PolicyAuthorization,
    ) -> Result<AuthorityId, ReceiptStoreError> {
        self.mint_with_id_generator(source, policy, Uuid::new_v4)
    }

    fn mint_with_id_generator(
        &self,
        source: &AuthenticatedSourceEvent,
        policy: &PolicyAuthorization,
        mut generate_id: impl FnMut() -> Uuid,
    ) -> Result<AuthorityId, ReceiptStoreError> {
        if source.principal != policy.principal || source.channel != policy.channel {
            return Err(ReceiptStoreError::PolicyMismatch);
        }

        let command =
            parse_structured_command(&source.content).ok_or(ReceiptStoreError::InvalidCommand)?;
        let now = Instant::now();
        let receipt = ActionReceipt {
            source_event_id: source.event_id,
            principal: source.principal,
            scope: ReactScope {
                channel: source.channel,
                target_msg: command.target_msg,
                emoji: command.emoji,
            },
            expires: now + DEFAULT_TTL,
            verifier: source.verifier,
            source_assurance: source.assurance,
            source_content_sha256: source.content_sha256,
            policy_snapshot: policy.snapshot.clone(),
        };

        let mut receipts = self
            .receipts
            .lock()
            .map_err(|_| ReceiptStoreError::StoreUnavailable)?;
        if receipts.len() >= self.max_active_receipts {
            let cleanup_now = Instant::now();
            receipts.retain(|_, active_receipt| active_receipt.expires > cleanup_now);
            if receipts.len() >= self.max_active_receipts {
                return Err(ReceiptStoreError::ReceiptCapacityExceeded);
            }
        }
        for _ in 0..MAX_AUTHORITY_ID_ATTEMPTS {
            let authority_id = AuthorityId(generate_id());
            if let Entry::Vacant(entry) = receipts.entry(authority_id) {
                entry.insert(receipt);
                return Ok(authority_id);
            }
        }
        Err(ReceiptStoreError::AuthorityIdCollision)
    }

    /// Evaluates an action proposal and atomically claims its invocation on allow.
    pub(crate) fn verify(&self, request: &VerifyRequest) -> GateResult {
        let receipt = {
            let receipts = match self.receipts.lock() {
                Ok(receipts) => receipts,
                Err(_) => return request.result(GateVerdict::Deny, GateReason::StoreUnavailable),
            };

            let Some(receipt) = receipts.get(&request.authority_id) else {
                return request.result(GateVerdict::Deny, GateReason::AuthorityNotFound);
            };

            if receipt.scope.channel != request.scope.channel {
                return request.result(GateVerdict::Deny, GateReason::ChannelMismatch);
            }
            if receipt.scope.target_msg != request.scope.target_msg {
                return request.result(GateVerdict::Deny, GateReason::TargetMismatch);
            }
            if receipt.scope.emoji != request.scope.emoji {
                return request.result(GateVerdict::Deny, GateReason::EmojiMismatch);
            }
            if Instant::now() >= receipt.expires {
                return request.result(GateVerdict::Deny, GateReason::AuthorityExpired);
            }

            receipt.clone()
        };

        let mut claimed = match self.claimed_invocations.lock() {
            Ok(claimed) => claimed,
            Err(_) => return request.result(GateVerdict::Deny, GateReason::StoreUnavailable),
        };
        let now = Instant::now();
        Self::claim_invocation_at(
            &mut claimed,
            request,
            receipt.expires,
            now,
            self.max_active_invocations,
        )
    }

    /// Removes expired authorities and invocation claims.
    pub(crate) fn gc_expired(&self) -> Result<(), ReceiptStoreError> {
        let now = Instant::now();
        self.receipts
            .lock()
            .map_err(|_| ReceiptStoreError::StoreUnavailable)?
            .retain(|_, receipt| receipt.expires > now);
        self.claimed_invocations
            .lock()
            .map_err(|_| ReceiptStoreError::StoreUnavailable)?
            .retain(|_, expires| *expires > now);
        Ok(())
    }

    fn claim_invocation_at(
        claimed: &mut HashMap<InvocationId, Instant>,
        request: &VerifyRequest,
        authority_expires: Instant,
        now: Instant,
        max_active_invocations: usize,
    ) -> GateResult {
        if now >= authority_expires {
            return request.result(GateVerdict::Deny, GateReason::AuthorityExpired);
        }
        if claimed.contains_key(&request.invocation_id) {
            return request.result(GateVerdict::Deny, GateReason::DuplicateInvocation);
        }
        if claimed.len() >= max_active_invocations {
            claimed.retain(|_, expires| *expires > now);
            if claimed.len() >= max_active_invocations {
                return request.result(GateVerdict::Deny, GateReason::InvocationCapacityExceeded);
            }
        }
        claimed.insert(request.invocation_id, authority_expires);

        request.result(GateVerdict::Allow, GateReason::PredicatesPassed)
    }
}

impl VerifyRequest {
    fn result(&self, verdict: GateVerdict, reason: GateReason) -> GateResult {
        GateResult {
            verdict,
            reason,
            authority_id: self.authority_id,
            invocation_id: self.invocation_id,
        }
    }
}

/// Parsed representation of the v1 `/react` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ParsedCommand {
    /// Exact Unicode or Discord custom-emoji token.
    pub(crate) emoji: String,
    /// Nonzero Discord message ID targeted by the reaction.
    pub(crate) target_msg: MessageId,
}

/// Parses the exact v1 grammar `/react <emoji-token> <nonzero-message-id>`.
///
/// The parser deliberately rejects leading/trailing whitespace, repeated
/// spaces, tabs, newlines, and extra tokens. Semantic interpretation is not
/// part of the authority path.
pub(crate) fn parse_structured_command(content: &str) -> Option<ParsedCommand> {
    let rest = content.strip_prefix("/react ")?;
    let (emoji, message_id) = rest.split_once(' ')?;
    if emoji.is_empty()
        || message_id.is_empty()
        || emoji.chars().any(char::is_whitespace)
        || message_id.chars().any(char::is_whitespace)
    {
        return None;
    }

    let message_id = message_id.parse::<u64>().ok()?;
    if message_id == 0 {
        return None;
    }

    Some(ParsedCommand {
        emoji: emoji.to_owned(),
        target_msg: MessageId::new(message_id),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    const PACE: UserId = UserId::new(437002871280631808);
    const GUEST: UserId = UserId::new(999999);
    const CHANNEL: ChannelId = ChannelId::new(100);

    fn test_scope() -> ReactScope {
        ReactScope {
            channel: CHANNEL,
            target_msg: MessageId::new(123),
            emoji: "🎯".into(),
        }
    }

    fn test_source_event() -> AuthenticatedSourceEvent {
        AuthenticatedSourceEvent::from_dione_gateway(
            MessageId::new(456),
            PACE,
            CHANNEL,
            "/react 🎯 123".into(),
        )
    }

    fn test_policy() -> PolicyAuthorization {
        PolicyAuthorization::allow_react(PACE, CHANNEL, "discord-role-policy", 7)
    }

    fn test_action_receipt(expires: Instant) -> ActionReceipt {
        ActionReceipt {
            source_event_id: MessageId::new(456),
            principal: PACE,
            scope: test_scope(),
            expires,
            verifier: VerifierKind::DioneGateway,
            source_assurance: SourceAssurance::AuthenticatedGatewayEvent,
            source_content_sha256: Sha256::digest("/react 🎯 123".as_bytes()).into(),
            policy_snapshot: test_policy().snapshot,
        }
    }

    fn mint_test_receipt(store: &ReceiptStore) -> AuthorityId {
        store
            .mint(&test_source_event(), &test_policy())
            .expect("mint should succeed")
    }

    fn verify_request(
        store: &ReceiptStore,
        authority_id: AuthorityId,
        scope: ReactScope,
    ) -> VerifyRequest {
        VerifyRequest::react(authority_id, scope, store.begin_invocation())
    }

    #[test]
    fn baseline_authenticated_command_allows_exact_reaction() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);

        let result = store.verify(&verify_request(&store, authority_id, test_scope()));

        assert_eq!(result.verdict, GateVerdict::Allow);
        assert_eq!(result.reason, GateReason::PredicatesPassed);
        assert_eq!(result.authority_id, authority_id);
    }

    #[test]
    fn fabricated_authority_denies() {
        let store = ReceiptStore::new();
        let request = VerifyRequest::react(
            AuthorityId(Uuid::new_v4()),
            test_scope(),
            store.begin_invocation(),
        );

        let result = store.verify(&request);

        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, GateReason::AuthorityNotFound);
    }

    #[test]
    fn mint_derives_scope_from_authenticated_content() {
        let store = ReceiptStore::new();
        let source = AuthenticatedSourceEvent::from_dione_gateway(
            MessageId::new(456),
            PACE,
            CHANNEL,
            "/react 👍 789".into(),
        );
        let authority_id = store
            .mint(&source, &test_policy())
            .expect("mint should succeed");

        let allowed_scope = ReactScope {
            channel: CHANNEL,
            target_msg: MessageId::new(789),
            emoji: "👍".into(),
        };
        assert_eq!(
            store
                .verify(&verify_request(&store, authority_id, allowed_scope))
                .verdict,
            GateVerdict::Allow
        );
    }

    #[test]
    fn invalid_authenticated_command_cannot_mint() {
        let store = ReceiptStore::new();
        let source = AuthenticatedSourceEvent::from_dione_gateway(
            MessageId::new(456),
            PACE,
            CHANNEL,
            "please react 🎯 to 123".into(),
        );

        assert!(matches!(
            store.mint(&source, &test_policy()),
            Err(ReceiptStoreError::InvalidCommand)
        ));
    }

    #[test]
    fn policy_for_different_principal_cannot_mint() {
        let store = ReceiptStore::new();
        let wrong_policy =
            PolicyAuthorization::allow_react(GUEST, CHANNEL, "discord-role-policy", 7);

        assert!(matches!(
            store.mint(&test_source_event(), &wrong_policy),
            Err(ReceiptStoreError::PolicyMismatch)
        ));
    }

    #[test]
    fn policy_for_different_channel_cannot_mint() {
        let store = ReceiptStore::new();
        let wrong_policy =
            PolicyAuthorization::allow_react(PACE, ChannelId::new(999), "discord-role-policy", 7);

        assert!(matches!(
            store.mint(&test_source_event(), &wrong_policy),
            Err(ReceiptStoreError::PolicyMismatch)
        ));
    }

    #[test]
    fn receipt_retains_mint_time_policy_snapshot() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);
        let receipts = store.receipts.lock().expect("store should be available");
        let receipt = receipts
            .get(&authority_id)
            .expect("minted receipt should exist");

        assert_eq!(receipt.policy_snapshot.policy_id, "discord-role-policy");
        assert_eq!(receipt.policy_snapshot.version, 7);
        assert_eq!(receipt.source_event_id, MessageId::new(456));
        assert_eq!(receipt.principal, PACE);
        assert_eq!(receipt.verifier, VerifierKind::DioneGateway);
        assert_eq!(
            receipt.source_assurance,
            SourceAssurance::AuthenticatedGatewayEvent
        );
        assert_eq!(
            receipt.source_content_sha256,
            <[u8; 32]>::from(Sha256::digest("/react 🎯 123".as_bytes()))
        );
    }

    #[test]
    fn scope_substitution_denies_each_mismatch() {
        let cases = [
            (
                ReactScope {
                    channel: ChannelId::new(999),
                    ..test_scope()
                },
                GateReason::ChannelMismatch,
            ),
            (
                ReactScope {
                    target_msg: MessageId::new(789),
                    ..test_scope()
                },
                GateReason::TargetMismatch,
            ),
            (
                ReactScope {
                    emoji: "👍".into(),
                    ..test_scope()
                },
                GateReason::EmojiMismatch,
            ),
        ];

        for (scope, expected_reason) in cases {
            let store = ReceiptStore::new();
            let authority_id = mint_test_receipt(&store);
            let result = store.verify(&verify_request(&store, authority_id, scope));
            assert_eq!(result.verdict, GateVerdict::Deny);
            assert_eq!(result.reason, expected_reason);
        }
    }

    #[test]
    fn receipt_is_reusable_with_distinct_harness_invocations() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);

        let first = store.verify(&verify_request(&store, authority_id, test_scope()));
        let second = store.verify(&verify_request(&store, authority_id, test_scope()));

        assert_eq!(first.verdict, GateVerdict::Allow);
        assert_eq!(second.verdict, GateVerdict::Allow);
        assert_ne!(first.invocation_id, second.invocation_id);
    }

    #[test]
    fn concurrent_duplicate_invocation_allows_exactly_once() {
        let store = Arc::new(ReceiptStore::new());
        let authority_id = mint_test_receipt(&store);
        let invocation_id = store.begin_invocation();
        let barrier = Arc::new(Barrier::new(3));

        let handles: Vec<_> = (0..2)
            .map(|_| {
                let store = Arc::clone(&store);
                let barrier = Arc::clone(&barrier);
                thread::spawn(move || {
                    let request = VerifyRequest::react(authority_id, test_scope(), invocation_id);
                    barrier.wait();
                    store.verify(&request)
                })
            })
            .collect();

        barrier.wait();
        let results: Vec<_> = handles
            .into_iter()
            .map(|handle| handle.join().expect("worker should not panic"))
            .collect();
        assert_eq!(
            results
                .iter()
                .filter(|result| result.verdict == GateVerdict::Allow)
                .count(),
            1
        );
        assert_eq!(
            results
                .iter()
                .filter(|result| result.reason == GateReason::DuplicateInvocation)
                .count(),
            1
        );
    }

    #[test]
    fn expired_receipt_denies() {
        let store = ReceiptStore::new();
        let authority_id = AuthorityId(Uuid::new_v4());
        let policy = test_policy();
        store
            .receipts
            .lock()
            .expect("store should be available")
            .insert(
                authority_id,
                ActionReceipt {
                    source_event_id: MessageId::new(1),
                    principal: PACE,
                    scope: test_scope(),
                    expires: Instant::now() - Duration::from_secs(1),
                    verifier: VerifierKind::DioneGateway,
                    source_assurance: SourceAssurance::AuthenticatedGatewayEvent,
                    source_content_sha256: Sha256::digest("/react 🎯 123".as_bytes()).into(),
                    policy_snapshot: policy.snapshot,
                },
            );

        let result = store.verify(&verify_request(&store, authority_id, test_scope()));
        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, GateReason::AuthorityExpired);
    }

    #[test]
    fn authority_collision_retries_without_overwriting_existing_receipt() {
        let store = ReceiptStore::new();
        let first_id = Uuid::new_v4();
        let second_id = Uuid::new_v4();
        let first = store
            .mint_with_id_generator(&test_source_event(), &test_policy(), || first_id)
            .expect("first mint should succeed");
        let mut generated = [first_id, second_id].into_iter();
        let second = store
            .mint_with_id_generator(&test_source_event(), &test_policy(), || {
                generated.next().expect("generator should have another ID")
            })
            .expect("collision should be retried");

        assert_eq!(first.as_uuid(), first_id);
        assert_eq!(second.as_uuid(), second_id);
        assert_eq!(
            store
                .receipts
                .lock()
                .expect("store should be available")
                .len(),
            2
        );
    }

    #[test]
    fn repeated_authority_collisions_fail_without_overwriting() {
        let store = ReceiptStore::new();
        let repeated_id = Uuid::new_v4();
        store
            .mint_with_id_generator(&test_source_event(), &test_policy(), || repeated_id)
            .expect("first mint should succeed");

        assert!(matches!(
            store.mint_with_id_generator(&test_source_event(), &test_policy(), || repeated_id),
            Err(ReceiptStoreError::AuthorityIdCollision)
        ));
        assert_eq!(
            store
                .receipts
                .lock()
                .expect("store should be available")
                .len(),
            1
        );
    }

    #[test]
    fn invocation_claims_expire_with_their_authority() {
        let store = ReceiptStore::new();
        let stale_invocation = store.begin_invocation();
        let live_invocation = store.begin_invocation();
        let now = Instant::now();
        {
            let mut claimed = store
                .claimed_invocations
                .lock()
                .expect("claims should be available");
            claimed.insert(stale_invocation, now - Duration::from_secs(1));
            claimed.insert(live_invocation, now + DEFAULT_TTL);
        }

        store.gc_expired().expect("gc should succeed");

        let claimed = store
            .claimed_invocations
            .lock()
            .expect("claims should be available");
        assert!(!claimed.contains_key(&stale_invocation));
        assert!(claimed.contains_key(&live_invocation));
    }

    #[test]
    fn invocation_claim_rechecks_expiry_at_commit_time() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);
        let request = verify_request(&store, authority_id, test_scope());
        let expires = Instant::now();
        let mut claimed = HashMap::new();

        let result = ReceiptStore::claim_invocation_at(
            &mut claimed,
            &request,
            expires,
            expires,
            DEFAULT_MAX_ACTIVE_INVOCATIONS,
        );

        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, GateReason::AuthorityExpired);
        assert!(!claimed.contains_key(&request.invocation_id));
    }

    #[test]
    fn verification_leaves_expired_claim_cleanup_to_gc() {
        let store = ReceiptStore::new();
        let stale_invocation = store.begin_invocation();
        store
            .claimed_invocations
            .lock()
            .expect("claims should be available")
            .insert(stale_invocation, Instant::now() - Duration::from_secs(1));
        let authority_id = mint_test_receipt(&store);

        let result = store.verify(&verify_request(&store, authority_id, test_scope()));

        assert_eq!(result.verdict, GateVerdict::Allow);
        assert!(
            store
                .claimed_invocations
                .lock()
                .expect("claims should be available")
                .contains_key(&stale_invocation),
            "verify must not scan the entire claim table"
        );
        store.gc_expired().expect("gc should succeed");
        assert!(
            !store
                .claimed_invocations
                .lock()
                .expect("claims should be available")
                .contains_key(&stale_invocation)
        );
    }

    #[test]
    fn receipt_capacity_fails_closed() {
        let store = ReceiptStore::with_capacity_limits(1, 2);
        mint_test_receipt(&store);

        assert!(matches!(
            store.mint(&test_source_event(), &test_policy()),
            Err(ReceiptStoreError::ReceiptCapacityExceeded)
        ));
        assert_eq!(
            store
                .receipts
                .lock()
                .expect("receipts should be available")
                .len(),
            1
        );
    }

    #[test]
    fn receipt_capacity_reclaims_expired_entries() {
        let store = ReceiptStore::with_capacity_limits(1, 2);
        store
            .receipts
            .lock()
            .expect("receipts should be available")
            .insert(
                AuthorityId(Uuid::new_v4()),
                test_action_receipt(Instant::now() - Duration::from_secs(1)),
            );

        let authority_id = store
            .mint(&test_source_event(), &test_policy())
            .expect("expired receipt should be reclaimed at capacity");

        let receipts = store.receipts.lock().expect("receipts should be available");
        assert_eq!(receipts.len(), 1);
        assert!(receipts.contains_key(&authority_id));
    }

    #[test]
    fn invocation_capacity_fails_closed() {
        let store = ReceiptStore::with_capacity_limits(2, 1);
        let authority_id = mint_test_receipt(&store);
        assert_eq!(
            store
                .verify(&verify_request(&store, authority_id, test_scope()))
                .verdict,
            GateVerdict::Allow
        );

        let result = store.verify(&verify_request(&store, authority_id, test_scope()));

        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, GateReason::InvocationCapacityExceeded);
    }

    #[test]
    fn invocation_capacity_reclaims_expired_entries() {
        let store = ReceiptStore::with_capacity_limits(2, 1);
        let stale_invocation = store.begin_invocation();
        store
            .claimed_invocations
            .lock()
            .expect("claims should be available")
            .insert(stale_invocation, Instant::now() - Duration::from_secs(1));
        let authority_id = mint_test_receipt(&store);

        let result = store.verify(&verify_request(&store, authority_id, test_scope()));

        assert_eq!(result.verdict, GateVerdict::Allow);
        let claims = store
            .claimed_invocations
            .lock()
            .expect("claims should be available");
        assert_eq!(claims.len(), 1);
        assert!(!claims.contains_key(&stale_invocation));
    }

    #[test]
    fn gc_removes_expired_receipts() {
        let store = ReceiptStore::new();
        let expired_id = AuthorityId(Uuid::new_v4());
        store
            .receipts
            .lock()
            .expect("receipts should be available")
            .insert(
                expired_id,
                test_action_receipt(Instant::now() - Duration::from_secs(1)),
            );

        store.gc_expired().expect("gc should succeed");

        assert!(
            !store
                .receipts
                .lock()
                .expect("receipts should be available")
                .contains_key(&expired_id)
        );
    }

    #[test]
    fn invocation_deduplication_is_global_across_authorities() {
        let store = ReceiptStore::new();
        let first_authority = mint_test_receipt(&store);
        let second_authority = mint_test_receipt(&store);
        let invocation_id = store.begin_invocation();
        let first = VerifyRequest::react(first_authority, test_scope(), invocation_id);
        let second = VerifyRequest::react(second_authority, test_scope(), invocation_id);

        assert_eq!(store.verify(&first).verdict, GateVerdict::Allow);
        let replay = store.verify(&second);
        assert_eq!(replay.verdict, GateVerdict::Deny);
        assert_eq!(replay.reason, GateReason::DuplicateInvocation);
    }

    #[test]
    fn poisoned_receipt_state_fails_closed_without_panicking() {
        let store = Arc::new(ReceiptStore::new());
        let authority_id = mint_test_receipt(&store);
        let poison_target = Arc::clone(&store);
        let _ = thread::spawn(move || {
            let _guard = poison_target
                .receipts
                .lock()
                .expect("store should initially be available");
            panic!("poison receipt state for the test");
        })
        .join();

        let result = store.verify(&verify_request(&store, authority_id, test_scope()));
        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, GateReason::StoreUnavailable);
        assert!(matches!(
            store.gc_expired(),
            Err(ReceiptStoreError::StoreUnavailable)
        ));
    }

    #[test]
    fn poisoned_invocation_state_fails_closed_without_panicking() {
        let store = Arc::new(ReceiptStore::new());
        let authority_id = mint_test_receipt(&store);
        let poison_target = Arc::clone(&store);
        let _ = thread::spawn(move || {
            let _guard = poison_target
                .claimed_invocations
                .lock()
                .expect("claims should initially be available");
            panic!("poison invocation state for the test");
        })
        .join();

        let result = store.verify(&verify_request(&store, authority_id, test_scope()));
        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, GateReason::StoreUnavailable);
    }

    #[test]
    fn authority_ids_are_unique_uuid_v4_values() {
        let store = ReceiptStore::new();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = mint_test_receipt(&store);
            assert_eq!(id.as_uuid().get_version_num(), 4);
            assert!(ids.insert(id), "collision detected");
        }
    }

    #[test]
    fn untrusted_authority_text_parses_only_as_uuid() {
        let authority_id = AuthorityId(Uuid::new_v4());
        let parsed = authority_id
            .to_string()
            .parse::<AuthorityId>()
            .expect("generated authority should parse");

        assert_eq!(parsed, authority_id);
        assert!("4@k3-fake-id".parse::<AuthorityId>().is_err());
        assert!("".parse::<AuthorityId>().is_err());
    }

    #[test]
    fn parser_accepts_exact_react_grammar() {
        assert_eq!(
            parse_structured_command("/react 🎯 123"),
            Some(ParsedCommand {
                emoji: "🎯".into(),
                target_msg: MessageId::new(123),
            })
        );
        assert_eq!(
            parse_structured_command("/react <:party:123456> 456"),
            Some(ParsedCommand {
                emoji: "<:party:123456>".into(),
                target_msg: MessageId::new(456),
            })
        );
    }

    #[test]
    fn parser_rejects_noncanonical_or_ambiguous_input() {
        for invalid in [
            "hello world",
            "react 🎯 123",
            "/reply hello",
            "/react",
            "/react 🎯",
            "/react 🎯 notanumber",
            "/react 🎯 extra 123",
            "/react  🎯 123",
            "/react 🎯  123",
            "/react\t🎯\t123",
            " /react 🎯 123",
            "/react 🎯 123 ",
            "/react 🎯\n123",
            "/react 🎯 0",
        ] {
            assert_eq!(
                parse_structured_command(invalid),
                None,
                "accepted {invalid:?}"
            );
        }
    }
}
