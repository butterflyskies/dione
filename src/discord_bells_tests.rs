#[cfg(test)]
pub(crate) mod tests {
    use crate::harness::test_support::FixtureNotification;
    use axum::response::IntoResponse;
    use dione_bell_rings::{Bell, BellEvaluator, BellStatus};
    use dione_bell_rings::{
        BellProviderConfig, BellProviderUrl, BellScope, BellTimbre, RecallOptions,
    };
    use dione_config::BellRingsConfig;
    use dione_config::Config;
    use dione_discord_bells::{evaluate_native, prepare_bells};
    use dione_discord_events::MentionKind;
    use dione_discord_events::MessageEvent;
    use dione_discord_events::MessageTargeting;
    use dione_discord_events::NotificationEvent;
    use dione_messages::timestamp::Timestamp;
    use serde_json::{Value, json};
    use serenity::model::id::{ChannelId, MessageId, UserId};
    use std::sync::{
        Arc, Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };
    pub(crate) struct McpFixture {
        pub(crate) calls: Arc<AtomicUsize>,
        initializations: Arc<AtomicUsize>,
        requests: Arc<StdMutex<Vec<RecallRequest>>>,
        pub(crate) endpoint: String,
        response: Arc<StdMutex<MockResponse>>,
        shutdown: tokio_util::sync::CancellationToken,
        server: tokio::task::JoinHandle<()>,
        observed: Arc<tokio::sync::Notify>,
        completed: Arc<tokio::sync::Notify>,
    }

    #[derive(Clone)]
    enum MockResponse {
        Value(Value),
        Error,
        ToolError,
        Pending,
        Gated(Value, Arc<tokio::sync::Semaphore>),
    }

    struct RecallRequest {
        query: String,
        scope: String,
        limit: usize,
    }

    impl McpFixture {
        pub(crate) async fn value(value: Value) -> Self {
            Self::start(MockResponse::Value(value)).await
        }

        async fn start(response: MockResponse) -> Self {
            let calls = Arc::new(AtomicUsize::new(0));
            let initializations = Arc::new(AtomicUsize::new(0));
            let requests = Arc::new(StdMutex::new(Vec::new()));
            let shutdown = tokio_util::sync::CancellationToken::new();
            let response = Arc::new(StdMutex::new(response));
            let observed = Arc::new(tokio::sync::Notify::new());
            let completed = Arc::new(tokio::sync::Notify::new());
            let state = (
                calls.clone(),
                requests.clone(),
                response.clone(),
                shutdown.clone(),
                initializations.clone(),
                observed.clone(),
                completed.clone(),
            );
            let router = axum::Router::new().route(
                "/mcp",
                axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                    let (calls, requests, response, shutdown, initializations, observed, completed) = state.clone();
                    async move {
                        let Some(id) = body.get("id") else {
                            return axum::http::StatusCode::ACCEPTED.into_response();
                        };
                        let result = match body["method"].as_str().unwrap() {
                            "initialize" => {
                                initializations.fetch_add(1, Ordering::SeqCst);
                                json!({
                                    "protocolVersion": "2025-03-26",
                                    "capabilities": {"tools": {}},
                                    "serverInfo": {"name": "bell-fixture", "version": "1"}
                                })
                            }
                            "tools/call" => {
                                assert_eq!(body["params"]["name"], "recall");
                                let arguments = &body["params"]["arguments"];
                                calls.fetch_add(1, Ordering::SeqCst);
                                requests.lock().unwrap().push(RecallRequest {
                                    query: arguments["query"].as_str().unwrap().to_owned(),
                                    scope: arguments["scope"].as_str().unwrap().to_owned(),
                                    limit: arguments["limit"].as_u64().unwrap() as usize,
                                });
                                observed.notify_one();
                                let response = response.lock().unwrap().clone();
                                match response {
                                    MockResponse::Value(value) => json!({
                                        "content": [{"type": "text", "text": value.to_string()}],
                                        "isError": false,
                                    }),
                                    MockResponse::Gated(value, release) => {
                                        release.acquire().await.unwrap().forget();
                                        json!({"content": [{"type": "text", "text": value.to_string()}], "isError": false})
                                    }
                                    MockResponse::Error => {
                                        return axum::Json(json!({
                                            "jsonrpc": "2.0", "id": id,
                                            "error": {"code": -32603, "message": "fixture failure"}
                                        }))
                                        .into_response();
                                    }
                                    MockResponse::ToolError => {
                                        json!({"content": [], "isError": true})
                                    }
                                    MockResponse::Pending => {
                                        shutdown.cancelled().await;
                                        return axum::http::StatusCode::SERVICE_UNAVAILABLE
                                            .into_response();
                                    }
                                }
                            }
                            other => panic!("unexpected fixture method {other}"),
                        };
                        if body["method"] == "tools/call" { completed.notify_one(); }
                        axum::Json(json!({"jsonrpc": "2.0", "id": id, "result": result}))
                            .into_response()
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
            let stopped = shutdown.clone();
            let server = tokio::spawn(async move {
                axum::serve(listener, router)
                    .with_graceful_shutdown(stopped.cancelled_owned())
                    .await
                    .unwrap();
            });
            Self {
                calls,
                initializations,
                requests,
                endpoint,
                response,
                shutdown,
                server,
                observed,
                completed,
            }
        }
    }

    impl Drop for McpFixture {
        fn drop(&mut self) {
            self.shutdown.cancel();
            self.server.abort();
        }
    }

    async fn evaluate_fixture(
        message: &MessageEvent,
        config: &BellRingsConfig,
        provider: &McpFixture,
    ) -> Option<Vec<Bell>> {
        let (_, bells, status) = evaluate_fixture_event(
            NotificationEvent::Message(message.clone()),
            config,
            provider,
        )
        .await;
        match status {
            Some(BellStatus::Ok) => Some(bells),
            None => None,
            other => panic!("expected successful fixture retrieval, got {other:?}"),
        }
    }

    async fn evaluate_fixture_event(
        event: NotificationEvent,
        config: &BellRingsConfig,
        provider: &McpFixture,
    ) -> (NotificationEvent, Vec<Bell>, Option<BellStatus>) {
        let mut config = config.clone();
        for item in &mut config.providers {
            item.url = BellProviderUrl::parse(&provider.endpoint).unwrap();
        }
        evaluate_native(event, &config, &BellEvaluator::new()).await
    }

    #[tokio::test]
    async fn native_retrieval_reuses_client_for_alias_changes_and_separates_scopes() {
        let fixture = McpFixture::value(envelope(vec![])).await;
        let mut config = config();
        config.providers[0].url = BellProviderUrl::parse(&fixture.endpoint).unwrap();
        let evaluator = BellEvaluator::new();
        for alias in ["first", "changed"] {
            config.providers[0].alias = Some(alias.to_owned());
            let (event, bells, status) = evaluate_native(
                NotificationEvent::Message(message(MessageTargeting::DirectMessage)),
                &config,
                &evaluator,
            )
            .await;
            assert!(matches!(event, NotificationEvent::Message(_)));
            assert!(bells.is_empty());
            assert_eq!(status, Some(BellStatus::Ok));
        }
        assert_eq!(fixture.initializations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
        config.providers[0].scope = BellScope::parse("another").unwrap();
        let (_, _, status) = evaluate_native(
            NotificationEvent::Message(message(MessageTargeting::DirectMessage)),
            &config,
            &evaluator,
        )
        .await;
        assert_eq!(status, Some(BellStatus::Ok));
        assert_eq!(fixture.initializations.load(Ordering::SeqCst), 2);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 3);
        assert_eq!(fixture.requests.lock().unwrap()[2].scope, "another");
    }

    #[tokio::test]
    async fn native_skips_never_initialize_an_mcp_client() {
        let fixture = McpFixture::value(envelope(vec![])).await;
        let mut config = config();
        config.providers[0].url = BellProviderUrl::parse(&fixture.endpoint).unwrap();
        let evaluator = BellEvaluator::new();
        let event = NotificationEvent::ConfigError {
            error: "fixture".to_owned(),
        };
        let (returned, bells, status) = evaluate_native(event, &config, &evaluator).await;
        assert!(matches!(returned, NotificationEvent::ConfigError { error } if error == "fixture"));
        assert!(bells.is_empty());
        assert_eq!(status, None);
        for targeting in [MessageTargeting::Ambient, MessageTargeting::DirectMessage] {
            config.enabled = targeting == MessageTargeting::Ambient;
            let (_, bells, status) = evaluate_native(
                NotificationEvent::Message(message(targeting)),
                &config,
                &evaluator,
            )
            .await;
            assert!(bells.is_empty());
            assert_eq!(status, None);
        }
        assert_eq!(fixture.initializations.load(Ordering::SeqCst), 0);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 0);
        config.enabled = true;
        let (_, _, status) = evaluate_native(
            NotificationEvent::Message(message(MessageTargeting::DirectMessage)),
            &config,
            &evaluator,
        )
        .await;
        assert_eq!(status, Some(BellStatus::Ok));
        assert_eq!(fixture.initializations.load(Ordering::SeqCst), 1);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn native_transport_errors_reconnect_but_tool_errors_reuse_client() {
        for (response, expected_initializations) in
            [(MockResponse::Error, 2), (MockResponse::ToolError, 1)]
        {
            let fixture = McpFixture::start(response).await;
            let mut config = config();
            config.providers[0].url = BellProviderUrl::parse(&fixture.endpoint).unwrap();
            let evaluator = BellEvaluator::new();
            let (_, bells, status) = evaluate_native(
                NotificationEvent::Message(message(MessageTargeting::DirectMessage)),
                &config,
                &evaluator,
            )
            .await;
            assert_eq!(status, Some(BellStatus::Error));
            assert!(bells.is_empty());
            *fixture.response.lock().unwrap() = MockResponse::Value(envelope(vec![]));
            let (_, bells, status) = evaluate_native(
                NotificationEvent::Message(message(MessageTargeting::DirectMessage)),
                &config,
                &evaluator,
            )
            .await;
            assert_eq!(status, Some(BellStatus::Ok));
            assert!(bells.is_empty());
            assert_eq!(
                fixture.initializations.load(Ordering::SeqCst),
                expected_initializations
            );
            assert_eq!(fixture.calls.load(Ordering::SeqCst), 2);
        }
    }

    #[tokio::test]
    async fn native_ready_and_refused_providers_return_surviving_bells() {
        let fixture =
            McpFixture::value(envelope(vec![hit("survivor", "memory", 0.1, "semantic")])).await;
        let mut config = config();
        config.providers[0].url = BellProviderUrl::parse(&fixture.endpoint).unwrap();
        config.providers.push(BellProviderConfig {
            url: BellProviderUrl::parse("http://127.0.0.1:1/mcp").unwrap(),
            scope: BellScope::parse("unavailable").unwrap(),
            alias: None,
        });
        let original = NotificationEvent::Message(message(MessageTargeting::DirectMessage));
        let before = original.clone().fixture_notification().await;
        let (returned, bells, status) =
            evaluate_native(original, &config, &BellEvaluator::new()).await;
        assert_eq!(returned.fixture_notification().await, before);
        assert_eq!(status, Some(BellStatus::PartialError));
        assert_eq!(bells.len(), 1);
        assert_eq!(bells[0].memory_id, "survivor");
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
    }
    fn config() -> BellRingsConfig {
        BellRingsConfig {
            enabled: true,
            mode: dione_config::BellMode::Live,
            providers: vec![BellProviderConfig {
                url: BellProviderUrl::parse("http://memory.example/mcp").unwrap(),
                scope: BellScope::parse("syne").unwrap(),
                alias: None,
            }],
            trigger: dione_config::BellTrigger::Directed,
            channel_overrides: vec![],
            recall: RecallOptions::default(),
        }
    }

    fn message(targeting: MessageTargeting) -> MessageEvent {
        MessageEvent {
            chat_id: ChannelId::new(1),
            message_id: MessageId::new(2),
            user: "user".to_owned(),
            user_id: UserId::new(3),
            author_kind: dione_attention::types::SourceAuthorKind::DirectHuman,
            content: "query text".to_owned(),
            targeting,
            timestamp: Timestamp::parse("2026-01-01T00:00:00Z").unwrap(),
            attachments: vec![],
            is_voice_message: false,
            thread_parent_id: None,
            reply_to_message_id: None,
            reply_to_user_id: None,
            reply_to_user: None,
            reply_to_content_preview: None,
            bells: None,
            bells_status: None,
        }
    }

    fn hit(id: &str, name: &str, distance: f64, match_type: &str) -> Value {
        json!({
            "id": id,
            "name": name,
            "scope": "syne",
            "distance": distance,
            "match_type": match_type,
        })
    }

    fn envelope(results: Vec<Value>) -> Value {
        json!({ "recall_id": "r_test", "results": results, "count": results.len() })
    }

    #[tokio::test]
    async fn directed_enabled_calls_one_provider_with_exact_scope() {
        let provider = McpFixture::value(envelope(vec![])).await;
        let outcome = evaluate_fixture(
            &message(MessageTargeting::GuildDirected(MentionKind::DirectMention)),
            &config(),
            &provider,
        )
        .await;
        assert_eq!(outcome, Some(vec![]));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].scope, "syne");
        assert_eq!(requests[0].query, "query text");
        assert_eq!(requests[0].limit, 3);
    }

    #[tokio::test]
    async fn disabled_undirected_and_missing_provider_make_zero_calls() {
        let provider = McpFixture::value(envelope(vec![])).await;
        let mut disabled = config();
        disabled.enabled = false;
        assert_eq!(
            evaluate_fixture(
                &message(MessageTargeting::DirectMessage),
                &disabled,
                &provider
            )
            .await,
            None
        );
        assert_eq!(
            evaluate_fixture(&message(MessageTargeting::Ambient), &config(), &provider).await,
            None
        );
        let mut missing = config();
        missing.providers = vec![];
        assert_eq!(
            evaluate_fixture(
                &message(MessageTargeting::DirectMessage),
                &missing,
                &provider
            )
            .await,
            None
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn all_scope_is_rejected_at_config_boundary() {
        let parsed = toml::from_str::<Config>(
            "[bell_rings]\nenabled=true\n[bell_rings.provider]\nurl='http://memory/mcp'\nscope='all'",
        );
        assert!(parsed.is_err());
        let parsed = toml::from_str::<Config>(
            "[bell_rings]\nenabled=true\n[[bell_rings.providers]]\nurl='http://memory/mcp'\nscope='all'",
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn bell_config_defaults_and_invalid_boundaries_are_non_vacuous() {
        let defaults = toml::from_str::<Config>("[bell_rings]").unwrap();
        assert!(!defaults.bell_rings.enabled);
        assert_eq!(defaults.bell_rings.recall.max_semantic_distance(), 0.3);
        assert_eq!(defaults.bell_rings.recall.max_bells(), 3);
        assert_eq!(defaults.bell_rings.recall.deadline_ms(), 300);
        assert_eq!(
            defaults.bell_rings.trigger,
            dione_config::BellTrigger::Directed
        );

        for invalid in [
            "[bell_rings]\nmax_semantic_distance=nan",
            "[bell_rings]\nmax_semantic_distance=2.1",
            "[bell_rings]\nmax_bells=0",
            "[bell_rings]\nmax_bells=101",
            "[bell_rings]\ndeadline_ms=0",
            "[bell_rings]\ndeadline_ms=2001",
            "[bell_rings]\nenabled=true",
            "[bell_rings.provider]\nurl='file:///tmp/memory'\nscope='syne'",
            "[bell_rings.provider]\nurl='https://user:pass@memory/mcp'\nscope='syne'",
        ] {
            assert!(toml::from_str::<Config>(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn loudness_tiers_and_timbre_mapping() {
        let provider = McpFixture::value(envelope(vec![
            hit("a", "loud", 0.05, "semantic"),
            hit("b", "middle", 0.15, "both"),
            hit("c", "quiet", 0.28, "semantic"),
            hit("d", "lexical-hit", -1.0, "lexical"),
            hit("e", "far", 0.31, "semantic"),
        ]))
        .await;
        let outcome = evaluate_fixture(
            &message(MessageTargeting::DirectMessage),
            &config(),
            &provider,
        )
        .await;
        let Some(bells) = outcome else {
            panic!("expected success");
        };
        assert_eq!(
            bells
                .iter()
                .map(|bell| (bell.loudness, bell.timbre))
                .collect::<Vec<_>>(),
            vec![
                (3, BellTimbre::Semantic),
                (3, BellTimbre::Both),
                (1, BellTimbre::Lexical),
            ]
        );
        assert!(bells.iter().all(|bell| bell.recall_id == "r_test"));
    }

    #[tokio::test]
    async fn timeout_error_and_malformed_fail_open() {
        for response in [
            MockResponse::Error,
            MockResponse::Value(json!({ "not": "recall" })),
            MockResponse::Value(json!({
                "recall_id": "r_cross_scope",
                "results": [{
                    "id": "x",
                    "name": "cross-scope",
                    "scope": "someone-else",
                    "distance": 0.01,
                    "match_type": "semantic"
                }]
            })),
            MockResponse::Value(envelope(vec![hit("x", "unknown", 0.01, "mystery")])),
        ] {
            let provider = McpFixture::start(response).await;
            let original = NotificationEvent::Message(message(MessageTargeting::DirectMessage));
            let before = original.clone().fixture_notification().await;
            let (observed, bells, status) =
                evaluate_fixture_event(original, &config(), &provider).await;
            assert_eq!(observed.fixture_notification().await, before);
            assert!(bells.is_empty());
            assert_eq!(status, Some(BellStatus::Error));
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        }

        let provider = McpFixture::start(MockResponse::Pending).await;
        let mut short = config();
        short.recall = RecallOptions::new(0.3, 3, 1000).unwrap();
        let original = NotificationEvent::Message(message(MessageTargeting::DirectMessage));
        let before = original.clone().fixture_notification().await;
        let (observed, bells, status) = evaluate_fixture_event(original, &short, &provider).await;
        assert_eq!(observed.fixture_notification().await, before);
        assert!(bells.is_empty());
        assert_eq!(status, Some(BellStatus::Timeout));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn evaluate_returns_bells_without_injecting_metadata() {
        let provider =
            McpFixture::value(envelope(vec![hit("a", "memory-name", 0.1, "semantic")])).await;
        let original = NotificationEvent::Message(message(MessageTargeting::DirectMessage));
        let before = original.clone().fixture_notification().await;
        let (observed, bells, status) =
            evaluate_fixture_event(original, &config(), &provider).await;
        assert_eq!(observed.fixture_notification().await, before);
        assert_eq!(bells.len(), 1);
        assert_eq!(bells[0].memory_name, "memory-name");
        assert_eq!(status, Some(BellStatus::Ok));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn max_bells_truncation_is_deterministic() {
        let provider = McpFixture::value(envelope(vec![
            hit("c", "same", 0.1, "semantic"),
            hit("b", "same", 0.1, "semantic"),
            hit("a", "first", 0.05, "semantic"),
        ]))
        .await;
        let mut limited = config();
        limited.recall = RecallOptions::new(0.3, 2, 300).unwrap();
        let outcome = evaluate_fixture(
            &message(MessageTargeting::DirectMessage),
            &limited,
            &provider,
        )
        .await;
        let Some(bells) = outcome else {
            panic!("expected success");
        };
        assert_eq!(
            bells
                .iter()
                .map(|bell| bell.memory_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[tokio::test]
    async fn lexical_bell_does_not_displace_louder_semantic_on_truncation() {
        let provider = McpFixture::value(envelope(vec![
            hit("semantic-hit", "loud-semantic", 0.05, "semantic"),
            hit("lexical-hit", "quiet-lexical", -1.0, "lexical"),
        ]))
        .await;
        let mut limited = config();
        limited.recall = RecallOptions::new(0.3, 1, 300).unwrap();
        let outcome = evaluate_fixture(
            &message(MessageTargeting::DirectMessage),
            &limited,
            &provider,
        )
        .await;
        let Some(bells) = outcome else {
            panic!("expected success");
        };
        assert_eq!(bells.len(), 1);
        assert_eq!(bells[0].memory_id, "semantic-hit");
        assert_eq!(bells[0].loudness, 3);
        assert_eq!(bells[0].timbre, BellTimbre::Semantic);
    }

    #[tokio::test]
    async fn all_trigger_rings_on_ambient_messages() {
        let provider = McpFixture::value(envelope(vec![hit("a", "found", 0.1, "semantic")])).await;
        let mut all_config = config();
        all_config.trigger = dione_config::BellTrigger::All;
        let outcome =
            evaluate_fixture(&message(MessageTargeting::Ambient), &all_config, &provider).await;
        let Some(bells) = outcome else {
            panic!("expected success on ambient message with All trigger");
        };
        assert_eq!(bells.len(), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn channel_override_trigger_takes_precedence() {
        let provider = McpFixture::value(envelope(vec![hit("a", "found", 0.1, "semantic")])).await;
        let mut override_config = config();
        override_config.trigger = dione_config::BellTrigger::Directed;
        override_config.channel_overrides = vec![dione_config::BellChannelOverride {
            channel_id: "1".to_owned(),
            trigger: dione_config::BellTrigger::All,
        }];
        let outcome = evaluate_fixture(
            &message(MessageTargeting::Ambient),
            &override_config,
            &provider,
        )
        .await;
        let Some(bells) = outcome else {
            panic!("expected success: channel 1 has All trigger override");
        };
        assert_eq!(bells.len(), 1);
    }

    #[test]
    fn zero_channel_id_is_rejected() {
        let parsed = toml::from_str::<Config>(
            "[bell_rings]\nenabled=true\ntrigger='all'\n\
            [bell_rings.provider]\nurl='http://memory/mcp'\nscope='syne'\n\
            [[bell_rings.channel_overrides]]\nchannel_id='0'\ntrigger='directed'",
        );
        assert!(parsed.is_err(), "channel_id=0 must be rejected");
    }

    #[tokio::test]
    async fn global_all_with_directed_channel_override_skips_ambient() {
        let provider = McpFixture::value(envelope(vec![hit("a", "found", 0.1, "semantic")])).await;
        let mut override_config = config();
        override_config.trigger = dione_config::BellTrigger::All;
        override_config.channel_overrides = vec![dione_config::BellChannelOverride {
            channel_id: "1".to_owned(),
            trigger: dione_config::BellTrigger::Directed,
        }];
        let outcome = evaluate_fixture(
            &message(MessageTargeting::Ambient),
            &override_config,
            &provider,
        )
        .await;
        assert_eq!(
            outcome, None,
            "channel 1 overrides to directed, ambient should be skipped"
        );
    }

    #[test]
    fn duplicate_provider_aliases_are_rejected() {
        let parsed = toml::from_str::<Config>(
            "[bell_rings]\nenabled=true\n\
            [[bell_rings.providers]]\nurl='http://personal/mcp'\nscope='lain'\nalias='same'\n\
            [[bell_rings.providers]]\nurl='http://shared/mcp'\nscope='shared'\nalias='same'",
        );
        assert!(parsed.is_err(), "duplicate aliases must be rejected");
    }

    #[test]
    fn whitespace_only_alias_is_rejected() {
        let parsed = toml::from_str::<Config>(
            "[bell_rings]\nenabled=true\n\
            [[bell_rings.providers]]\nurl='http://personal/mcp'\nscope='lain'\nalias='   '",
        );
        assert!(parsed.is_err(), "whitespace-only alias must be rejected");
    }

    #[test]
    fn case_insensitive_alias_collision_is_rejected() {
        let parsed = toml::from_str::<Config>(
            "[bell_rings]\nenabled=true\n\
            [[bell_rings.providers]]\nurl='http://personal/mcp'\nscope='lain'\nalias='cc'\n\
            [[bell_rings.providers]]\nurl='http://shared/mcp'\nscope='shared'\nalias=' CC '",
        );
        assert!(
            parsed.is_err(),
            "case-insensitive alias collision must be rejected"
        );
    }

    #[test]
    fn alias_whitespace_is_trimmed_at_parse_time() {
        let cfg: Config = toml::from_str(
            "[bell_rings]\nenabled=true\n\
            [[bell_rings.providers]]\nurl='http://personal/mcp'\nscope='lain'\nalias='  personal  '",
        )
        .unwrap();
        assert_eq!(
            cfg.bell_rings.providers[0].alias(),
            "personal",
            "alias whitespace should be trimmed"
        );
    }

    #[test]
    fn legacy_singular_provider_config_still_works() {
        let cfg: Config = toml::from_str(
            "[bell_rings]\nenabled=true\n[bell_rings.provider]\nurl='http://memory/mcp'\nscope='syne'",
        )
        .unwrap();
        assert_eq!(cfg.bell_rings.providers.len(), 1);
        assert_eq!(cfg.bell_rings.providers[0].scope.as_str(), "syne");
    }

    #[test]
    fn multi_provider_config_works() {
        let cfg: Config = toml::from_str(
            "[bell_rings]\nenabled=true\n\
            [[bell_rings.providers]]\nurl='http://personal/mcp'\nscope='lain'\n\
            [[bell_rings.providers]]\nurl='http://shared/mcp'\nscope='shared'",
        )
        .unwrap();
        assert_eq!(cfg.bell_rings.providers.len(), 2);
        assert_eq!(cfg.bell_rings.providers[0].scope.as_str(), "lain");
        assert_eq!(cfg.bell_rings.providers[1].scope.as_str(), "shared");
    }

    #[test]
    fn trigger_all_config_parses() {
        let cfg: Config = toml::from_str(
            "[bell_rings]\nenabled=true\ntrigger='all'\n\
            [bell_rings.provider]\nurl='http://memory/mcp'\nscope='syne'",
        )
        .unwrap();
        assert_eq!(cfg.bell_rings.trigger, dione_config::BellTrigger::All);
    }

    fn loaded_mode(
        provider: &McpFixture,
        mode: dione_config::BellMode,
    ) -> dione_config::LoadedConfig {
        let mut settings = config();
        settings.mode = mode;
        settings.providers[0].url = BellProviderUrl::parse(&provider.endpoint).unwrap();
        settings.providers[0].alias = Some("fixture".into());
        dione_config::LoadedConfig::try_from_raw(Config {
            bell_rings: settings,
            ..Config::default()
        })
        .unwrap()
    }

    #[tokio::test]
    async fn shadow_returns_existing_metadata_before_gated_provider_and_never_injects() {
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let provider = McpFixture::start(MockResponse::Gated(
            envelope(vec![hit("new", "retrieved", 0.1, "semantic")]),
            release.clone(),
        ))
        .await;
        let cfg = loaded_mode(&provider, dione_config::BellMode::Shadow);
        let mut msg = message(MessageTargeting::DirectMessage);
        msg.bells = Some("existing bell".into());
        msg.bells_status = Some(BellStatus::PartialError);
        let event = NotificationEvent::Message(msg);
        let before = event.clone().fixture_notification().await;
        let evaluator = Arc::new(BellEvaluator::new());
        let returned = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::select! {
                biased;
                returned = prepare_bells(event, &cfg, &evaluator) => returned,
                _ = provider.observed.notified() => {
                    panic!("shadow must return before awaiting the actual provider request")
                }
            }
        })
        .await
        .expect("shadow must return while provider is held");
        assert_eq!(returned.fixture_notification().await, before);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            provider.observed.notified(),
        )
        .await
        .expect("actual provider request");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        release.add_permits(1);
        tokio::time::timeout(
            std::time::Duration::from_secs(2),
            provider.completed.notified(),
        )
        .await
        .expect("fixture response produced");
    }

    #[tokio::test]
    async fn live_injects_literal_render_and_empty_skip_error_preserve_existing_metadata() {
        let provider =
            McpFixture::value(envelope(vec![hit("new", "retrieved", 0.1, "semantic")])).await;
        let cfg = loaded_mode(&provider, dione_config::BellMode::Live);
        let evaluator = Arc::new(BellEvaluator::new());
        let returned = prepare_bells(
            NotificationEvent::Message(message(MessageTargeting::DirectMessage)),
            &cfg,
            &evaluator,
        )
        .await;
        let wire = returned.fixture_notification().await;
        assert_eq!(
            wire["params"]["meta"]["bells"],
            "2s syne/retrieved@fixture d=0.1 id=new rid=r_test"
        );
        assert_eq!(wire["params"]["meta"]["bells_status"], "ok");
        let mut old = message(MessageTargeting::DirectMessage);
        old.content = "distinct empty query".into();
        old.bells = Some("existing bell".into());
        old.bells_status = Some(BellStatus::PartialError);
        *provider.response.lock().unwrap() = MockResponse::Value(envelope(vec![]));
        let returned =
            prepare_bells(NotificationEvent::Message(old.clone()), &cfg, &evaluator).await;
        let wire = returned.fixture_notification().await;
        assert_eq!(wire["params"]["meta"]["bells"], "existing bell");
        assert_eq!(wire["params"]["meta"]["bells_status"], "ok");
        let mut skipped = cfg.raw.clone();
        skipped.bell_rings.enabled = false;
        let skipped = dione_config::LoadedConfig::try_from_raw(skipped).unwrap();
        let original = NotificationEvent::Message(old.clone());
        let before = original.clone().fixture_notification().await;
        assert_eq!(
            prepare_bells(original, &skipped, &evaluator)
                .await
                .fixture_notification()
                .await,
            before
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
        old.content = "distinct error query".into();
        *provider.response.lock().unwrap() = MockResponse::ToolError;
        let wire = prepare_bells(NotificationEvent::Message(old), &cfg, &evaluator)
            .await
            .fixture_notification()
            .await;
        assert_eq!(wire["params"]["meta"]["bells"], "existing bell");
        assert_eq!(wire["params"]["meta"]["bells_status"], "error");
        let control = NotificationEvent::ConfigError {
            error: "unchanged control".into(),
        };
        let before = control.clone().fixture_notification().await;
        assert_eq!(
            prepare_bells(control, &cfg, &evaluator)
                .await
                .fixture_notification()
                .await,
            before
        );
    }
}
