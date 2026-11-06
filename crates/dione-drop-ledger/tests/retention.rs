use dione_drop_ledger::{DropLedger, SCOPE_CAPACITY, global};
use serenity::model::id::{ChannelId, MessageId};

#[test]
fn literal_capacity_cutoff_duplicate_order_and_neighbor_scope_isolation() {
    let ledger = DropLedger::new();
    let scope = ChannelId::new(20);
    let neighbor = ChannelId::new(21);
    assert_eq!(SCOPE_CAPACITY, 1024);
    ledger.record(neighbor, MessageId::new(1));
    for raw in 1..=1024 {
        ledger.record(scope, MessageId::new(raw));
    }
    for raw in 1..=1024 {
        assert!(ledger.contains(scope, MessageId::new(raw)));
    }
    ledger.record(scope, MessageId::new(1));
    ledger.record(scope, MessageId::new(1025));
    assert!(
        !ledger.contains(scope, MessageId::new(1)),
        "duplicate must not refresh oldest"
    );
    assert!(ledger.contains(scope, MessageId::new(2)));
    ledger.record(scope, MessageId::new(1026));
    assert!(!ledger.contains(scope, MessageId::new(2)));
    for raw in 3..=1026 {
        assert!(
            ledger.contains(scope, MessageId::new(raw)),
            "retained {raw}"
        );
    }
    assert!(ledger.contains(neighbor, MessageId::new(1)));
    assert!(!ledger.contains(neighbor, MessageId::new(1026)));
    assert!(!ledger.reply_inherits_drop(scope, None));
    assert!(!ledger.reply_inherits_drop(scope, Some(MessageId::new(1))));
    assert!(ledger.reply_inherits_drop(scope, Some(MessageId::new(1026))));
    for raw in 30..230 {
        ledger.record(ChannelId::new(raw), MessageId::new(raw));
    }
    assert!(ledger.contains(neighbor, MessageId::new(1)));
    assert!(ledger.contains(scope, MessageId::new(3)));
}

#[test]
fn process_global_is_one_shared_ledger_and_preserves_scoped_hops() {
    let first = global();
    let second = global();
    assert!(std::ptr::eq(first, second));
    let scope = ChannelId::new(8_765_432);
    let other = ChannelId::new(8_765_433);
    let root = MessageId::new(9_876_541);
    let hop = MessageId::new(9_876_542);
    assert!(!first.reply_inherits_drop(scope, Some(root)));
    first.record(scope, root);
    assert!(second.reply_inherits_drop(scope, Some(root)));
    assert!(!second.reply_inherits_drop(other, Some(root)));
    second.record(scope, hop);
    assert!(first.reply_inherits_drop(scope, Some(hop)));
    assert!(!first.reply_inherits_drop(scope, None));
}
