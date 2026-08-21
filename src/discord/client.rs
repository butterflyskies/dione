use serenity::{client::Client, model::gateway::GatewayIntents};

/// Builds a serenity Discord [`Client`] with the standard intents and the given event handler.
pub async fn build_client(
    token: &str,
    handler: crate::discord::events::Handler,
) -> serenity::Result<Client> {
    let intents = GatewayIntents::DIRECT_MESSAGES
        | GatewayIntents::DIRECT_MESSAGE_REACTIONS
        | GatewayIntents::GUILDS
        | GatewayIntents::GUILD_MESSAGES
        | GatewayIntents::MESSAGE_CONTENT
        | GatewayIntents::GUILD_MESSAGE_REACTIONS
        | GatewayIntents::GUILD_VOICE_STATES;

    Client::builder(token, intents).event_handler(handler).await
}
