use camino::Utf8Path;
use dione_config::{AccessConfig, ChannelConfig, Config, DmPolicy, LoadedConfig};
use dione_discord_gate::{GateDecision, InboundGate, OutboundGate};
use dione_mutes::{MuteState, MuteStore};
use std::collections::{BTreeMap, HashSet};

#[tokio::test]
async fn borrowed_live_store_observes_mute_unmute_and_policy_controls() {
    let directory = tempfile::tempdir().unwrap();
    let store = MuteStore::from_state(
        MuteState::default(),
        Utf8Path::from_path(directory.path()).unwrap(),
    );
    let raw = Config {
        access: AccessConfig {
            dm_policy: DmPolicy::Queue,
            allow_from: vec!["7".into()],
            ..AccessConfig::default()
        },
        channels: vec![ChannelConfig {
            id: "100".into(),
            require_mention: true,
            allow_from: vec!["7".into()],
            ..ChannelConfig::default()
        }],
        ..Config::default()
    };
    let config = LoadedConfig::try_from_raw(raw.clone()).unwrap();
    let check = |guild, mutes| InboundGate::check_guild(&config, 100, 7, true, guild, mutes);
    assert_eq!(check(Some(500), Some(&store)), GateDecision::Deliver);
    store
        .mute_guild(500, 60, "fixture".into(), None)
        .await
        .unwrap();
    assert_eq!(check(Some(500), Some(&store)), GateDecision::Drop);
    assert_eq!(check(Some(501), Some(&store)), GateDecision::Deliver);
    assert_eq!(check(Some(500), None), GateDecision::Deliver);
    assert_eq!(check(None, Some(&store)), GateDecision::Deliver);
    assert_eq!(
        InboundGate::check_guild(&config, 100, 7, false, Some(501), Some(&store)),
        GateDecision::Drop
    );
    assert_eq!(
        InboundGate::check_guild(&config, 100, 8, true, Some(501), Some(&store)),
        GateDecision::Drop
    );
    let mut ignored = raw;
    ignored.access.ignore_from = vec!["7".into()];
    let ignored = LoadedConfig::try_from_raw(ignored).unwrap();
    assert_eq!(
        InboundGate::check_guild(&ignored, 100, 7, true, Some(500), Some(&store)),
        GateDecision::Drop
    );
    assert_eq!(
        InboundGate::check_guild(&ignored, 100, 7, true, Some(501), Some(&store)),
        GateDecision::Drop
    );
    assert_eq!(InboundGate::check_dm(&config, 7), GateDecision::Deliver);
    assert_eq!(InboundGate::check_dm(&ignored, 7), GateDecision::Drop);
    assert_eq!(
        InboundGate::check_reaction(&config, 8),
        GateDecision::Deliver
    );
    assert_eq!(InboundGate::check_reaction(&ignored, 7), GateDecision::Drop);
    assert!(OutboundGate::check_channel_with_threads(
        &config,
        101,
        &HashSet::new(),
        &BTreeMap::from([(101, Some(100))])
    ));
    assert!(!OutboundGate::check_channel_with_threads(
        &config,
        102,
        &HashSet::new(),
        &BTreeMap::from([(102, Some(999))])
    ));
    store.unmute_guild(500).await.unwrap();
    assert_eq!(check(Some(500), Some(&store)), GateDecision::Deliver);
}
