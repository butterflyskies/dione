use dione_discord_events::lifecycle_storage::{
    CreateEntry, EditOutcome, LifecycleStorage, Lookup, PayloadChange, StorageLimits,
};
use serenity::model::{
    Timestamp,
    id::{ChannelId, MessageId, UserId},
};
use std::time::Instant;

struct CallerLineage(&'static str);

#[test]
fn borrowed_transactions_preserve_non_clone_non_eq_lineage_and_literal_content() {
    let storage = LifecycleStorage::new(StorageLimits::default());
    let now = Instant::now();
    storage
        .with_locked(|locked| {
            let mut transaction = locked.transitions(now);
            let CreateEntry::Vacant(slot) = transaction.create_entry(
                MessageId::new(10),
                ChannelId::new(100),
                false,
                "original content",
                Timestamp::from_unix_timestamp(1).unwrap(),
            ) else {
                panic!("empty storage must offer an insertion slot")
            };
            let record = slot
                .insert(
                    UserId::new(300),
                    Some(ChannelId::new(99)),
                    CallerLineage("original lineage"),
                )
                .unwrap();
            assert_eq!(record.actor_id(), UserId::new(300));
            assert_eq!(record.thread_parent_id(), Some(ChannelId::new(99)));
            let entry = transaction.edit_entry(MessageId::new(10)).unwrap();
            assert_eq!(entry.record().payload().0, "original lineage");
            assert!(matches!(
                entry.apply(
                    "original content",
                    Timestamp::from_unix_timestamp(2).unwrap(),
                    PayloadChange::Replace(CallerLineage("unused lineage"))
                ),
                EditOutcome::Duplicate
            ));
            let entry = transaction.edit_entry(MessageId::new(10)).unwrap();
            let EditOutcome::Changed(record) = entry.apply(
                "changed content",
                Timestamp::from_unix_timestamp(3).unwrap(),
                PayloadChange::Keep,
            ) else {
                panic!("new content must change storage")
            };
            assert_eq!(record.payload().0, "original lineage");
        })
        .unwrap();
    storage
        .with_locked(|locked| {
            let transaction = locked.transitions(now);
            let Lookup::Active(record) = transaction.lookup(MessageId::new(10)) else {
                panic!("retained metadata")
            };
            assert_eq!(record.channel_id(), ChannelId::new(100));
            assert_eq!(record.payload().0, "original lineage");
            assert!(!record.matches_hash_hex("not a sha256 value"));
        })
        .unwrap();
}
