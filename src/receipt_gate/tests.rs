use serenity::model::id::{ChannelId, MessageId, UserId};

use super::*;

fn test_scope() -> ActionScope {
    ActionScope {
        channel: ChannelId::new(100),
        target_msg: MessageId::new(123),
        emoji: Some("🎯".into()),
    }
}

fn test_store_with_receipt() -> (ReceiptStore, String) {
    let store = ReceiptStore::new();
    let authority_id = store.mint(
        MessageId::new(456),
        UserId::new(999),
        AuthorizedAction::React,
        test_scope(),
    );
    (store, authority_id)
}

// ── Case 1: Baseline — real command, full chain, ALLOW ───────────────────────

#[test]
fn case1_baseline_allow() {
    let (store, authority_id) = test_store_with_receipt();

    let result = store.verify(&VerifyRequest {
        authority_id: authority_id.clone(),
        action: AuthorizedAction::React,
        scope: test_scope(),
        invocation_id: "inv_001".into(),
    });

    assert_eq!(result.verdict, GateVerdict::Allow);
    assert_eq!(result.authority_id, authority_id);
}

// ── Case 2: Phantom — fabricated authority_id, DENY ──────────────────────────

#[test]
fn case2_phantom_deny() {
    let store = ReceiptStore::new();

    let result = store.verify(&VerifyRequest {
        authority_id: "4@k3".into(),
        action: AuthorizedAction::React,
        scope: test_scope(),
        invocation_id: "inv_002".into(),
    });

    assert_eq!(result.verdict, GateVerdict::Deny);
    assert_eq!(result.reason, "authority_id not found");
}

// ── Case 3: Scope substitution — wrong action, DENY ─────────────────────────

#[test]
fn case3_scope_substitution_wrong_action() {
    let (store, authority_id) = test_store_with_receipt();

    let result = store.verify(&VerifyRequest {
        authority_id,
        action: AuthorizedAction::Delete,
        scope: ActionScope {
            channel: ChannelId::new(100),
            target_msg: MessageId::new(789),
            emoji: None,
        },
        invocation_id: "inv_003".into(),
    });

    assert_eq!(result.verdict, GateVerdict::Deny);
    assert!(result.reason.contains("action mismatch"));
}

// ── Case 4: Replay — reuse within TTL, ALLOW ────────────────────────────────

#[test]
fn case4_replay_allow() {
    let (store, authority_id) = test_store_with_receipt();

    let r1 = store.verify(&VerifyRequest {
        authority_id: authority_id.clone(),
        action: AuthorizedAction::React,
        scope: test_scope(),
        invocation_id: "inv_004a".into(),
    });
    assert_eq!(r1.verdict, GateVerdict::Allow);

    let r2 = store.verify(&VerifyRequest {
        authority_id,
        action: AuthorizedAction::React,
        scope: test_scope(),
        invocation_id: "inv_004b".into(),
    });
    assert_eq!(r2.verdict, GateVerdict::Allow);
}

// ── Case 5: Concurrent duplicate — same invocation_id, DENY ─────────────────

#[test]
fn case5_concurrent_duplicate_deny() {
    let (store, authority_id) = test_store_with_receipt();

    let r1 = store.verify(&VerifyRequest {
        authority_id: authority_id.clone(),
        action: AuthorizedAction::React,
        scope: test_scope(),
        invocation_id: "inv_005".into(),
    });
    assert_eq!(r1.verdict, GateVerdict::Allow);

    let r2 = store.verify(&VerifyRequest {
        authority_id,
        action: AuthorizedAction::React,
        scope: test_scope(),
        invocation_id: "inv_005".into(),
    });
    assert_eq!(r2.verdict, GateVerdict::Deny);
    assert_eq!(r2.reason, "duplicate invocation");
}

// ── Case 8: Wrong target, DENY ───────────────────────────────────────────────

#[test]
fn case8_wrong_target_deny() {
    let (store, authority_id) = test_store_with_receipt();

    let result = store.verify(&VerifyRequest {
        authority_id,
        action: AuthorizedAction::React,
        scope: ActionScope {
            channel: ChannelId::new(100),
            target_msg: MessageId::new(456),
            emoji: Some("🎯".into()),
        },
        invocation_id: "inv_008".into(),
    });

    assert_eq!(result.verdict, GateVerdict::Deny);
    assert_eq!(result.reason, "target mismatch");
}

// ── Case 9: Wrong channel, DENY ─────────────────────────────────────────────

#[test]
fn case9_wrong_channel_deny() {
    let (store, authority_id) = test_store_with_receipt();

    let result = store.verify(&VerifyRequest {
        authority_id,
        action: AuthorizedAction::React,
        scope: ActionScope {
            channel: ChannelId::new(999),
            target_msg: MessageId::new(123),
            emoji: Some("🎯".into()),
        },
        invocation_id: "inv_009".into(),
    });

    assert_eq!(result.verdict, GateVerdict::Deny);
    assert_eq!(result.reason, "channel mismatch");
}

// ── Case 10: Wrong emoji, DENY ──────────────────────────────────────────────

#[test]
fn case10_wrong_emoji_deny() {
    let (store, authority_id) = test_store_with_receipt();

    let result = store.verify(&VerifyRequest {
        authority_id,
        action: AuthorizedAction::React,
        scope: ActionScope {
            channel: ChannelId::new(100),
            target_msg: MessageId::new(123),
            emoji: Some("👍".into()),
        },
        invocation_id: "inv_010".into(),
    });

    assert_eq!(result.verdict, GateVerdict::Deny);
    assert_eq!(result.reason, "emoji mismatch");
}

// ── Case 11: No authority cited (empty string), DENY ────────────────────────

#[test]
fn case11_no_authority_deny() {
    let store = ReceiptStore::new();

    let result = store.verify(&VerifyRequest {
        authority_id: "".into(),
        action: AuthorizedAction::React,
        scope: test_scope(),
        invocation_id: "inv_011".into(),
    });

    assert_eq!(result.verdict, GateVerdict::Deny);
    assert_eq!(result.reason, "authority_id not found");
}

// ── Parser tests ─────────────────────────────────────────────────────────────

#[test]
fn parse_valid_react() {
    let cmd = parse_structured_command("/react 🎯 123");
    assert!(cmd.is_some());
    let cmd = cmd.unwrap();
    assert_eq!(cmd.action, AuthorizedAction::React);
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

// ── GC test ──────────────────────────────────────────────────────────────────

#[test]
fn gc_removes_expired() {
    let store = ReceiptStore::new();

    {
        let mut receipts = store.receipts.lock().unwrap();
        receipts.insert(
            "expired_one".into(),
            ActionReceipt {
                source_event_id: MessageId::new(1),
                principal: UserId::new(1),
                action: AuthorizedAction::React,
                scope: test_scope(),
                issued: Instant::now() - Duration::from_secs(7200),
                expires: Instant::now() - Duration::from_secs(3600),
                authority_id: "expired_one".into(),
            },
        );
    }

    let r = store.verify(&VerifyRequest {
        authority_id: "expired_one".into(),
        action: AuthorizedAction::React,
        scope: test_scope(),
        invocation_id: "inv_gc".into(),
    });
    assert_eq!(r.verdict, GateVerdict::Deny);
    assert_eq!(r.reason, "authority expired");

    store.gc_expired();
    let receipts = store.receipts.lock().unwrap();
    assert!(!receipts.contains_key("expired_one"));
}
