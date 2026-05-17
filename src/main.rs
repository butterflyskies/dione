use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use color_eyre::eyre::{Result, WrapErr};
use tokio::sync::{RwLock, mpsc};
use tokio::time::interval;
use tokio_util::sync::CancellationToken;

use dione::discord::events::{Handler, NotificationEvent};
use dione::mcp::server::DioneServer;
use dione::state::SharedState;

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    // Logging must go to stderr — stdout is reserved for MCP transport.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("dione=info".parse().wrap_err("invalid directive")?),
        )
        .with_writer(std::io::stderr)
        .init();

    let state_dir = dione::config::state_dir();
    let config = dione::config::load_config(&state_dir);
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

    // MCP notification channel (for server-initiated writes, currently unused beyond event_rx).
    let (notif_tx, _notif_rx) = mpsc::channel::<serde_json::Value>(64);

    // Build Discord event handler.
    let handler = Handler {
        state: state.clone(),
        queue: queue.clone(),
        tx: event_tx,
        state_dir: state_dir.clone(),
        bot_user_id: AtomicU64::new(0),
    };

    let mut discord_client = dione::discord::client::build_client(&token, handler)
        .await
        .wrap_err("failed to build Discord client")?;

    let http = discord_client.http.clone();

    // Build MCP server.
    let server = DioneServer {
        state: state.clone(),
        queue: queue.clone(),
        http,
        state_dir: state_dir.clone(),
        notification_tx: notif_tx,
        discord_cmd_tx: None,
    };

    // Spawn background pruning task (R-39).
    // Runs every 60 seconds; prunes stale permissions and expired queue entries.
    let prune_state = state.clone();
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
                    prune_state.write().await.prune_stale_permissions();
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
