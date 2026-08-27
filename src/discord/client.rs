use serenity::{client::Client, model::gateway::GatewayIntents};

/// Builds a serenity Discord [`Client`] with the standard intents and the given event handler.
// `allow` rather than `expect`: the lint fires only on Rust 1.98+, so an expectation
// would itself warn as unfulfilled on older toolchains (MSRV 1.95). `serenity::Error`'s
// size is upstream's; boxing it would tax the single startup call site for nothing.
#[allow(clippy::result_large_err)]
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
