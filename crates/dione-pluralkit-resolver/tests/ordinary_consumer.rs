use dione_pluralkit_resolver::{
    LookupContext, MessageBinding, PkResolveError, PkResolver, PkResolverConfig, VerifiedPkFacts,
};
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};

#[test]
fn ordinary_lookup_metadata_and_failures_preserve_the_public_contract() {
    let binding = MessageBinding {
        message_id: MessageId::new(5108),
        channel_id: ChannelId::new(500),
        context: LookupContext::Guild(GuildId::new(1)),
    };
    assert_eq!(binding.message_id.get(), 5108);
    assert_eq!(binding.channel_id.get(), 500);
    assert_eq!(binding.context, LookupContext::Guild(GuildId::new(1)));
    let facts = VerifiedPkFacts::Represented {
        discord_user_id: UserId::new(42),
        system_id: None,
        member_id: None,
    };
    assert!(
        matches!(facts, VerifiedPkFacts::Represented { discord_user_id, .. } if discord_user_id.get() == 42)
    );
    assert_eq!(
        PkResolveError::BindingMismatch {
            dimension: "channel_id",
            expected: 500,
            actual: 501,
        }
        .to_string(),
        "PK response channel_id binding mismatch: expected 500, got 501"
    );
    let error = PkResolver::new(PkResolverConfig {
        max_concurrent: 0,
        ..PkResolverConfig::default()
    })
    .err()
    .expect("zero concurrency is invalid");
    assert_eq!(error, "max_concurrent must be > 0");
}
