//! Process startup, offline commands, and ordered shutdown.
mod error;
use crate::{
    attention::control::AttentionCommand,
    discord::events::{Handler, NotificationEvent},
    harness::{
        AttentionPolicy, CodexSessionOptions, HarnessSelection, HarnessSession, HarnessSessionError,
    },
    mcp::server::DioneServer,
    state::SharedState,
    teams::{
        AdmissionPolicy, ChannelPolicy,
        edge::{ClientSecretTokenProvider, TeamsEdge},
        runtime::{TeamsReplyAuthority, TeamsResidentBridge},
    },
    tracing_channel::{TraceLevelController, TracingChannelLayer},
};
use camino::{Utf8Path, Utf8PathBuf};
use clap::Parser;
pub use error::{SecretEnvironmentError, StartupError, StartupErrorKind};
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    future::Future,
    net::SocketAddr,
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};
use tokio::{
    io::AsyncReadExt,
    sync::{RwLock, mpsc},
    task::JoinHandle,
    time::interval,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, prelude::*, reload};

/// Discord MCP channel server for Claude Code, Codex, and Oh My Pi.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Tracing filter level (e.g. "info", "debug", "dione=debug,serenity=warn")
    #[arg(long, default_value = "dione=info")]
    log_level: String,

    /// Path to config file (overrides default <state_dir>/config.toml)
    #[arg(long)]
    config: Option<Utf8PathBuf>,

    /// Path to a log file for persistent structured logs
    #[arg(long, env = "DIONE_LOG_FILE")]
    log_file: Option<Utf8PathBuf>,

    /// Agent harness transport for inbound Discord events
    #[arg(long, value_enum, default_value_t = HarnessSelection::ClaudeCode)]
    mode: HarnessSelection,

    /// Codex app-server Unix socket (defaults to the managed daemon socket)
    #[arg(long)]
    codex_app_server_socket: Option<Utf8PathBuf>,

    /// Exact Codex thread to receive Discord events (defaults to CODEX_THREAD_ID)
    #[arg(long)]
    codex_thread_id: Option<crate::codex::CodexThreadId>,

    /// Operator-owned Unix socket for the minimum resident queue/publication API
    #[arg(long)]
    resident_control_socket: Option<Utf8PathBuf>,

    /// Execute an attention JSON command file (or - for stdin) without starting the gateway.
    /// Metadata commands require exclusive store ownership; use MCP while the daemon runs.
    #[arg(long)]
    attention_command: Option<Utf8PathBuf>,
}

struct PreparedTeams {
    bridge: Arc<TeamsResidentBridge<ClientSecretTokenProvider>>,
    listen: SocketAddr,
}

/// Runs the application once after the runtime and error reporter are installed.
///
/// This initializes process globals and is not a reentrant entry point.
pub async fn run() -> Result<(), StartupError> {
    let cli = Cli::parse();

    if let Some(config_path) = cli.config {
        crate::config::set_config_path(config_path);
    }

    let stderr_filter = log_filter(&cli.log_level)?;
    let (stderr_filter_layer, stderr_reload_handle) = reload::Layer::new(stderr_filter);

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter_layer);

    let file_layer = if let Some(ref log_file_path) = cli.log_file {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_file_path.as_std_path())
            .map_err(|source| StartupErrorKind::LogFile {
                path: log_file_path.clone(),
                source,
            })?;
        let file_filter = log_filter(&cli.log_level)?;
        Some(
            tracing_subscriber::fmt::layer()
                .with_writer(std::sync::Mutex::new(file))
                .with_ansi(false)
                .with_filter(file_filter),
        )
    } else {
        None
    };

    let (channel_layer, channel_event_rx) = TracingChannelLayer::new();
    let channel_filter = EnvFilter::try_new("off").map_err(StartupErrorKind::ChannelFilter)?;
    let (channel_filter_layer, channel_reload_handle) = reload::Layer::new(channel_filter);

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .with(channel_layer.with_filter(channel_filter_layer))
        .try_init()
        .map_err(StartupErrorKind::Tracing)?;

    let state_dir = crate::config::state_dir();
    let (config, config_warning) = crate::config::ConfigRuntime::new(state_dir.clone())
        .startup_load(crate::config::NoLkgPolicy::default())
        .await
        .map_err(StartupErrorKind::Config)?;
    if let Some(warning) = config_warning {
        tracing::warn!(warning = %warning, "config loaded with a startup warning");
    }
    let pre_send_pipeline =
        crate::pre_send::production_pipeline().map_err(StartupErrorKind::Observe)?;
    crate::pre_send::install_pipeline(Some(pre_send_pipeline));

    let mute_store = crate::mute_store::MuteStore::load(&state_dir)
        .await
        .map_err(StartupErrorKind::Mute)?;
    crate::mute_store::init_global(mute_store);

    if let Some(path) = &cli.attention_command {
        return run_attention_command(path, &state_dir, &config, cli.mode.attention_policy()).await;
    }

    let expiry_handle = crate::mute_store::global()
        .ok_or(StartupErrorKind::MuteUnavailable)?
        .spawn_expiry_task();

    tracing::info!(
        ?state_dir,
        dm_policy = ?config.access.dm_policy,
        "dione starting"
    );

    let token = crate::config::resolve_token(&config).ok_or(StartupErrorKind::DiscordToken)?;

    let cancel = CancellationToken::new();
    let state = Arc::new(RwLock::new(SharedState::new()));
    let queue = Arc::new(tokio::sync::Mutex::new(crate::queue::AccessQueue::load(
        &state_dir,
    )));

    let (event_tx, event_rx) = mpsc::channel::<NotificationEvent>(256);

    crate::config_watcher::spawn(state_dir.clone(), event_tx.clone(), cancel.clone());

    let (notif_tx, _notif_rx) = mpsc::channel::<serde_json::Value>(64);

    let authority_module = crate::core::ModuleInstanceId::new(
        crate::core::ProviderId::new("discord").map_err(StartupErrorKind::ModuleIdentity)?,
        crate::core::ModuleSlotId::new("primary").map_err(StartupErrorKind::ModuleIdentity)?,
    );
    let teams_enabled = env::var("TEAMS_APP_ID").is_ok();
    validate_teams_control(
        cli.mode,
        teams_enabled,
        cli.resident_control_socket.as_deref(),
    )?;
    let session = cli
        .mode
        .prepare(
            &state_dir,
            CodexSessionOptions::new(
                cli.codex_app_server_socket,
                cli.codex_thread_id,
                crate::codex::LegacyDiscordScope::new(authority_module.clone()),
            ),
            teams_enabled,
        )
        .await?;
    let teams = prepare_teams(&session, &state_dir, &config.raw.teams, |key| env::var(key)).await?;

    let pronoun_service = if config.raw.pronouns.enabled {
        Some(std::sync::Arc::new(crate::pronouns::PronounService::new(
            config.pronoun_excluded.clone(),
            config.raw.pronouns.deadline_ms,
            std::time::Duration::from_secs(config.raw.pronouns.cache_ttl_seconds),
        )))
    } else {
        None
    };

    let nameplate_service = if config.raw.nameplates.enabled {
        Some(std::sync::Arc::new(
            crate::nameplates::NameplateService::new(
                &config.raw.nameplates.url,
                config.raw.nameplates.deadline_ms,
                std::time::Duration::from_secs(config.raw.nameplates.cache_ttl_seconds),
                &config.raw.nameplates.exclude_for,
            ),
        ))
    } else {
        None
    };

    let ingress_ledger = std::sync::Arc::new(crate::ingress_ledger::IngressLedger::new());

    let pk_resolver = Some(std::sync::Arc::new(
        crate::pluralkit::PkResolver::with_defaults(),
    ));

    let (shared_presence, server_presence) = crate::discord::presence::wire_shared_presence();
    let handler = Handler {
        state: state.clone(),
        queue: queue.clone(),
        tx: event_tx.clone(),
        state_dir: state_dir.clone(),
        bot_user_id: AtomicU64::new(0),
        discord_cmd_rx: tokio::sync::Mutex::new(None),
        presence: shared_presence,
        pronoun_service,
        nameplate_service,
        ingress_ledger: ingress_ledger.clone(),
        pk_resolver,
        observation: crate::discord::observation::ObservationIngress::disabled(
            authority_module.clone(),
        ),
    };

    let mut discord_client = crate::discord::client::build_client(&token, handler)
        .await
        .map_err(|source| StartupErrorKind::Discord(Box::new(source)))?;

    let http = discord_client.http.clone();

    let trace_controller = TraceLevelController::new(stderr_reload_handle, channel_reload_handle);
    let server = DioneServer::new(
        state.clone(),
        queue.clone(),
        http,
        state_dir.clone(),
        notif_tx,
        trace_controller,
        session.connection(),
        Arc::new(crate::no_rly::consent::ConsentGate::new(&state_dir)),
        ingress_ledger,
        authority_module,
        Some(event_tx.clone()),
    )
    .await
    .map_err(StartupErrorKind::McpServer)?
    .with_presence(server_presence);

    let running_session = session
        .start(&config.delivery, server.attention.clone(), cancel.clone())
        .await?;
    let teams_reply = teams.as_ref().map(|prepared| {
        let authority: Arc<dyn TeamsReplyAuthority> = prepared.bridge.clone();
        authority
    });
    let tools =
        crate::teams::mcp_tools(teams_reply).map_err(StartupErrorKind::ModuleToolDefinition)?;
    let server = Arc::new(
        server
            .with_module_tools(tools)
            .map_err(StartupErrorKind::ModuleToolRegistration)?,
    );

    let teams_handle = if let Some(prepared) = &teams {
        let bridge = prepared.bridge.clone();
        let listen = prepared.listen;
        let ingress_cancel = cancel.clone();
        let shutdown_on_failure = cancel.clone();
        Some(tokio::spawn(async move {
            if let Err(error) =
                crate::teams::runtime::run_listener(bridge, listen, ingress_cancel).await
            {
                tracing::error!(%error, "Teams resident listener failed");
                shutdown_on_failure.cancel();
            }
        }))
    } else {
        None
    };

    let resident_control_handle = if let Some(socket_path) = cli.resident_control_socket {
        let control_server = server.clone();
        let control_cancel = cancel.clone();
        Some(tokio::spawn(async move {
            if let Err(error) = crate::mcp::server::run_resident_control(
                control_server,
                socket_path,
                control_cancel.clone(),
            )
            .await
            {
                tracing::error!(%error, "resident control server failed");
                control_cancel.cancel();
            }
        }))
    } else {
        None
    };
    let stdio_exit_policy = cli
        .mode
        .stdio_exit_policy(resident_control_handle.is_some());

    let trace_event_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut rx = channel_event_rx;
        while let Some(trace_event) = rx.recv().await {
            if trace_event_tx.send(trace_event).await.is_err() {
                break;
            }
        }
    });

    let mut coordination_rx = crate::coordination::subscribe_shared();
    let coordination_tx = event_tx.clone();
    tokio::spawn(async move {
        loop {
            let event = match coordination_rx.recv().await {
                Ok(event) => event,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    tracing::warn!(skipped, "claim-once: coordination events dropped (lagged)");
                    continue;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            };
            if coordination_tx
                .send(NotificationEvent::Coordination(event))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let prune_queue = queue.clone();
    let prune_expiry = Duration::from_secs(config.access_requests.expiry_seconds);
    let cancel_prune = cancel.clone();
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(60));
        loop {
            tokio::select! {
                biased;
                _ = cancel_prune.cancelled() => break,
                _ = ticker.tick() => {
                    prune_queue.lock().await.prune_expired(prune_expiry);
                }
            }
        }
    });

    let cancel_discord = cancel.clone();
    let discord_handle = tokio::spawn(async move {
        if let Err(e) = discord_client.start().await {
            tracing::error!(error = ?e, "discord client exited with error");
            cancel_discord.cancel();
        }
    });

    let cancel_mcp = cancel.clone();
    let mcp_handle = tokio::spawn(async move {
        if let Err(e) =
            crate::mcp::server::run(server, event_rx, cancel_mcp, stdio_exit_policy).await
        {
            tracing::error!(error = ?e, "MCP server exited with error");
        }
    });

    // Wait for a shutdown signal. SIGTERM (service managers, container stop)
    // must trigger the same graceful path as SIGINT so the no_rly sweeper
    // drains pending handles into the journal before exit.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(StartupErrorKind::Signal)?;
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("SIGINT received, shutting down");
        }
        _ = sigterm.recv() => {
            tracing::info!("SIGTERM received, shutting down");
        }
        _ = cancel.cancelled() => {
            tracing::info!("shutdown requested by internal cancellation");
        }
    }

    cancel.cancel();

    expiry_handle.abort();
    discord_handle.abort();
    drain_shutdown(
        teams_handle,
        resident_control_handle,
        mcp_handle,
        async {
            if let Some(prepared) = teams {
                prepared.bridge.drain_replies().await;
            }
        },
        running_session.shutdown(),
    )
    .await?;

    tracing::info!("dione stopped");
    Ok(())
}

async fn drain_shutdown(
    teams: Option<JoinHandle<()>>,
    resident_control: Option<JoinHandle<()>>,
    mcp: JoinHandle<()>,
    replies: impl Future<Output = ()>,
    session: impl Future<Output = Result<(), HarnessSessionError>>,
) -> Result<(), StartupError> {
    let teams_result = match teams {
        Some(handle) => handle.await.map_err(StartupErrorKind::TeamsDrain),
        None => Ok(()),
    };
    let resident_result = match resident_control {
        Some(handle) => handle.await.map_err(StartupErrorKind::ResidentDrain),
        None => Ok(()),
    };
    let mcp_result = mcp.await.map_err(StartupErrorKind::McpDrain);
    replies.await;
    let session_result = session.await.map_err(StartupErrorKind::Harness);

    let mut outcome = Ok(());
    for result in [teams_result, resident_result, mcp_result, session_result] {
        if let Err(error) = result {
            if outcome.is_ok() {
                outcome = Err(error.into());
            } else {
                tracing::error!(error = ?error, "additional failure during shutdown drain");
            }
        }
    }
    outcome
}

async fn run_attention_command(
    path: &Utf8PathBuf,
    state_dir: &Utf8PathBuf,
    config: &crate::config::LoadedConfig,
    policy: AttentionPolicy,
) -> Result<(), StartupError> {
    let mut bytes = Vec::new();
    if path.as_str() == "-" {
        tokio::io::stdin()
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .await
            .map_err(StartupErrorKind::AttentionRead)?;
    } else {
        tokio::fs::File::open(path)
            .await
            .map_err(StartupErrorKind::AttentionOpen)?
            .take(1_048_577)
            .read_to_end(&mut bytes)
            .await
            .map_err(StartupErrorKind::AttentionRead)?;
    }
    if bytes.len() > 1_048_576 {
        return Err(StartupErrorKind::AttentionTooLarge.into());
    }
    let command: AttentionCommand =
        serde_json::from_slice(&bytes).map_err(StartupErrorKind::AttentionParse)?;
    let token = if matches!(
        &command,
        AttentionCommand::Status | AttentionCommand::Configure { .. }
    ) {
        String::new()
    } else {
        crate::config::resolve_token(config).ok_or(StartupErrorKind::ValidationToken)?
    };
    let runtime = Arc::new(
        crate::attention::runtime::AttentionRuntime::new(
            crate::attention::source::SourceResolver {
                http: Arc::new(serenity::http::Http::new(&token)),
                state: Arc::new(RwLock::new(SharedState::new())),
                state_dir: state_dir.clone(),
                ledger: Arc::new(crate::ingress_ledger::IngressLedger::new()),
            },
            policy == AttentionPolicy::SafeTurn,
            crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord")
                    .map_err(StartupErrorKind::ModuleIdentity)?,
                crate::core::ModuleSlotId::new("primary")
                    .map_err(StartupErrorKind::ModuleIdentity)?,
            ),
        )
        .await,
    );
    runtime
        .with_store(|_| Ok(()))
        .await
        .map_err(StartupErrorKind::AttentionStore)?;
    let result =
        crate::attention::control::execute(runtime, command, !config.access.admin_only_mutations)
            .await
            .map_err(StartupErrorKind::AttentionExecute)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&result).map_err(StartupErrorKind::AttentionSerialize)?
    );
    Ok(())
}

fn log_filter(level: &str) -> Result<EnvFilter, StartupError> {
    EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new(level))
        .map_err(|source| StartupErrorKind::LogFilter(source).into())
}

fn validate_teams_control(
    mode: HarnessSelection,
    teams_enabled: bool,
    socket: Option<&Utf8Path>,
) -> Result<(), StartupError> {
    if matches!(mode, HarnessSelection::OhMyPi) && teams_enabled && socket.is_none() {
        return Err(StartupErrorKind::OmpControlRequired.into());
    }
    Ok(())
}

async fn prepare_teams(
    session: &HarnessSession,
    state_dir: &Utf8Path,
    teams_config: &crate::config::TeamsConfig,
    environment: impl Fn(&str) -> Result<String, env::VarError>,
) -> Result<Option<PreparedTeams>, StartupError> {
    let Ok(app_id) = environment("TEAMS_APP_ID") else {
        return Ok(None);
    };
    let app_id =
        crate::teams::ApplicationId::parse(app_id).map_err(StartupErrorKind::TeamsIdentity)?;
    if teams_config.allowed_conversations.is_empty() {
        return Err(StartupErrorKind::TeamsConversationsRequired.into());
    }
    let delivery = match session.resident_admission()? {
        crate::harness::ResidentAdmission::Codex(queue) => {
            crate::composition::teams_admission::from_codex(queue)
        }
        crate::harness::ResidentAdmission::Omp(admission) => {
            crate::composition::teams_admission::from_omp(admission)
        }
    };
    let tenant_id = environment("TEAMS_TENANT_ID").map_err(StartupErrorKind::TeamsTenant)?;
    let tenant_id =
        crate::teams::TenantId::parse(tenant_id).map_err(StartupErrorKind::TeamsIdentity)?;
    let client_secret = secrecy::SecretString::from(
        environment("TEAMS_CLIENT_SECRET")
            .map_err(|source| StartupErrorKind::TeamsSecret(source.into()))?,
    );
    let allowed_host =
        environment("TEAMS_ALLOWED_SERVICE_HOST").map_err(StartupErrorKind::TeamsHost)?;
    let listen: SocketAddr = environment("TEAMS_LISTEN_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3978".to_owned())
        .parse()
        .map_err(StartupErrorKind::TeamsListen)?;
    let client = reqwest::Client::builder()
        .use_rustls_tls()
        .https_only(true)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(StartupErrorKind::TeamsHttp)?;
    let provider =
        ClientSecretTokenProvider::new(client.clone(), &tenant_id, app_id.clone(), client_secret)
            .map_err(StartupErrorKind::TeamsEdge)?;
    let edge = TeamsEdge::new(
        client,
        provider,
        AdmissionPolicy {
            app_id,
            tenant_id,
            allowed_conversations: teams_config.allowed_conversations.clone(),
            allowed_service_hosts: BTreeSet::from([allowed_host]),
            channels: BTreeMap::from([(
                "msteams".to_owned(),
                ChannelPolicy {
                    requires_key_endorsement: true,
                },
            )]),
        },
    )
    .map_err(StartupErrorKind::TeamsEdge)?;
    let bridge = Arc::new(
        TeamsResidentBridge::new_durable(edge, delivery, state_dir)
            .await
            .map_err(StartupErrorKind::TeamsRestore)?,
    );
    Ok(Some(PreparedTeams { bridge, listen }))
}

#[cfg(test)]
mod tests;
