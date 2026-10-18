//! Typed failures from process composition and offline commands.
use crate::{
    config::StartupConfigError,
    harness::HarnessSessionError,
    mcp::{RegistrationError, ToolNameError, server::ServerConstructionError},
    pre_send::PipelineError,
    teams::{edge::TeamsEdgeError, runtime::TeamsRuntimeError},
};
use camino::Utf8PathBuf;
use std::{env::VarError, io, net::AddrParseError};
use thiserror::Error;
use tokio::task::JoinError;
use tracing_subscriber::{filter::ParseError, util::TryInitError};

/// A failure while preparing, running, or draining the application.
#[derive(Debug, Error)]
#[error(transparent)]
pub struct StartupError {
    /// The operation and its original failure.
    pub kind: StartupErrorKind,
}
impl From<StartupErrorKind> for StartupError {
    fn from(kind: StartupErrorKind) -> Self {
        Self { kind }
    }
}
impl From<HarnessSessionError> for StartupError {
    fn from(source: HarnessSessionError) -> Self {
        StartupErrorKind::Harness(source).into()
    }
}

/// Exhaustive startup and offline-command failures.
#[derive(Debug, Error)]
pub enum StartupErrorKind {
    /// A configured Teams identity violates its wire constraints.
    #[error("invalid Teams configuration identity")]
    TeamsIdentity(#[source] crate::teams::IdentityError),
    /// The configured module identity is invalid.
    #[error(transparent)]
    ModuleIdentity(crate::core::IdentityError),
    /// A module supplied an invalid MCP tool definition.
    #[error("invalid module tool definition")]
    ModuleToolDefinition(#[source] ToolNameError),
    /// A module tool collided with another registered or built-in tool.
    #[error("failed to register module tools")]
    ModuleToolRegistration(#[source] RegistrationError),
    /// The MCP server could not build its owner registrations.
    #[error("failed to construct MCP server")]
    McpServer(#[source] ServerConstructionError),
    /// The fallback CLI log filter is invalid.
    #[error("invalid log filter")]
    LogFilter(#[source] ParseError),
    /// The log file could not be opened.
    #[error("failed to open log file: {path}")]
    LogFile {
        /// The requested log path.
        path: Utf8PathBuf,
        /// The filesystem failure.
        #[source]
        source: io::Error,
    },
    /// The initial channel filter could not be built.
    #[error("failed to build initial channel filter")]
    ChannelFilter(#[source] ParseError),
    /// The process tracing subscriber could not be installed.
    #[error("failed to initialize tracing")]
    Tracing(#[source] TryInitError),
    /// Configuration could not be loaded or recovered.
    #[error("failed to load config at startup")]
    Config(#[source] StartupConfigError),
    /// The configured Observe pipeline could not be constructed.
    #[error("failed to configure Observe pre-send pipeline")]
    Observe(#[source] PipelineError),
    /// The mute store's existing loader returned a failure.
    #[error("failed to load guild mute store")]
    Mute(#[source] Box<dyn std::error::Error + Send + Sync>),
    /// Global mute state was unavailable after initialization.
    #[error("mute store unavailable after initialization")]
    MuteUnavailable,
    /// No Discord token was configured.
    #[error("no Discord bot token found; set DISCORD_BOT_TOKEN or token in config.toml")]
    DiscordToken,
    /// Harness preparation, startup, or shutdown failed.
    #[error(transparent)]
    Harness(HarnessSessionError),
    /// Teams was enabled without an explicit conversation scope.
    #[error(
        "teams.allowed_conversations must contain at least one native conversation ID when TEAMS_APP_ID is set"
    )]
    TeamsConversationsRequired,
    /// Teams delivery to OMP needs the private saved-session control channel.
    #[error("--resident-control-socket is required for Teams in --mode oh-my-pi")]
    OmpControlRequired,
    /// The Teams tenant variable was unavailable.
    #[error("TEAMS_TENANT_ID is required when TEAMS_APP_ID is set")]
    TeamsTenant(#[source] VarError),
    /// The Teams client secret variable was unavailable.
    #[error("TEAMS_CLIENT_SECRET is required when TEAMS_APP_ID is set")]
    TeamsSecret(#[source] SecretEnvironmentError),
    /// The Teams service host variable was unavailable.
    #[error("TEAMS_ALLOWED_SERVICE_HOST is required when TEAMS_APP_ID is set")]
    TeamsHost(#[source] VarError),
    /// The Teams listener address was invalid.
    #[error("TEAMS_LISTEN_ADDR is invalid")]
    TeamsListen(#[source] AddrParseError),
    /// The restricted Teams HTTP client could not be built.
    #[error("failed to build Teams HTTP client")]
    TeamsHttp(#[source] reqwest::Error),
    /// Teams authentication or admission configuration was invalid.
    #[error(transparent)]
    TeamsEdge(TeamsEdgeError),
    /// Durable Teams reply authority could not be restored.
    #[error("failed to restore Teams reply authority")]
    TeamsRestore(#[source] TeamsRuntimeError),
    /// The Discord client could not be constructed.
    #[error("failed to build Discord client")]
    Discord(#[source] Box<serenity::Error>),
    /// The SIGTERM handler could not be registered.
    #[error("failed to install SIGTERM handler")]
    Signal(#[source] io::Error),
    /// The Teams ingress task failed to join.
    #[error("Teams ingress task panicked during drain")]
    TeamsDrain(#[source] JoinError),
    /// The resident control task failed to join.
    #[error("resident control task panicked during drain")]
    ResidentDrain(#[source] JoinError),
    /// The MCP task failed to join.
    #[error("MCP server task panicked during drain")]
    McpDrain(#[source] JoinError),
    /// The attention command file could not be opened.
    #[error("failed to open attention command")]
    AttentionOpen(#[source] io::Error),
    /// The attention command input could not be read.
    #[error("failed to read attention command")]
    AttentionRead(#[source] io::Error),
    /// The bounded command input exceeded its limit.
    #[error("attention command exceeds one MiB")]
    AttentionTooLarge,
    /// The attention command did not match its JSON grammar.
    #[error("invalid attention command")]
    AttentionParse(#[source] serde_json::Error),
    /// A source-validating command lacked its Discord token.
    #[error("source validation requires a Discord bot token")]
    ValidationToken,
    /// The attention owner rejected exclusive store access.
    #[error("{0}")]
    AttentionStore(String),
    /// The attention owner rejected command execution.
    #[error("{0}")]
    AttentionExecute(String),
    /// The attention response could not be serialized.
    #[error(transparent)]
    AttentionSerialize(serde_json::Error),
}

/// A secret environment variable failed without retaining its value.
#[derive(Debug, Error, PartialEq, Eq)]
pub enum SecretEnvironmentError {
    /// The variable was not present.
    #[error("environment variable not found")]
    Missing,
    /// The variable contained a value that was not Unicode.
    #[error("environment variable contains non-Unicode data")]
    NonUnicode,
}
impl From<VarError> for SecretEnvironmentError {
    fn from(error: VarError) -> Self {
        match error {
            VarError::NotPresent => Self::Missing,
            VarError::NotUnicode(_) => Self::NonUnicode,
        }
    }
}
