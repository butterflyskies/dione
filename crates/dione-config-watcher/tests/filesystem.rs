use camino::{Utf8Path, Utf8PathBuf};
use dione_config::{
    ConfigRuntime, config_path, load_config, set_config_path, subscribe_generation,
};
use dione_discord_events::NotificationEvent;
use std::{fmt, sync::Arc};
use tokio::{
    sync::{Notify, mpsc, watch},
    time::{Duration, timeout},
};
use tokio_util::sync::CancellationToken;
use tracing::{
    Event, Subscriber,
    field::{Field, Visit},
};
use tracing_subscriber::{Layer, layer::Context, prelude::*};

struct ReadyLayer(Arc<Notify>);
#[derive(Default)]
struct Started(bool);
impl Visit for Started {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        if field.name() == "message" && format!("{value:?}") == "config file watcher started" {
            self.0 = true;
        }
    }
}
impl<S: Subscriber> Layer<S> for ReadyLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let mut started = Started::default();
        event.record(&mut started);
        if started.0 {
            self.0.notify_one();
        }
    }
}

fn text(delay: u64) -> String {
    format!("[delivery]\ndelivery_delay_ms = {delay}\n")
}
async fn published(generation: &mut watch::Receiver<u64>, state: &Utf8Path, delay: u64) {
    timeout(Duration::from_secs(5), async {
        loop {
            if load_config(state).raw.delivery.delivery_delay_ms == delay {
                return;
            }
            generation.changed().await.unwrap();
        }
    })
    .await
    .expect("actual watcher publication must expose the literal config");
}
async fn quiet(generation: &mut watch::Receiver<u64>) {
    timeout(Duration::from_secs(3), async {
        loop {
            match timeout(Duration::from_millis(200), generation.changed()).await {
                Ok(result) => result.unwrap(),
                Err(_) => return,
            }
        }
    })
    .await
    .expect("finite watch effects must settle");
}
async fn config_error(events: &mut mpsc::Receiver<NotificationEvent>) -> String {
    let event = timeout(Duration::from_secs(5), events.recv())
        .await
        .expect("invalid canonical config must emit an error")
        .expect("watcher remains alive");
    let NotificationEvent::ConfigError { error } = event else {
        panic!("expected actual config error");
    };
    assert!(
        error.starts_with("config parse error:"),
        "unexpected diagnostic category"
    );
    error
}

#[tokio::test(flavor = "current_thread")]
async fn real_filesystem_publication_replacement_error_recovery_and_normal_cancellation() {
    let directory = tempfile::TempDir::new().unwrap();
    let root = Utf8PathBuf::from_path_buf(directory.path().to_path_buf()).unwrap();
    let state = root.join("state");
    let configured = root.join("configured");
    tokio::fs::create_dir_all(&state).await.unwrap();
    tokio::fs::create_dir_all(&configured).await.unwrap();
    let canonical = configured.join("chosen.toml");
    set_config_path(canonical.clone());
    assert_eq!(config_path(&state), canonical);
    tokio::fs::write(&canonical, text(11)).await.unwrap();
    let (initial, error) = ConfigRuntime::new(state.clone()).reload().await;
    assert!(error.is_none());
    assert_eq!(initial.raw.delivery.delivery_delay_ms, 11);
    let mut generation = subscribe_generation();
    let started = Arc::new(Notify::new());
    let subscriber = tracing_subscriber::registry().with(ReadyLayer(started.clone()));
    let _scoped_subscriber = tracing::subscriber::set_default(subscriber);
    let (events, mut errors) = mpsc::channel(16);
    let cancel = CancellationToken::new();
    let task = dione_config_watcher::spawn(state.clone(), events, cancel.clone());
    timeout(Duration::from_secs(5), started.notified())
        .await
        .expect("existing watcher-started event follows successful watch registration");

    tokio::fs::write(&canonical, text(22)).await.unwrap();
    published(&mut generation, &state, 22).await;
    assert!(load_config(&state).generation() > initial.generation());
    let replacement = configured.join("replacement.tmp");
    tokio::fs::write(&replacement, text(33)).await.unwrap();
    tokio::fs::rename(&replacement, &canonical).await.unwrap();
    published(&mut generation, &state, 33).await;
    quiet(&mut generation).await;
    let before_unrelated = load_config(&state).generation();
    tokio::fs::write(configured.join("other.toml"), text(999))
        .await
        .unwrap();
    tokio::fs::write(state.join("config.toml"), text(999))
        .await
        .unwrap();
    assert!(
        timeout(Duration::from_millis(250), generation.changed())
            .await
            .is_err()
    );
    assert_eq!(load_config(&state).generation(), before_unrelated);
    assert_eq!(load_config(&state).raw.delivery.delivery_delay_ms, 33);
    assert!(errors.try_recv().is_err());
    tokio::fs::write(&canonical, text(44)).await.unwrap();
    published(&mut generation, &state, 44).await;
    quiet(&mut generation).await;

    let invalid = "[delivery\n";
    tokio::fs::write(&canonical, invalid).await.unwrap();
    let recovered = config_error(&mut errors).await;
    assert!(recovered.contains("canonical restored from last-known-good"));
    assert_eq!(load_config(&state).raw.delivery.delivery_delay_ms, 44);
    assert_eq!(
        tokio::fs::read_to_string(&canonical).await.unwrap(),
        text(44)
    );
    assert_eq!(
        tokio::fs::read_to_string(dione_config::quarantine_path(&canonical))
            .await
            .unwrap(),
        invalid
    );
    tokio::fs::write(&canonical, text(55)).await.unwrap();
    published(&mut generation, &state, 55).await;
    quiet(&mut generation).await;
    while let Ok(event) = errors.try_recv() {
        let NotificationEvent::ConfigError { error } = event else {
            panic!("config error");
        };
        assert!(error.starts_with("config parse error:"));
    }
    tokio::fs::remove_file(dione_config::lkg_path(&canonical))
        .await
        .unwrap();
    let before_no_lkg = load_config(&state).generation();
    tokio::fs::write(&canonical, invalid).await.unwrap();
    let retained = config_error(&mut errors).await;
    assert!(!retained.contains("canonical restored from last-known-good"));
    assert_eq!(load_config(&state).raw.delivery.delivery_delay_ms, 55);
    assert_eq!(load_config(&state).generation(), before_no_lkg);
    assert_eq!(
        tokio::fs::read_to_string(&canonical).await.unwrap(),
        invalid
    );
    tokio::fs::write(&canonical, text(66)).await.unwrap();
    published(&mut generation, &state, 66).await;
    quiet(&mut generation).await;

    cancel.cancel();
    timeout(Duration::from_secs(3), task)
        .await
        .expect("normal quiescent watcher stops")
        .unwrap();
    let stopped_generation = load_config(&state).generation();
    tokio::fs::write(&canonical, text(77)).await.unwrap();
    assert!(
        timeout(Duration::from_millis(250), generation.changed())
            .await
            .is_err()
    );
    assert_eq!(load_config(&state).generation(), stopped_generation);
    assert_eq!(load_config(&state).raw.delivery.delivery_delay_ms, 66);
    let (after, error) = ConfigRuntime::new(state.clone()).reload().await;
    assert!(error.is_none());
    assert_eq!(after.raw.delivery.delivery_delay_ms, 77);
    assert!(after.generation() > stopped_generation);
}
