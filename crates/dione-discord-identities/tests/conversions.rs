use dione_discord_identities::Snowflake;
use serenity::model::id::{ChannelId, GuildId, MessageId, UserId};

#[test]
fn zero_is_rejected_and_one_and_max_survive_all_sdk_conversions() {
    assert!(Snowflake::new(0).is_none());
    for expected in [1, u64::MAX] {
        let input = Snowflake::new(expected).unwrap();
        let channel: ChannelId = input.channel();
        let message: MessageId = input.message();
        let user: UserId = input.user();
        let guild: GuildId = input.guild();
        assert_eq!(channel.get(), expected);
        assert_eq!(message.get(), expected);
        assert_eq!(user.get(), expected);
        assert_eq!(guild.get(), expected);
    }
}
