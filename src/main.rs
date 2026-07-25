use camino::Utf8PathBuf;
use clap::{Parser, Subcommand};
use color_eyre::eyre::{Result, WrapErr};
use dione::{
    codex::{CodexDeliveryConfig, CodexEventQueue, TransportMode},
    discord::events::{Handler, NotificationEvent},
    lifecycle::cancel_root_when_complete,
    mcp::server::DioneServer,
    state::SharedState,
    tracing_channel::{TraceLevelController, TracingChannelLayer},
};
use std::{
    env,
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

    /// Codex app-server Unix socket (defaults to the managed daemon socket)
    #[arg(long)]
    codex_app_server_socket: Option<Utf8PathBuf>,

    /// Exact Codex thread to receive Discord events (defaults to CODEX_THREAD_ID)
    #[arg(long)]
    codex_thread_id: Option<dione::codex::CodexThreadId>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand)]
enum Command {
    /// Bind a Codex SessionStart hook payload through the running daemon
    BindCodexThread {
        /// State directory of the running Codex-mode Dione daemon
        #[arg(long)]
        state_dir: Utf8PathBuf,
    },
}

async fn cancel_root_on_task_failure<T, E>(
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<Result<T, E>>,
) -> Result<Result<T, E>, tokio::task::JoinError> {
    let result = task.await;
    if !matches!(result, Ok(Ok(_))) {
        cancel.cancel();
    }
    result
}

#[tokio::main]
async fn main() -> Result<()> {
    color_eyre::install()?;

    let cli = Cli::parse();

    if let Some(Command::BindCodexThread { state_dir }) = cli.command {
        dione::codex::run_session_start_bind_client(&state_dir, tokio::io::stdin())
            .await
            .wrap_err("failed to bind the Codex SessionStart session")?;
        return Ok(());
    }

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
    // Keep the Observe pipeline installed for the process lifetime. Each
    // message's freshly loaded config decides whether it participates, so
    // `pre_send.enabled` hot reloads in both directions without a restart.
    let pre_send_pipeline = dione::pre_send::observe_pipeline(Vec::new())
        .wrap_err("failed to configure Observe pre-send pipeline")?;
    dione::pre_send::install_pipeline(Some(pre_send_pipeline));
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
    let (codex_queue, codex_handle, codex_thread_binder, codex_control_handle) =
        if cli.mode == TransportMode::Codex {
            let codex_queue =
                CodexEventQueue::load(&state_dir).wrap_err("failed to open Codex event queue")?;
            let delivery_config = CodexDeliveryConfig::resolve(cli.codex_app_server_socket)
                .wrap_err("failed to configure Codex live delivery")?;
            let initial_thread = match cli.codex_thread_id {
                Some(thread_id) => Some(thread_id),
                None => env::var("CODEX_THREAD_ID")
                    .ok()
                    .map(|value| value.parse())
                    .transpose()
                    .wrap_err("invalid CODEX_THREAD_ID")?,
            };
            let binding_rx = codex_queue.subscribe_live_binding();
            let binder = dione::codex::CodexThreadBinder::new(codex_queue.clone());
            match initial_thread {
                Some(thread_id) => binder
                    .bind(thread_id)
                    .wrap_err("failed to set the initial Codex thread binding")?,
                None => binder.clear(),
            }
            let delivery_queue = codex_queue.clone();
            let delivery_cancel = cancel.clone();
            let handle = tokio::spawn(async move {
                let worker_cancel = delivery_cancel.clone();
                let result = cancel_root_when_complete(
                    delivery_cancel,
                    dione::codex::run_delivery_worker(
                        delivery_queue,
                        delivery_config,
                        binding_rx,
                        worker_cancel,
                    ),
                )
                .await;
                if let Err(error) = result {
                    tracing::error!(error = %error, "Codex live delivery worker exited");
                }
            });
            let control_listener =
                dione::codex::CodexBindControlListener::bind(&state_dir, binder.clone())
                    .await
                    .wrap_err("failed to start the Codex bind control listener")?;
            let control_cancel = cancel.child_token();
            let control_root_cancel = cancel.clone();
            let control_handle = tokio::spawn(async move {
                let listener_task = tokio::spawn(control_listener.run(control_cancel));
                match cancel_root_on_task_failure(control_root_cancel, listener_task).await {
                    Ok(Ok(())) => {}
                    Ok(Err(error)) => {
                        tracing::error!(error = %error, "Codex bind control listener exited");
                    }
                    Err(error) => {
                        tracing::error!(error = %error, "Codex bind control listener panicked");
                    }
                }
            });
            (
                Some(codex_queue),
                Some(handle),
                Some(binder),
                Some(control_handle),
            )
        } else {
            (None, None, None, None)
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
        codex_thread_binder,
    };
    let codex_shutdown_binder = server.codex_thread_binder.clone();

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
        let mcp_cancel = cancel_mcp.clone();
        let result = cancel_root_when_complete(
            cancel_mcp,
            dione::mcp::server::run(server, event_rx, mcp_cancel),
        )
        .await;
        if let Err(e) = result {
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

    if let Some(binder) = codex_shutdown_binder.as_ref() {
        binder.begin_shutdown();
    }
    cancel.cancel();

    // Stop accepting and drain bounded in-flight control requests after the
    // fence, so every late request observes the stopping phase.
    if let Some(codex_control_handle) = codex_control_handle {
        let _ = codex_control_handle.await;
    }

    // Allow up to 2 seconds for tasks to wind down.
    let _ = tokio::time::timeout(Duration::from_secs(2), async {
        discord_handle.abort();
        let _ = mcp_handle.await;
        if let Some(codex_handle) = codex_handle {
            let _ = codex_handle.await;
        }
    })
    .await;

    tracing::info!("dione stopped");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn delivery_config(state_dir: &camino::Utf8Path) -> CodexDeliveryConfig {
        CodexDeliveryConfig {
            socket_path: state_dir.join("unused-app-server.sock"),
            request_timeout: Duration::from_millis(10),
        }
    }

    #[test]
    fn bind_codex_thread_requires_explicit_state_directory() {
        let error = match Cli::try_parse_from(["dione", "bind-codex-thread"]) {
            Ok(_) => panic!("state directory must be required"),
            Err(error) => error,
        };

        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn bind_codex_thread_parses_explicit_state_directory() {
        let cli = Cli::try_parse_from([
            "dione",
            "bind-codex-thread",
            "--state-dir",
            "/srv/dione-state",
        ])
        .expect("valid bind client CLI");

        assert!(matches!(
            cli.command,
            Some(Command::BindCodexThread { state_dir })
                if state_dir == "/srv/dione-state"
        ));
    }

    #[tokio::test]
    async fn clean_task_completion_cancels_root_lifecycle() {
        let cancel = CancellationToken::new();

        let result = cancel_root_when_complete(cancel.clone(), async { "clean EOF" }).await;

        assert_eq!(result, "clean EOF");
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn directly_awaited_task_panic_cancels_root_lifecycle() {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(cancel_root_when_complete(cancel.clone(), async {
            panic!("directly awaited worker panic")
        }));

        assert!(task.await.expect_err("worker must panic").is_panic());
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn recoverable_registration_io_does_not_cancel_root_lifecycle() {
        let dir = TempDir::new().unwrap();
        let state_dir = Utf8PathBuf::from_path_buf(dir.path().join("state")).unwrap();
        let queue = CodexEventQueue::load(&state_dir).unwrap();
        dione::codex::CodexThreadBinder::new(queue.clone())
            .bind(dione::codex::CodexThreadId::parse("thread-io-retry").unwrap())
            .unwrap();
        let moved = state_dir.with_extension("moved");
        std::fs::rename(&state_dir, &moved).unwrap();
        std::fs::write(&state_dir, b"not a directory").unwrap();
        let root = CancellationToken::new();
        let worker_cancel = CancellationToken::new();
        let task = tokio::spawn(cancel_root_when_complete(
            root.clone(),
            dione::codex::run_delivery_worker(
                queue.clone(),
                delivery_config(&state_dir),
                queue.subscribe_live_binding(),
                worker_cancel.clone(),
            ),
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!root.is_cancelled());
        assert!(!task.is_finished());
        worker_cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn intentional_pull_handoff_does_not_cancel_root_lifecycle() {
        let dir = TempDir::new().unwrap();
        let state_dir = Utf8PathBuf::from_path_buf(dir.path().join("state")).unwrap();
        let queue = CodexEventQueue::load(&state_dir).unwrap();
        queue
            .register_consumer(
                "pull primary".to_owned(),
                Duration::from_secs(60),
                true,
                true,
            )
            .await
            .unwrap();
        dione::codex::CodexThreadBinder::new(queue.clone())
            .bind(dione::codex::CodexThreadId::parse("thread-standby").unwrap())
            .unwrap();
        let root = CancellationToken::new();
        let worker_cancel = CancellationToken::new();
        let task = tokio::spawn(cancel_root_when_complete(
            root.clone(),
            dione::codex::run_delivery_worker(
                queue.clone(),
                delivery_config(&state_dir),
                queue.subscribe_live_binding(),
                worker_cancel.clone(),
            ),
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!root.is_cancelled());
        assert!(!task.is_finished());
        worker_cancel.cancel();
        task.await.unwrap().unwrap();
    }

    #[tokio::test]
    async fn terminal_task_error_cancels_root_lifecycle() {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async { Err::<(), _>("terminal listener") });

        let result = cancel_root_on_task_failure(cancel.clone(), task).await;

        assert_eq!(result.expect("task joined"), Err("terminal listener"));
        assert!(cancel.is_cancelled());
    }

    #[tokio::test]
    async fn clean_child_task_exit_does_not_cancel_root_lifecycle() {
        let cancel = CancellationToken::new();
        let task = tokio::spawn(async { Ok::<_, &str>(()) });

        let result = cancel_root_on_task_failure(cancel.clone(), task).await;

        assert_eq!(result.expect("task joined"), Ok(()));
        assert!(!cancel.is_cancelled());
    }

    #[tokio::test]
    async fn child_task_panic_cancels_root_lifecycle() {
        let cancel = CancellationToken::new();
        let task: tokio::task::JoinHandle<Result<(), &str>> =
            tokio::spawn(async { panic!("listener panic") });

        let result = cancel_root_on_task_failure(cancel.clone(), task).await;

        assert!(result.expect_err("panic must fail the join").is_panic());
        assert!(cancel.is_cancelled());
    }
}
