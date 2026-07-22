use std::collections::{HashMap, HashSet};
use std::fmt;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serenity::model::id::{ChannelId, MessageId, UserId};

const DEFAULT_TTL: Duration = Duration::from_secs(3600);

// ── Types ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum AuthorizedAction {
    React,
    Reply,
    Delete,
}

impl fmt::Display for AuthorizedAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::React => f.write_str("react"),
            Self::Reply => f.write_str("reply"),
            Self::Delete => f.write_str("delete"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActionScope {
    pub channel: ChannelId,
    pub target_msg: MessageId,
    pub emoji: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ActionReceipt {
    pub source_event_id: MessageId,
    pub principal: UserId,
    pub action: AuthorizedAction,
    pub scope: ActionScope,
    pub issued: Instant,
    pub expires: Instant,
    pub authority_id: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
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

#[derive(Debug, Clone)]
pub struct VerifyRequest {
    pub authority_id: String,
    pub action: AuthorizedAction,
    pub scope: ActionScope,
    pub invocation_id: String,
}

// ── Store ────────────────────────────────────────────────────────────────────

pub struct ReceiptStore {
    receipts: Mutex<HashMap<String, ActionReceipt>>,
    claimed_invocations: Mutex<HashSet<String>>,
}

impl Default for ReceiptStore {
    fn default() -> Self {
        Self {
            receipts: Mutex::new(HashMap::new()),
            claimed_invocations: Mutex::new(HashSet::new()),
        }
    }
}

impl ReceiptStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn mint(
        &self,
        source_event_id: MessageId,
        principal: UserId,
        action: AuthorizedAction,
        scope: ActionScope,
    ) -> String {
        let now = Instant::now();
        let authority_id = generate_authority_id();
        let receipt = ActionReceipt {
            source_event_id,
            principal,
            action,
            scope,
            issued: now,
            expires: now + DEFAULT_TTL,
            authority_id: authority_id.clone(),
        };

        let mut store = self.receipts.lock().expect("receipt store poisoned");
        store.insert(authority_id.clone(), receipt);
        authority_id
    }

    pub fn verify(&self, request: &VerifyRequest) -> GateResult {
        let store = self.receipts.lock().expect("receipt store poisoned");

        let receipt = match store.get(&request.authority_id) {
            Some(r) => r,
            None => {
                return GateResult {
                    verdict: GateVerdict::Deny,
                    reason: "authority_id not found".into(),
                    authority_id: request.authority_id.clone(),
                    invocation_id: Some(request.invocation_id.clone()),
                };
            }
        };

        if receipt.action != request.action {
            return GateResult {
                verdict: GateVerdict::Deny,
                reason: format!(
                    "action mismatch: stored={}, requested={}",
                    receipt.action, request.action,
                ),
                authority_id: request.authority_id.clone(),
                invocation_id: Some(request.invocation_id.clone()),
            };
        }

        if receipt.scope != request.scope {
            let reason = if receipt.scope.channel != request.scope.channel {
                "channel mismatch"
            } else if receipt.scope.target_msg != request.scope.target_msg {
                "target mismatch"
            } else {
                "emoji mismatch"
            };
            return GateResult {
                verdict: GateVerdict::Deny,
                reason: reason.into(),
                authority_id: request.authority_id.clone(),
                invocation_id: Some(request.invocation_id.clone()),
            };
        }

        if Instant::now() >= receipt.expires {
            return GateResult {
                verdict: GateVerdict::Deny,
                reason: "authority expired".into(),
                authority_id: request.authority_id.clone(),
                invocation_id: Some(request.invocation_id.clone()),
            };
        }

        drop(store);

        let mut claimed = self
            .claimed_invocations
            .lock()
            .expect("claimed set poisoned");
        if !claimed.insert(request.invocation_id.clone()) {
            return GateResult {
                verdict: GateVerdict::Deny,
                reason: "duplicate invocation".into(),
                authority_id: request.authority_id.clone(),
                invocation_id: Some(request.invocation_id.clone()),
            };
        }

        GateResult {
            verdict: GateVerdict::Allow,
            reason: "all predicates passed".into(),
            authority_id: request.authority_id.clone(),
            invocation_id: Some(request.invocation_id.clone()),
        }
    }

    pub fn gc_expired(&self) {
        let now = Instant::now();
        let mut store = self.receipts.lock().expect("receipt store poisoned");
        store.retain(|_, receipt| receipt.expires > now);
    }
}

// ── Command parser ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParsedCommand {
    pub action: AuthorizedAction,
    pub emoji: String,
    pub target_msg: MessageId,
}

pub fn parse_structured_command(content: &str) -> Option<ParsedCommand> {
    let content = content.trim();
    let rest = content.strip_prefix("/react ")?;
    let rest = rest.trim();

    let idx = rest.rfind(' ')?;
    let emoji = rest[..idx].trim();
    let msg_part = rest[idx + 1..].trim();

    let msg_id: u64 = msg_part.parse().ok()?;
    if emoji.is_empty() {
        return None;
    }

    Some(ParsedCommand {
        action: AuthorizedAction::React,
        emoji: emoji.to_string(),
        target_msg: MessageId::new(msg_id),
    })
}

// ── Internals ────────────────────────────────────────────────────────────────

fn generate_authority_id() -> String {
    use std::time::SystemTime;

    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("time went backwards")
        .as_nanos();
    let hash = nanos
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    format!(
        "{}@{}",
        &format!("{hash:x}")[..4],
        &format!("{:x}", hash.rotate_right(32))[..3],
    )
}

#[cfg(test)]
mod tests;
