use camino::Utf8PathBuf;
use dione_config::{
    ChunkMode, Config, ConfigDurability, ConfigRuntime, DmPolicy, LoadedConfig, NoLkgPolicy,
    PreambleMode, ReplyToMode, attention_effect_guard, load_config, load_config_readonly,
    set_config_path, subscribe_generation,
};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

#[tokio::test]
async fn runtime_handles_share_publication_writer_and_generation_receiver() {
    let directory = tempfile::tempdir().unwrap();
    let state = Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
    let path = state.join("selected.toml");
    let original = "# retain this comment\n[access]\ndm_policy = \"queue\"\nallow_from = []\n\n[contradictionary]\nenabled = true\n";
    std::fs::write(&path, original).unwrap();
    std::fs::write(
        state.join("contradictionary.toml"),
        "[[entry]]\npattern = \"external-canary\"\naction = \"block\"\n",
    )
    .unwrap();
    set_config_path(path.clone());
    let first = ConfigRuntime::new(state.clone());
    let other = ConfigRuntime::new(state.join("different-handle"));
    let mut receiver = subscribe_generation();
    let (initial, warning) = first.startup_load(NoLkgPolicy::FailStartup).await.unwrap();
    assert!(warning.is_none());
    receiver.borrow_and_update();
    assert_eq!(
        load_config(&state.join("ignored-reader-path")).generation(),
        initial.generation()
    );
    assert_eq!(
        initial
            .contradictionary
            .as_ref()
            .unwrap()
            .check("external-canary")
            .len(),
        1
    );

    let guard = attention_effect_guard().await;
    let edited = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&edited);
    let (began, started) = tokio::sync::oneshot::channel();
    let mut mutation = tokio::spawn(async move {
        began.send(()).unwrap();
        other
            .mutate(move |store| {
                observed.store(true, Ordering::SeqCst);
                store.ensure_in_allow_from("42")
            })
            .await
            .unwrap()
    });
    started.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut mutation)
            .await
            .is_err()
    );
    assert!(
        !edited.load(Ordering::SeqCst),
        "the admission guard must block the actual editor"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    assert_eq!(load_config(&state).generation(), initial.generation());
    drop(guard);
    let outcome = tokio::time::timeout(Duration::from_secs(5), mutation)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(outcome.durability, ConfigDurability::Durable);
    assert!(outcome.generation > initial.generation());
    tokio::time::timeout(Duration::from_secs(5), receiver.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*receiver.borrow_and_update(), outcome.generation);
    let disk = std::fs::read_to_string(&path).unwrap();
    assert!(disk.starts_with("# retain this comment\n"));
    let parsed: Config = toml::from_str(&disk).unwrap();
    assert_eq!(parsed.access.allow_from, vec!["42"]);
    assert_eq!(load_config(&state).access.allow_from, vec!["42"]);
    assert_eq!(load_config(&state).generation(), outcome.generation);
    assert_eq!(
        load_config(&state)
            .contradictionary
            .as_ref()
            .unwrap()
            .check("external-canary")
            .len(),
        1
    );

    let readonly = load_config_readonly(&path).unwrap();
    assert!(readonly.generation() > outcome.generation);
    assert_eq!(load_config(&state).generation(), outcome.generation);
    assert!(!receiver.has_changed().unwrap());
    let (reloaded, warning) = first.reload().await;
    assert!(warning.is_none());
    assert!(reloaded.generation() > readonly.generation());
    tokio::time::timeout(Duration::from_secs(5), receiver.changed())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(*receiver.borrow_and_update(), reloaded.generation());

    std::fs::write(state.join("contradictionary.toml"), "[[entry]\nbroken").unwrap();
    let error = first.mutate(|store| store.ensure_in_allow_from("99")).await;
    assert!(error.is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), disk);
    assert_eq!(load_config(&state).generation(), reloaded.generation());
    assert!(!receiver.has_changed().unwrap());
}

#[test]
fn settings_are_exhaustive_and_identity_intent_remains_read_only() {
    fn dm(value: DmPolicy) -> &'static str {
        match value {
            DmPolicy::Queue => "queue",
            DmPolicy::Drop => "drop",
            DmPolicy::Disabled => "disabled",
        }
    }
    fn preamble(value: PreambleMode) -> &'static str {
        match value {
            PreambleMode::First => "first",
            PreambleMode::Always => "always",
            PreambleMode::Never => "never",
        }
    }
    fn reply(value: ReplyToMode) -> &'static str {
        match value {
            ReplyToMode::First => "first",
            ReplyToMode::All => "all",
            ReplyToMode::Off => "off",
        }
    }
    fn chunk(value: ChunkMode) -> &'static str {
        match value {
            ChunkMode::Paragraph => "paragraph",
            ChunkMode::Length => "length",
        }
    }
    assert_eq!(dm(DmPolicy::Queue), "queue");
    assert_eq!(preamble(PreambleMode::First), "first");
    assert_eq!(reply(ReplyToMode::First), "first");
    assert_eq!(chunk(ChunkMode::Paragraph), "paragraph");
    let raw: Config =
        toml::from_str("[[channels]]\nid = \"42\"\nallow_pk_members = [\"invalid\"]\n").unwrap();
    let loaded = LoadedConfig::try_from_raw(raw).unwrap();
    let policy = loaded.channel_policy(42).unwrap();
    assert!(policy.raw_had_identity_entries());
    assert!(policy.has_identity_filter());
}

#[test]
fn marker_version_can_be_named_from_public_config_root() {
    use dione_config::EvidenceMarkerVersion;
    assert_eq!(
        Config::default().delivery.evidence_marker_version,
        EvidenceMarkerVersion::V3
    );
    for (literal, version) in [
        ("v2", EvidenceMarkerVersion::V2),
        ("v3", EvidenceMarkerVersion::V3),
    ] {
        let _: dione_evidence::EvidenceMarkerVersion = version;
        let parsed: Config = toml::from_str(&format!(
            "[delivery]\nevidence_marker_version = {literal:?}\n"
        ))
        .unwrap();
        assert_eq!(parsed.delivery.evidence_marker_version, version);
    }
}
