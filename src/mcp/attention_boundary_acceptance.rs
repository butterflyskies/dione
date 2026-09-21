use super::*;

#[derive(Debug, PartialEq)]
struct BoundaryTrace {
    sink: Vec<Value>,
    network: Vec<(String, String, Value)>,
}

fn captured_network(network: &Network) -> Vec<(String, String, Value)> {
    network
        .state
        .lock()
        .requests
        .iter()
        .map(|request| {
            (
                request.method.clone(),
                request.path.clone(),
                request.body.clone(),
            )
        })
        .collect()
}

async fn trace_one_ambient(
    fixture: &mut Fixture,
    channel: u64,
    id: u64,
    text: &str,
    parent: Option<(u64, u64)>,
) -> BoundaryTrace {
    fixture.network.insert(channel, id, text, parent);
    assert!(
        fixture
            .dispatch(fixture.event(channel, id).await)
            .await
            .is_none()
    );
    BoundaryTrace {
        sink: fixture.drain().await,
        network: captured_network(&fixture.network),
    }
}

async fn has_training_target(fixture: &Fixture) -> bool {
    fixture
        .attention
        .with_store(|store| {
            Ok(store
                .list_records(&"default".into())
                .into_iter()
                .any(|record| store.recipient_target(&record.id).is_some()))
        })
        .await
        .unwrap()
}

async fn remove_attention_table(fixture: &Fixture) {
    let config_path = fixture.path.join("config.toml");
    let mut document: toml::Value =
        toml::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    assert!(
        document
            .as_table_mut()
            .unwrap()
            .remove("attention")
            .is_some()
    );
    std::fs::write(&config_path, toml::to_string(&document).unwrap()).unwrap();
    let (_, warning) = ConfigRuntime::new(fixture.path.clone()).reload().await;
    assert!(warning.is_none(), "{warning:?}");
}

#[test]
fn a01_absent_explicit_off_and_emergency_off_have_the_exact_same_sink_and_no_provider() {
    scenario(|| async {
        const ID: u64 = 1_001;
        const TEXT: &str = "same authorized ambient source x";

        let mut off = Fixture::new(AttentionMode::Off).await;
        let baseline = trace_one_ambient(&mut off, 100, ID, TEXT, None).await;
        assert_eq!(off.network.provider_requests(), Vec::<Value>::new());
        assert_eq!(baseline.sink.len(), 1);
        drop(off);

        let mut absent = Fixture::new(AttentionMode::On).await;
        remove_attention_table(&absent).await;
        let absent_trace = trace_one_ambient(&mut absent, 100, ID, TEXT, None).await;
        assert_eq!(absent.network.provider_requests(), Vec::<Value>::new());
        assert_eq!(absent_trace, baseline);
        drop(absent);

        let mut emergency = Fixture::new(AttentionMode::On).await;
        let mut settings = crate::config::load_config(&emergency.path)
            .raw
            .attention
            .clone();
        settings.emergency_off = true;
        settings.rooms.get_mut(&ChannelId::new(100)).unwrap().mode = Some(AttentionMode::On);
        emergency
            .control(AttentionCommand::Configure { settings })
            .await;
        assert_eq!(
            crate::config::load_config(&emergency.path)
                .raw
                .attention
                .effective_mode(ChannelId::new(100)),
            AttentionMode::Off
        );
        let emergency_trace = trace_one_ambient(&mut emergency, 100, ID, TEXT, None).await;
        assert_eq!(emergency.network.provider_requests(), Vec::<Value>::new());
        assert_eq!(emergency_trace, baseline);
    });
}

#[test]
fn a02_log_and_on_ineligible_rooms_are_exact_off_delivery_without_provider_requests() {
    scenario(|| async {
        const ID: u64 = 1_101;
        const TEXT: &str = "same ineligible ambient source x";

        let mut off = Fixture::new(AttentionMode::Off).await;
        let baseline = trace_one_ambient(&mut off, 100, ID, TEXT, None).await;
        assert_eq!(off.network.provider_requests(), Vec::<Value>::new());
        drop(off);

        for mode in [AttentionMode::Log, AttentionMode::On] {
            let mut fixture = Fixture::new(mode).await;
            let mut settings = crate::config::load_config(&fixture.path)
                .raw
                .attention
                .clone();
            settings
                .rooms
                .get_mut(&ChannelId::new(100))
                .unwrap()
                .provider_eligible = false;
            fixture
                .control(AttentionCommand::Configure { settings })
                .await;
            assert_eq!(
                crate::config::load_config(&fixture.path)
                    .raw
                    .attention
                    .effective_mode(ChannelId::new(100)),
                mode
            );
            let trace = trace_one_ambient(&mut fixture, 100, ID, TEXT, None).await;
            assert_eq!(fixture.network.provider_requests(), Vec::<Value>::new());
            assert_eq!(trace, baseline);
        }
    });
}

#[test]
fn a02_provider_ineligible_ancestor_is_excluded_and_forces_ordinary_delivery() {
    scenario(|| async {
        const PARENT_ID: u64 = 1_200;
        const TRIGGER_ID: u64 = 1_201;
        const PARENT_TEXT: &str = "provider-ineligible parent secret";
        const TRIGGER_TEXT: &str = "prompt eligible trigger x";

        let mut off = Fixture::new(AttentionMode::Off).await;
        off.network.insert(200, PARENT_ID, PARENT_TEXT, None);
        let baseline = trace_one_ambient(
            &mut off,
            100,
            TRIGGER_ID,
            TRIGGER_TEXT,
            Some((200, PARENT_ID)),
        )
        .await;
        drop(off);

        let mut fixture = Fixture::new(AttentionMode::On).await;
        let mut settings = crate::config::load_config(&fixture.path)
            .raw
            .attention
            .clone();
        settings.rooms.insert(
            ChannelId::new(200),
            RoomAttention {
                provider_eligible: false,
                ..RoomAttention::default()
            },
        );
        fixture
            .control(AttentionCommand::Configure { settings })
            .await;
        fixture.network.insert(200, PARENT_ID, PARENT_TEXT, None);
        fixture
            .network
            .insert(100, TRIGGER_ID, TRIGGER_TEXT, Some((200, PARENT_ID)));

        let record = fixture
            .dispatch(fixture.event(100, TRIGGER_ID).await)
            .await
            .unwrap();
        assert_eq!(record.hypothetical, Admission::Unknown);
        assert_eq!(record.actual, Admission::Ordinary);
        let sink = fixture.drain().await;
        assert_eq!(sink, baseline.sink);

        let requests = fixture.network.provider_requests();
        assert_eq!(requests.len(), 1);
        let state = &requests[0]["state"];
        assert_eq!(state["trigger"]["text"], TRIGGER_TEXT);
        assert_eq!(
            state["trigger"]["source"],
            serde_json::to_value(&record.sources[0]).unwrap()
        );
        assert_eq!(state["antecedents"], json!([]));
        assert_eq!(state["missing_context"], true);
        assert!(
            !serde_json::to_string(&requests)
                .unwrap()
                .contains(PARENT_TEXT)
        );
        assert!(
            !captured_network(&fixture.network)
                .iter()
                .any(|(_, path, _)| path.contains("/channels/200"))
        );
    });
}

#[test]
fn a04_production_ingress_gate_receipts_cover_denied_and_muted_direct_variants() {
    scenario(|| async {
        const MUTED_GUILD: u64 = 9_000_004;
        let fixture = Fixture::new(AttentionMode::On).await;

        if crate::mute_store::global().is_none() {
            crate::mute_store::init_global(crate::mute_store::MuteStore::from_state(
                crate::mute_store::MuteState::default(),
                &fixture.path,
            ));
        }
        let mute_store = crate::mute_store::global().unwrap();
        mute_store
            .mute_guild(MUTED_GUILD, 60, "boundary-fixture".into(), None)
            .await
            .unwrap();
        assert!(mute_store.is_guild_muted(MUTED_GUILD));

        let config = crate::config::load_config(&fixture.path);
        use crate::gate::GateDecision::{Drop, Queue};

        // The production gateway receipt
        // `discord::events::tests::admission_authority_controls_delivery_and_inheritance`
        // proves that only DirectGuildAdmission::Deliver constructs a delivery and
        // covers GateDrop, NotOptedIn, and GuildMuted. Exercise the missing ambient,
        // explicitly directed, and DM variants at those exact production gates.
        assert_eq!(
            crate::gate::InboundGate::check_guild(&config, 100, 8, false, Some(500)),
            Drop
        );
        assert_eq!(
            crate::gate::InboundGate::check_guild(&config, 999, 7, false, Some(500)),
            Drop
        );
        assert_eq!(
            crate::gate::InboundGate::check_guild(&config, 100, 8, true, Some(500)),
            Drop
        );
        assert_eq!(crate::gate::InboundGate::check_dm(&config, 8), Queue);
        assert_eq!(
            crate::gate::InboundGate::check_guild(&config, 100, 7, false, Some(MUTED_GUILD)),
            Drop
        );
        assert_eq!(
            crate::gate::InboundGate::check_guild(&config, 100, 7, true, Some(MUTED_GUILD)),
            Drop
        );

        mute_store.unmute_guild(MUTED_GUILD).await.unwrap();
    });
}

async fn ordered_off_baseline() -> BoundaryTrace {
    let mut fixture = Fixture::new(AttentionMode::Off).await;
    fixture.network.insert(100, 1_401, "prompt ordered x", None);
    fixture.network.insert(100, 1_402, "prompt ordered y", None);
    fixture.network.insert(100, 1_403, "ordered direct d", None);
    assert!(
        fixture
            .dispatch(fixture.event(100, 1_401).await)
            .await
            .is_none()
    );
    assert!(
        fixture
            .dispatch(fixture.event(100, 1_402).await)
            .await
            .is_none()
    );
    let mut direct = fixture.event(100, 1_403).await;
    direct.targeting = MessageTargeting::GuildDirected(crate::gate::MentionKind::DirectMention);
    assert!(fixture.dispatch(direct).await.is_none());
    assert_eq!(fixture.network.provider_requests(), Vec::<Value>::new());
    BoundaryTrace {
        sink: fixture.drain().await,
        network: captured_network(&fixture.network),
    }
}

#[test]
fn a05_log_sink_matches_off_while_y_finishes_before_x_and_direct_does_not_wait() {
    scenario(|| async {
        let baseline = ordered_off_baseline().await;
        assert_eq!(baseline.sink.len(), 3);

        let mut fixture = Fixture::new(AttentionMode::Log).await;
        fixture.network.insert(100, 1_401, "prompt ordered x", None);
        fixture.network.insert(100, 1_402, "prompt ordered y", None);
        fixture.network.insert(100, 1_403, "ordered direct d", None);

        let x_gate = Arc::new(Semaphore::new(0));
        fixture.network.state.lock().provider_gate = Some(x_gate.clone());
        let x_work = fixture
            .begin(fixture.event(100, 1_401).await)
            .await
            .unwrap();
        let x_source = x_work.record.sources[0].clone();
        let x_pending = tokio::spawn(fixture.attention.clone().evaluate(x_work));
        fixture.network.wait_for_provider(1).await;

        let y_gate = Arc::new(Semaphore::new(0));
        fixture.network.state.lock().provider_gate = Some(y_gate.clone());
        let y_work = fixture
            .begin(fixture.event(100, 1_402).await)
            .await
            .unwrap();
        let y_source = y_work.record.sources[0].clone();
        let y_pending = tokio::spawn(fixture.attention.clone().evaluate(y_work));
        fixture.network.wait_for_provider(2).await;

        let mut direct = fixture.event(100, 1_403).await;
        direct.targeting = MessageTargeting::GuildDirected(crate::gate::MentionKind::DirectMention);
        assert!(fixture.begin(direct).await.is_none());
        assert!(!x_pending.is_finished());
        assert!(!y_pending.is_finished());
        let delivered_while_held = fixture.drain().await;
        assert_eq!(delivered_while_held, baseline.sink);

        y_gate.add_permits(1);
        let y_record = y_pending.await.unwrap();
        assert!(!x_pending.is_finished());
        assert_eq!(y_record.actual, Admission::Ordinary);
        fixture.finish(y_record).await;
        x_gate.add_permits(1);
        let x_record = x_pending.await.unwrap();
        assert_eq!(x_record.actual, Admission::Ordinary);
        fixture.finish(x_record).await;
        assert_eq!(fixture.drain().await, Vec::<Value>::new());

        let requests = fixture.network.provider_requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            requests[0]["state"]["trigger"]["source"],
            serde_json::to_value(&x_source).unwrap()
        );
        assert_eq!(
            requests[1]["state"]["trigger"]["source"],
            serde_json::to_value(&y_source).unwrap()
        );
        assert_eq!(requests[0]["state"]["trigger"]["text"], "prompt ordered x");
        assert_eq!(requests[1]["state"]["trigger"]["text"], "prompt ordered y");
    });
}

#[test]
fn a05_log_provider_failure_keeps_the_exact_off_order_and_sink_envelopes() {
    scenario(|| async {
        let baseline = ordered_off_baseline().await;

        let mut fixture = Fixture::new(AttentionMode::Log).await;
        fixture.network.state.lock().provider_status = 503;
        fixture.network.insert(100, 1_401, "prompt ordered x", None);
        fixture.network.insert(100, 1_402, "prompt ordered y", None);
        fixture.network.insert(100, 1_403, "ordered direct d", None);

        let x_record = fixture
            .dispatch(fixture.event(100, 1_401).await)
            .await
            .unwrap();
        assert_eq!(x_record.actual, Admission::Ordinary);
        assert!(x_record.judgment.is_none());
        assert!(
            fixture
                .dispatch(fixture.event(100, 1_402).await)
                .await
                .is_none()
        );
        let mut direct = fixture.event(100, 1_403).await;
        direct.targeting = MessageTargeting::GuildDirected(crate::gate::MentionKind::DirectMention);
        assert!(fixture.dispatch(direct).await.is_none());

        assert_eq!(fixture.drain().await, baseline.sink);
        let requests = fixture.network.provider_requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]["state"]["trigger"]["source"],
            serde_json::to_value(&x_record.sources[0]).unwrap()
        );
        assert_eq!(requests[0]["state"]["trigger"]["text"], "prompt ordered x");
    });
}

#[test]
fn a07_missing_and_stale_briefs_are_unknown_with_exact_off_delivery() {
    scenario(|| async {
        const ID: u64 = 1_501;
        const TEXT: &str = "noise must not become a not-needed decision";

        let mut off = Fixture::new(AttentionMode::Off).await;
        let baseline = trace_one_ambient(&mut off, 100, ID, TEXT, None).await;
        assert_eq!(off.network.provider_requests(), Vec::<Value>::new());
        drop(off);

        let mut missing = Fixture::new(AttentionMode::On).await;
        let mut settings = crate::config::load_config(&missing.path)
            .raw
            .attention
            .clone();
        settings.brief = None;
        missing
            .control(AttentionCommand::Configure { settings })
            .await;
        let missing_trace = trace_one_ambient(&mut missing, 100, ID, TEXT, None).await;
        assert_eq!(missing.network.provider_requests(), Vec::<Value>::new());
        assert!(missing.attention.health().last_unknown.is_some());
        assert!(!has_training_target(&missing).await);
        assert_eq!(missing_trace, baseline);
        drop(missing);

        let mut stale = Fixture::new(AttentionMode::On).await;
        let mut settings = crate::config::load_config(&stale.path)
            .raw
            .attention
            .clone();
        settings.brief = Some(AttentionBrief {
            text: "explicitly stale recipient-authored brief".into(),
            expires_at_ms: 1,
            provider_eligible: true,
        });
        stale
            .control(AttentionCommand::Configure { settings })
            .await;
        let stale_trace = trace_one_ambient(&mut stale, 100, ID, TEXT, None).await;
        assert_eq!(stale.network.provider_requests(), Vec::<Value>::new());
        assert!(stale.attention.health().last_unknown.is_some());
        assert!(!has_training_target(&stale).await);
        assert_eq!(stale_trace, baseline);
    });
}
