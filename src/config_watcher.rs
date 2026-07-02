use camino::Utf8PathBuf;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::discord::events::NotificationEvent;

pub fn spawn(
    state_dir: Utf8PathBuf,
    event_tx: mpsc::Sender<NotificationEvent>,
    cancel: CancellationToken,
) {
    tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel::<()>(16);

        let cfg_path = crate::config::config_path(&state_dir);
        let watch_dir = cfg_path
            .parent()
            .unwrap_or(state_dir.as_path())
            .as_std_path()
            .to_path_buf();
        let config_filename = cfg_path.file_name().unwrap_or("config.toml").to_string();
        let mut watcher: RecommendedWatcher = match Watcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if let Ok(event) = res {
                    let is_config = event
                        .paths
                        .iter()
                        .any(|p| p.file_name().is_some_and(|n| n == config_filename.as_str()));
                    if is_config
                        && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
                    {
                        let _ = tx.blocking_send(());
                    }
                }
            },
            notify::Config::default(),
        ) {
            Ok(w) => w,
            Err(e) => {
                tracing::warn!(error = %e, "failed to create config file watcher");
                return;
            }
        };

        if let Err(e) = watcher.watch(&watch_dir, RecursiveMode::NonRecursive) {
            tracing::warn!(error = %e, path = %state_dir, "failed to watch config directory");
            return;
        }

        tracing::info!(path = %state_dir, "config file watcher started");

        loop {
            tokio::select! {
                biased;
                _ = cancel.cancelled() => break,
                Some(()) = rx.recv() => {
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    while rx.try_recv().is_ok() {}

                    let dir = state_dir.clone();
                    let result = tokio::task::spawn_blocking(move || {
                        crate::config::reload_config(&dir)
                    }).await;

                    match result {
                        Ok((_, Some(error))) => {
                            let _ = event_tx.send(NotificationEvent::ConfigError { error }).await;
                        }
                        Ok((_, None)) => {
                            tracing::debug!("config reloaded via file watcher");
                        }
                        Err(e) => {
                            tracing::warn!(error = %e, "config reload task panicked");
                        }
                    }
                }
            }
        }

        tracing::debug!("config file watcher stopped");
    });
}
