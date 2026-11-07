//! Filesystem notification, reload triggering and configuration error forwarding.
//!
//! The process-wide `ConfigRuntime` remains the sole configuration publisher.
#![warn(missing_docs)]

use camino::Utf8PathBuf;
use dione_discord_events::NotificationEvent;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Spawn the detached configuration watcher using the canonical config path.
///
/// The callback can block on its bounded queue. Cancellation is checked at the
/// outer select, after any active coalescing, reload and awaited error send.
/// Dropping the returned handle does not stop the task. No prompt cancellation
/// or flood safety guarantee is supplied.
pub fn spawn(
    state_dir: Utf8PathBuf,
    event_tx: mpsc::Sender<NotificationEvent>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let (tx, mut rx) = mpsc::channel::<()>(16);

        let cfg_path = dione_config::config_path(&state_dir);
        let watch_dir = cfg_path
            .parent()
            .unwrap_or(state_dir.as_path())
            .as_std_path()
            .to_path_buf();
        let config_filename = cfg_path.file_name().unwrap_or("config.toml").to_string();
        let mut watcher: RecommendedWatcher = match Watcher::new(
            move |res: Result<notify::Event, notify::Error>| {
                if selects_config_event(&res, &config_filename) {
                    let _ = tx.blocking_send(());
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

                    // The runtime's async reload serializes under the single
                    // config writer and runs its file I/O on the blocking
                    // pool internally.
                    let dir = state_dir.clone();
                    let (_, error) = dione_config::ConfigRuntime::new(dir).reload().await;
                    match error {
                        Some(error) => {
                            let _ = event_tx.send(NotificationEvent::ConfigError { error }).await;
                        }
                        None => {
                            tracing::debug!("config reloaded via file watcher");
                        }
                    }
                }
            }
        }

        tracing::debug!("config file watcher stopped");
    })
}

fn selects_config_event(res: &Result<notify::Event, notify::Error>, config_filename: &str) -> bool {
    if let Ok(event) = res {
        let is_config = event
            .paths
            .iter()
            .any(|p| p.file_name().is_some_and(|n| n == config_filename));
        is_config && matches!(event.kind, EventKind::Modify(_) | EventKind::Create(_))
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::selects_config_event;
    use notify::{
        Event, EventKind,
        event::{CreateKind, ModifyKind, RemoveKind},
    };
    use std::path::PathBuf;

    #[test]
    fn actual_callback_selects_only_successful_create_modify_for_canonical_filename() {
        for kind in [
            EventKind::Create(CreateKind::File),
            EventKind::Modify(ModifyKind::Any),
        ] {
            let event = Ok(Event::new(kind).add_path(PathBuf::from("/configured/custom.toml")));
            assert!(selects_config_event(&event, "custom.toml"));
            assert!(!selects_config_event(&event, "other.toml"));
        }
        let multiple = Ok(Event::new(EventKind::Modify(ModifyKind::Any))
            .add_path(PathBuf::from("/configured/replacement.tmp"))
            .add_path(PathBuf::from("/configured/custom.toml")));
        assert!(selects_config_event(&multiple, "custom.toml"));
        let remove = Ok(Event::new(EventKind::Remove(RemoveKind::File))
            .add_path(PathBuf::from("/configured/custom.toml")));
        assert!(!selects_config_event(&remove, "custom.toml"));
        let other =
            Ok(Event::new(EventKind::Any).add_path(PathBuf::from("/configured/custom.toml")));
        assert!(!selects_config_event(&other, "custom.toml"));
        assert!(!selects_config_event(
            &Err(notify::Error::generic("backend failure")),
            "custom.toml"
        ));
    }
}
