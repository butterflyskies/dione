use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serenity::model::id::{ChannelId, MessageId, UserId};
use uuid::Uuid;

const DEFAULT_TTL: Duration = Duration::from_secs(3600);

// ── Types ────────────────────────────────────────────────────────────────────

/// Opaque, CSPRNG-backed authority identifier. Cannot be constructed outside
/// this module — the only source is `ReceiptStore::mint`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct AuthorityId(String);

impl AuthorityId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for AuthorityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Opaque invocation identifier. Constructed only by trusted harness code via
/// `InvocationId::from_harness`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct InvocationId(String);

impl InvocationId {
    pub(crate) fn from_harness(id: impl Into<String>) -> Self {
        Self(id.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// Sealed source event — only trusted ingress adapters can construct this.
/// Fields are read-only after construction.
pub struct AuthenticatedSourceEvent {
    pub(crate) event_id: MessageId,
    pub(crate) principal: UserId,
    pub(crate) channel: ChannelId,
    pub(crate) content: String,
    pub(crate) verifier: VerifierKind,
}

impl AuthenticatedSourceEvent {
    pub(crate) fn from_dione_gateway(
        event_id: MessageId,
        principal: UserId,
        channel: ChannelId,
        content: String,
    ) -> Self {
        Self {
            event_id,
            principal,
            channel,
            content,
            verifier: VerifierKind::DioneGateway,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum VerifierKind {
    DioneGateway,
}

/// v1 supports only React. Other actions require explicit scope extensions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReactScope {
    pub channel: ChannelId,
    pub target_msg: MessageId,
    pub emoji: String,
}

#[derive(Debug, Clone)]
struct ActionReceipt {
    _source_event_id: MessageId,
    principal: UserId,
    scope: ReactScope,
    issued: Instant,
    expires: Instant,
    _verifier: VerifierKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateVerdict {
    Allow,
    Deny,
}

#[derive(Debug, Clone)]
pub struct GateResult {
    pub verdict: GateVerdict,
    pub reason: String,
    pub authority_id: String,
    pub invocation_id: Option<String>,
}

pub struct VerifyRequest {
    pub authority_id: AuthorityId,
    pub scope: ReactScope,
    pub invocation_id: InvocationId,
}

#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ReceiptStoreError {
    #[error("lock poisoned")]
    LockPoisoned,
    #[error("principal not authorized for react")]
    Unauthorized,
    #[error("authority_id collision")]
    Collision,
}

// ── Store ────────────────────────────────────────────────────────────────────

pub struct ReceiptStore {
    receipts: Mutex<HashMap<String, ActionReceipt>>,
    claimed_invocations: Mutex<HashMap<String, Instant>>,
}

impl Default for ReceiptStore {
    fn default() -> Self {
        Self {
            receipts: Mutex::new(HashMap::new()),
            claimed_invocations: Mutex::new(HashMap::new()),
        }
    }
}

impl ReceiptStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// Mint a receipt from a verified source event. Only callable with a sealed
    /// `AuthenticatedSourceEvent` (constructed by trusted adapter code).
    pub(crate) fn mint(
        &self,
        source: &AuthenticatedSourceEvent,
        scope: ReactScope,
        policy_check: impl FnOnce(UserId) -> bool,
    ) -> Result<AuthorityId, ReceiptStoreError> {
        if !policy_check(source.principal) {
            return Err(ReceiptStoreError::Unauthorized);
        }

        let now = Instant::now();
        let authority_id = Uuid::new_v4().to_string();

        let receipt = ActionReceipt {
            _source_event_id: source.event_id,
            principal: source.principal,
            scope,
            issued: now,
            expires: now + DEFAULT_TTL,
            _verifier: source.verifier,
        };

        let mut store = self
            .receipts
            .lock()
            .map_err(|_| ReceiptStoreError::LockPoisoned)?;
        use std::collections::hash_map::Entry;
        match store.entry(authority_id.clone()) {
            Entry::Vacant(e) => {
                e.insert(receipt);
            }
            Entry::Occupied(_) => {
                return Err(ReceiptStoreError::Collision);
            }
        }
        Ok(AuthorityId(authority_id))
    }

    pub(crate) fn verify(&self, request: &VerifyRequest) -> GateResult {
        let store = match self.receipts.lock() {
            Ok(s) => s,
            Err(_) => {
                return GateResult {
                    verdict: GateVerdict::Deny,
                    reason: "store lock poisoned".into(),
                    authority_id: request.authority_id.0.clone(),
                    invocation_id: Some(request.invocation_id.0.clone()),
                };
            }
        };

        let receipt = match store.get(&request.authority_id.0) {
            Some(r) => r,
            None => {
                return GateResult {
                    verdict: GateVerdict::Deny,
                    reason: "authority_id not found".into(),
                    authority_id: request.authority_id.0.clone(),
                    invocation_id: Some(request.invocation_id.0.clone()),
                };
            }
        };

        if receipt.scope.channel != request.scope.channel {
            return GateResult {
                verdict: GateVerdict::Deny,
                reason: "channel mismatch".into(),
                authority_id: request.authority_id.0.clone(),
                invocation_id: Some(request.invocation_id.0.clone()),
            };
        }

        if receipt.scope.target_msg != request.scope.target_msg {
            return GateResult {
                verdict: GateVerdict::Deny,
                reason: "target mismatch".into(),
                authority_id: request.authority_id.0.clone(),
                invocation_id: Some(request.invocation_id.0.clone()),
            };
        }

        if receipt.scope.emoji != request.scope.emoji {
            return GateResult {
                verdict: GateVerdict::Deny,
                reason: "emoji mismatch".into(),
                authority_id: request.authority_id.0.clone(),
                invocation_id: Some(request.invocation_id.0.clone()),
            };
        }

        if Instant::now() >= receipt.expires {
            return GateResult {
                verdict: GateVerdict::Deny,
                reason: "authority expired".into(),
                authority_id: request.authority_id.0.clone(),
                invocation_id: Some(request.invocation_id.0.clone()),
            };
        }

        drop(store);

        let mut claimed = match self.claimed_invocations.lock() {
            Ok(c) => c,
            Err(_) => {
                return GateResult {
                    verdict: GateVerdict::Deny,
                    reason: "claimed set lock poisoned".into(),
                    authority_id: request.authority_id.0.clone(),
                    invocation_id: Some(request.invocation_id.0.clone()),
                };
            }
        };

        let inv_key = request.invocation_id.0.clone();
        if claimed.contains_key(&inv_key) {
            return GateResult {
                verdict: GateVerdict::Deny,
                reason: "duplicate invocation".into(),
                authority_id: request.authority_id.0.clone(),
                invocation_id: Some(request.invocation_id.0.clone()),
            };
        }
        claimed.insert(inv_key, Instant::now());

        GateResult {
            verdict: GateVerdict::Allow,
            reason: "all predicates passed".into(),
            authority_id: request.authority_id.0.clone(),
            invocation_id: Some(request.invocation_id.0.clone()),
        }
    }

    pub(crate) fn gc_expired(&self) {
        let now = Instant::now();
        if let Ok(mut store) = self.receipts.lock() {
            store.retain(|_, receipt| receipt.expires > now);
        }
        if let Ok(mut claimed) = self.claimed_invocations.lock() {
            claimed.retain(|_, issued_at| now.duration_since(*issued_at) < DEFAULT_TTL);
        }
    }
}

// ── Command parser ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub emoji: String,
    pub target_msg: MessageId,
}

pub fn parse_structured_command(content: &str) -> Option<ParsedCommand> {
    let content = content.trim();
    if content.contains('\n') {
        return None;
    }

    let rest = content.strip_prefix("/react ")?;
    let parts: Vec<&str> = rest.split_whitespace().collect();
    if parts.len() != 2 {
        return None;
    }

    let emoji = parts[0];
    let msg_id: u64 = parts[1].parse().ok()?;
    if msg_id == 0 {
        return None;
    }

    Some(ParsedCommand {
        emoji: emoji.to_string(),
        target_msg: MessageId::new(msg_id),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Barrier};
    use std::thread;

    use super::*;

    const PACE: UserId = UserId::new(437002871280631808);
    const GUEST: UserId = UserId::new(999999);

    fn test_scope() -> ReactScope {
        ReactScope {
            channel: ChannelId::new(100),
            target_msg: MessageId::new(123),
            emoji: "🎯".into(),
        }
    }

    fn test_source_event() -> AuthenticatedSourceEvent {
        AuthenticatedSourceEvent::from_dione_gateway(
            MessageId::new(456),
            PACE,
            ChannelId::new(100),
            "/react 🎯 123".into(),
        )
    }

    fn allow_all(_: UserId) -> bool {
        true
    }

    fn deny_guests(user: UserId) -> bool {
        user != GUEST
    }

    fn mint_test_receipt(store: &ReceiptStore) -> AuthorityId {
        store
            .mint(&test_source_event(), test_scope(), allow_all)
            .expect("mint should succeed")
    }

    // ── Case 1: Baseline — real command, full chain, ALLOW ──────────────────

    #[test]
    fn case1_baseline_allow() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);

        let result = store.verify(&VerifyRequest {
            authority_id: authority_id.clone(),
            scope: test_scope(),
            invocation_id: InvocationId::from_harness("inv_001"),
        });

        assert_eq!(result.verdict, GateVerdict::Allow);
        assert_eq!(result.authority_id, authority_id.as_str());
    }

    // ── Case 2: Phantom — fabricated authority_id, DENY ─────────────────────

    #[test]
    fn case2_phantom_deny() {
        let store = ReceiptStore::new();

        let result = store.verify(&VerifyRequest {
            authority_id: AuthorityId("4@k3-fake-id".into()),
            scope: test_scope(),
            invocation_id: InvocationId::from_harness("inv_002"),
        });

        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, "authority_id not found");
    }

    // ── Case 3: Scope substitution — wrong target ───────────────────────────

    #[test]
    fn case3_scope_substitution_wrong_target() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);

        let result = store.verify(&VerifyRequest {
            authority_id,
            scope: ReactScope {
                channel: ChannelId::new(100),
                target_msg: MessageId::new(789),
                emoji: "🎯".into(),
            },
            invocation_id: InvocationId::from_harness("inv_003"),
        });

        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, "target mismatch");
    }

    // ── Case 4: Replay — reuse within TTL, ALLOW ────────────────────────────

    #[test]
    fn case4_replay_allow() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);

        let r1 = store.verify(&VerifyRequest {
            authority_id: authority_id.clone(),
            scope: test_scope(),
            invocation_id: InvocationId::from_harness("inv_004a"),
        });
        assert_eq!(r1.verdict, GateVerdict::Allow);

        let r2 = store.verify(&VerifyRequest {
            authority_id,
            scope: test_scope(),
            invocation_id: InvocationId::from_harness("inv_004b"),
        });
        assert_eq!(r2.verdict, GateVerdict::Allow);
    }

    // ── Case 5: Concurrent duplicate — same invocation_id races ─────────────

    #[test]
    fn case5_concurrent_duplicate() {
        let store = Arc::new(ReceiptStore::new());
        let source = AuthenticatedSourceEvent::from_dione_gateway(
            MessageId::new(456),
            PACE,
            ChannelId::new(100),
            "/react 🎯 123".into(),
        );
        let authority_id = store
            .mint(&source, test_scope(), allow_all)
            .expect("mint should succeed");

        let barrier = Arc::new(Barrier::new(2));
        let mut handles = Vec::new();

        for _ in 0..2 {
            let s = Arc::clone(&store);
            let aid = authority_id.clone();
            let b = Arc::clone(&barrier);
            handles.push(thread::spawn(move || {
                b.wait();
                s.verify(&VerifyRequest {
                    authority_id: aid,
                    scope: test_scope(),
                    invocation_id: InvocationId::from_harness("inv_005_shared"),
                })
            }));
        }

        let results: Vec<_> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let allows = results
            .iter()
            .filter(|r| r.verdict == GateVerdict::Allow)
            .count();
        let denies = results
            .iter()
            .filter(|r| r.verdict == GateVerdict::Deny)
            .count();

        assert_eq!(allows, 1, "exactly one thread should win");
        assert_eq!(denies, 1, "exactly one thread should be denied");
    }

    // ── Case 6: Unauthorized principal — DENY at mint ───────────────────────

    #[test]
    fn case6_unauthorized_principal() {
        let store = ReceiptStore::new();

        let source = AuthenticatedSourceEvent::from_dione_gateway(
            MessageId::new(500),
            GUEST,
            ChannelId::new(100),
            "/react 🎯 123".into(),
        );

        let result = store.mint(&source, test_scope(), deny_guests);
        assert!(result.is_err());
        assert!(matches!(
            result.unwrap_err(),
            ReceiptStoreError::Unauthorized
        ));
    }

    // ── Case 7: Expired receipt — past TTL, DENY ────────────────────────────

    #[test]
    fn case7_expired_deny() {
        let store = ReceiptStore::new();
        let authority_id_str = "expired-test-id".to_string();

        {
            let mut receipts = store.receipts.lock().unwrap();
            receipts.insert(
                authority_id_str.clone(),
                ActionReceipt {
                    _source_event_id: MessageId::new(1),
                    principal: PACE,
                    scope: test_scope(),
                    issued: Instant::now() - Duration::from_secs(7200),
                    expires: Instant::now() - Duration::from_secs(3600),
                    _verifier: VerifierKind::DioneGateway,
                },
            );
        }

        let r = store.verify(&VerifyRequest {
            authority_id: AuthorityId(authority_id_str),
            scope: test_scope(),
            invocation_id: InvocationId::from_harness("inv_007"),
        });
        assert_eq!(r.verdict, GateVerdict::Deny);
        assert_eq!(r.reason, "authority expired");
    }

    // ── Case 8: Wrong target, DENY ──────────────────────────────────────────

    #[test]
    fn case8_wrong_target_deny() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);

        let result = store.verify(&VerifyRequest {
            authority_id,
            scope: ReactScope {
                channel: ChannelId::new(100),
                target_msg: MessageId::new(456),
                emoji: "🎯".into(),
            },
            invocation_id: InvocationId::from_harness("inv_008"),
        });

        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, "target mismatch");
    }

    // ── Case 9: Wrong channel, DENY ─────────────────────────────────────────

    #[test]
    fn case9_wrong_channel_deny() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);

        let result = store.verify(&VerifyRequest {
            authority_id,
            scope: ReactScope {
                channel: ChannelId::new(999),
                target_msg: MessageId::new(123),
                emoji: "🎯".into(),
            },
            invocation_id: InvocationId::from_harness("inv_009"),
        });

        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, "channel mismatch");
    }

    // ── Case 10: Wrong emoji, DENY ──────────────────────────────────────────

    #[test]
    fn case10_wrong_emoji_deny() {
        let store = ReceiptStore::new();
        let authority_id = mint_test_receipt(&store);

        let result = store.verify(&VerifyRequest {
            authority_id,
            scope: ReactScope {
                channel: ChannelId::new(100),
                target_msg: MessageId::new(123),
                emoji: "👍".into(),
            },
            invocation_id: InvocationId::from_harness("inv_010"),
        });

        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, "emoji mismatch");
    }

    // ── Case 11: No authority cited, DENY ───────────────────────────────────

    #[test]
    fn case11_no_authority_deny() {
        let store = ReceiptStore::new();

        let result = store.verify(&VerifyRequest {
            authority_id: AuthorityId(String::new()),
            scope: test_scope(),
            invocation_id: InvocationId::from_harness("inv_011"),
        });

        assert_eq!(result.verdict, GateVerdict::Deny);
        assert_eq!(result.reason, "authority_id not found");
    }

    // ── Authority ID uniqueness ─────────────────────────────────────────────

    #[test]
    fn authority_ids_are_unique() {
        let store = ReceiptStore::new();
        let mut ids = std::collections::HashSet::new();
        for _ in 0..1000 {
            let id = store
                .mint(&test_source_event(), test_scope(), allow_all)
                .expect("mint should succeed");
            assert!(ids.insert(id.as_str().to_string()), "collision detected");
        }
    }

    // ── Parser tests ────────────────────────────────────────────────────────

    #[test]
    fn parse_valid_react() {
        let cmd = parse_structured_command("/react 🎯 123");
        assert!(cmd.is_some());
        let cmd = cmd.unwrap();
        assert_eq!(cmd.emoji, "🎯");
        assert_eq!(cmd.target_msg, MessageId::new(123));
    }

    #[test]
    fn parse_not_a_command() {
        assert!(parse_structured_command("hello world").is_none());
        assert!(parse_structured_command("react 🎯 123").is_none());
        assert!(parse_structured_command("/reply hello").is_none());
    }

    #[test]
    fn parse_missing_parts() {
        assert!(parse_structured_command("/react").is_none());
        assert!(parse_structured_command("/react 🎯").is_none());
        assert!(parse_structured_command("/react 🎯 notanumber").is_none());
    }

    #[test]
    fn parse_rejects_extra_tokens() {
        assert!(parse_structured_command("/react 🎯 extra 123").is_none());
    }

    #[test]
    fn parse_rejects_newlines() {
        assert!(parse_structured_command("/react 🎯\n123").is_none());
    }

    #[test]
    fn parse_rejects_zero_message_id() {
        assert!(parse_structured_command("/react 🎯 0").is_none());
    }

    // ── GC test ─────────────────────────────────────────────────────────────

    #[test]
    fn gc_removes_expired_receipts_and_stale_invocations() {
        let store = ReceiptStore::new();

        {
            let mut receipts = store.receipts.lock().unwrap();
            receipts.insert(
                "expired_one".into(),
                ActionReceipt {
                    _source_event_id: MessageId::new(1),
                    principal: PACE,
                    scope: test_scope(),
                    issued: Instant::now() - Duration::from_secs(7200),
                    expires: Instant::now() - Duration::from_secs(3600),
                    _verifier: VerifierKind::DioneGateway,
                },
            );
        }

        {
            let mut claimed = store.claimed_invocations.lock().unwrap();
            claimed.insert(
                "stale_inv".into(),
                Instant::now() - Duration::from_secs(7200),
            );
        }

        store.gc_expired();

        let receipts = store.receipts.lock().unwrap();
        assert!(!receipts.contains_key("expired_one"));
        drop(receipts);

        let claimed = store.claimed_invocations.lock().unwrap();
        assert!(!claimed.contains_key("stale_inv"));
    }
}
