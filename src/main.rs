use camino::Utf8PathBuf;
use clap::Parser;
use color_eyre::eyre::{Result, WrapErr};
use dione::{
    codex::{CodexEventQueue, TransportMode},
    discord::events::{Handler, NotificationEvent},
    mcp::server::DioneServer,
    state::SharedState,
    tracing_channel::{TraceLevelController, TracingChannelLayer},
};
use std::{
    sync::{Arc, atomic::AtomicU64},
    time::Duration,
};
use tokio::{
    sync::{RwLock, mpsc},
    time::interval,
};
use tokio_util::sync::CancellationToken;
use tracing_subscriber::{EnvFilter, prelude::*, reload};

/// Discord MCP channel server for Claude Code and Codex.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    /// Tracing filter level (e.g. "info", "debug", "dione=debug,serenity=warn")
    #[arg(long, default_value = "dione=info")]
    log_level: String,

    /// Path to config file (overrides default <state_dir>/config.toml)
    #[arg(long)]
    config: Option<Utf8PathBuf>,

    /// Agent harness transport for inbound Discord events
    #[arg(long, value_enum, default_value_t = TransportMode::ClaudeCode)]
    mode: TransportMode,
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    if let Some(config_path) = cli.config {
        dione::config::set_config_path(config_path);
    }

    // Build a reloadable EnvFilter so trace level can be changed at runtime via MCP tool.
    // Respect RUST_LOG if set, otherwise use the CLI flag.
    let stderr_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        EnvFilter::try_new(&cli.log_level).expect("--log-level validated by clap")
    });
    let (stderr_filter_layer, stderr_reload_handle) = reload::Layer::new(stderr_filter);

    // Stderr logging layer — stdout is reserved for MCP transport.
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_filter(stderr_filter_layer);

    // Channel-forwarding layer — starts disabled (off). When set_trace_level is called,
    // events above the threshold are forwarded as channel notifications with type="trace".
    let (channel_layer, channel_event_rx) = TracingChannelLayer::new();
    let channel_filter =
        EnvFilter::try_new("off").wrap_err("failed to build initial channel filter")?;
    let (channel_filter_layer, channel_reload_handle) = reload::Layer::new(channel_filter);

    tracing_subscriber::registry()
        .with(stderr_layer)
        .with(channel_layer.with_filter(channel_filter_layer))
        .init();

    let state_dir = dione::config::state_dir();
    let config = dione::config::reload_config(&state_dir).0;
    tracing::info!(
        ?state_dir,
        dm_policy = ?config.access.dm_policy,
        "dione starting"
    );

    let token = dione::config::resolve_token(&config).ok_or_else(|| {
        color_eyre::eyre::eyre!(
            "no Discord bot token found; set DISCORD_BOT_TOKEN or token in config.toml"
        )
    })?;

    let cancel = CancellationToken::new();
    let state = Arc::new(RwLock::new(SharedState::new()));
    let queue = Arc::new(tokio::sync::Mutex::new(dione::queue::AccessQueue::load(
        &state_dir,
    )));

    // Discord → MCP event channel.
    let (event_tx, event_rx) = mpsc::channel::<NotificationEvent>(256);

    // Config file watcher — reloads ArcSwap cache on config.toml changes.
    dione::config_watcher::spawn(state_dir.clone(), event_tx.clone(), cancel.clone());

    // MCP notification channel (for server-initiated writes, currently unused beyond event_rx).
    let (notif_tx, _notif_rx) = mpsc::channel::<serde_json::Value>(64);

    // Codex owns one durable pull queue. The lifetime file lock prevents two
    // stdio-spawned Dione processes from corrupting the same inbox.
    let codex_queue = if cli.mode == TransportMode::Codex {
        Some(CodexEventQueue::load(&state_dir).wrap_err("failed to open Codex event queue")?)
    } else {
        None
    };

    // MCP → Discord gateway command channel (for presence updates, etc.).
    let (discord_cmd_tx, discord_cmd_rx) =
        mpsc::channel::<dione::mcp::tools::bot_state::DiscordCommand>(16);

    // Build Discord event handler.
    let handler = Handler {
        state: state.clone(),
        queue: queue.clone(),
        tx: event_tx.clone(),
        state_dir: state_dir.clone(),
        bot_user_id: AtomicU64::new(0),
        discord_cmd_rx: tokio::sync::Mutex::new(Some(discord_cmd_rx)),
    };

    let mut discord_client = dione::discord::client::build_client(&token, handler)
        .await
        .wrap_err("failed to build Discord client")?;

    let http = discord_client.http.clone();

    // Build MCP server.
    let trace_controller = TraceLevelController::new(stderr_reload_handle, channel_reload_handle);
    let server = DioneServer {
        state: state.clone(),
        queue: queue.clone(),
        http,
        state_dir: state_dir.clone(),
        notification_tx: notif_tx,
        discord_cmd_tx: Some(discord_cmd_tx),
        trace_controller,
        mode: cli.mode,
        codex_queue,
    };

    // Spawn the tracing-channel forwarder: converts tracing events into NotificationEvents.
    let trace_event_tx = event_tx.clone();
    tokio::spawn(async move {
        let mut rx = channel_event_rx;
        while let Some(trace_event) = rx.recv().await {
            if trace_event_tx.send(trace_event).await.is_err() {
                break;
            }
        }
    });

    // Spawn background pruning task.
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

    // Spawn Discord client.
    let cancel_discord = cancel.clone();
    let discord_handle = tokio::spawn(async move {
        if let Err(e) = discord_client.start().await {
            tracing::error!(error = ?e, "discord client exited with error");
            cancel_discord.cancel();
        }
    });

    // Spawn MCP server (owns stdin/stdout).
    let cancel_mcp = cancel.clone();
    let mcp_handle = tokio::spawn(async move {
        if let Err(e) = dione::mcp::server::run(server, event_rx, cancel_mcp).await {
            tracing::error!(error = ?e, "MCP server exited with error");
        }
    });

    // Wait for a shutdown signal.
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            tracing::info!("SIGINT received, shutting down");
        }
        _ = cancel.cancelled() => {
            tracing::info!("shutdown requested by internal cancellation");
        }
    }

    cancel.cancel();

    // Allow up to 2 seconds for tasks to wind down.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        discord_handle.abort();
        let _ = mcp_handle.await;
    })
    .await;

    tracing::info!("dione stopped");
    Ok(())
}
