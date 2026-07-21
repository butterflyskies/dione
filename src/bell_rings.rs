//! Inbound memory bell evaluation.
//!
//! Evaluates incoming directed messages against a configured memory-mcp
//! provider. In shadow mode, results are logged without affecting delivery.
//! In live mode, admitted bells are rendered into compact metadata and
//! injected into the notification event.

use crate::{
    config::{BellProviderConfig, BellRingsConfig},
    discord::events::{MessageEvent, NotificationEvent},
};
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientInfo, RawContent},
    service::{RoleClient, RunningService},
    transport::StreamableHttpClientTransport,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use tokio::sync::Mutex;

/// One admitted memory hit, with provenance preserved for later rendering and feedback slices.
#[derive(Debug, Clone, PartialEq)]
pub struct Bell {
    pub provider_url: String,
    pub scope: String,
    pub recall_id: String,
    pub memory_id: String,
    pub memory_name: String,
    pub match_type: SemanticMatchType,
    pub distance: f64,
    pub loudness: u8,
    pub provider_rank: usize,
}

/// Match strategies that carry a meaningful semantic distance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SemanticMatchType {
    Semantic,
    Both,
}

/// Result of one shadow evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum ShadowOutcome {
    /// The event was outside the enabled, directed, configured slice.
    Skipped,
    /// Recall completed and produced zero or more admitted bells.
    Success(Vec<Bell>),
    /// The total hard deadline elapsed.
    Timeout,
    /// Provider transport or tool execution failed.
    ProviderError,
    /// The provider response did not satisfy the recall wire contract.
    MalformedResponse,
}

#[derive(Debug, Clone)]
pub struct RecallRequest {
    pub query: String,
    pub scope: String,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BellProviderError {
    Transport,
    MalformedResponse,
}

/// Narrow provider seam used by shadow evaluation and behavior tests.
pub trait BellProvider: Send + Sync {
    fn recall(
        &self,
        request: RecallRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Value, BellProviderError>> + Send + '_>>;
}

/// Persistent, lazily initialized memory-mcp client.
///
/// rmcp owns the stateful Streamable HTTP session, including SSE and session
/// headers. A failed session is discarded so the next message reconnects;
/// initialization is never repeated per successful message.
struct MemoryMcpProvider {
    endpoint: String,
    client: Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
}

impl MemoryMcpProvider {
    fn new(endpoint: String) -> Self {
        Self {
            endpoint,
            client: Mutex::new(None),
        }
    }

    async fn recall_inner(&self, request: RecallRequest) -> Result<Value, BellProviderError> {
        let mut client = self.client.lock().await;
        if client.as_ref().is_none_or(RunningService::is_closed) {
            let transport = StreamableHttpClientTransport::from_uri(self.endpoint.clone());
            let connected = ClientInfo::default()
                .serve(transport)
                .await
                .map_err(|_| BellProviderError::Transport)?;
            *client = Some(connected);
        }

        let arguments = json!({
            "query": request.query,
            "scope": request.scope,
            "limit": request.limit,
        });
        let arguments = serde_json::from_value::<Map<String, Value>>(arguments)
            .map_err(|_| BellProviderError::MalformedResponse)?;
        let result = match client
            .as_ref()
            .ok_or(BellProviderError::Transport)?
            .call_tool(CallToolRequestParams::new("recall").with_arguments(arguments))
            .await
        {
            Ok(result) if result.is_error != Some(true) => result,
            _ => {
                *client = None;
                return Err(BellProviderError::Transport);
            }
        };

        let text = result
            .content
            .iter()
            .find_map(|content| match &content.raw {
                RawContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .ok_or(BellProviderError::MalformedResponse)?;
        serde_json::from_str(text).map_err(|_| BellProviderError::MalformedResponse)
    }
}

impl BellProvider for MemoryMcpProvider {
    fn recall(
        &self,
        request: RecallRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Value, BellProviderError>> + Send + '_>> {
        Box::pin(self.recall_inner(request))
    }
}

#[derive(Default)]
struct ProviderSlot {
    endpoint: Option<String>,
    provider: Option<Arc<MemoryMcpProvider>>,
}

/// Render admitted bells into the compact `mem_hits` metadata format.
///
/// Format: `"name 91;other-name 42"` — memory name + similarity score
/// (cosine distance × 100, rounded, lower = more similar).
pub fn render_bells(bells: &[Bell]) -> String {
    bells
        .iter()
        .map(|bell| {
            let score = (bell.distance * 100.0).round() as u32;
            format!("{} {score}", bell.memory_name)
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Owns the one reconnectable provider used by the inbound delivery loop.
#[derive(Default)]
pub struct BellEvaluator {
    provider: Mutex<ProviderSlot>,
}

impl BellEvaluator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluates one event and returns bells (if any) alongside it.
    pub async fn evaluate(
        &self,
        event: NotificationEvent,
        config: &BellRingsConfig,
    ) -> (NotificationEvent, Vec<Bell>) {
        let eligible = matches!(
            &event,
            NotificationEvent::Message(message)
                if config.enabled && message.targeting.is_directed()
        );
        if eligible && let Some(provider_config) = config.provider.as_ref() {
            let provider = self.provider(provider_config).await;
            return evaluate_with_provider(event, config, provider.as_ref()).await;
        }
        record_outcome(&ShadowOutcome::Skipped);
        (event, vec![])
    }

    async fn provider(&self, config: &BellProviderConfig) -> Arc<MemoryMcpProvider> {
        let mut slot = self.provider.lock().await;
        if slot.endpoint.as_deref() != Some(config.url.as_str()) {
            slot.endpoint = Some(config.url.as_str().to_owned());
            slot.provider = Some(Arc::new(MemoryMcpProvider::new(
                config.url.as_str().to_owned(),
            )));
        }
        if let Some(provider) = slot.provider.as_ref() {
            return provider.clone();
        }
        let provider = Arc::new(MemoryMcpProvider::new(config.url.as_str().to_owned()));
        slot.endpoint = Some(config.url.as_str().to_owned());
        slot.provider = Some(provider.clone());
        provider
    }
}

/// Testable evaluation seam. Returns the event and any admitted bells.
pub(crate) async fn evaluate_with_provider<P: BellProvider + ?Sized>(
    event: NotificationEvent,
    config: &BellRingsConfig,
    provider: &P,
) -> (NotificationEvent, Vec<Bell>) {
    let outcome = match &event {
        NotificationEvent::Message(message) => evaluate_message(message, config, provider).await,
        _ => ShadowOutcome::Skipped,
    };
    record_outcome(&outcome);
    let bells = match outcome {
        ShadowOutcome::Success(bells) => bells,
        _ => vec![],
    };
    (event, bells)
}

/// Computes bells for one message without mutating the message or its delivery envelope.
pub async fn evaluate_message<P: BellProvider + ?Sized>(
    message: &MessageEvent,
    config: &BellRingsConfig,
    provider: &P,
) -> ShadowOutcome {
    let Some(provider_config) = config.provider.as_ref() else {
        return ShadowOutcome::Skipped;
    };
    if !config.enabled || !message.targeting.is_directed() || config.max_bells == 0 {
        return ShadowOutcome::Skipped;
    }
    let request = RecallRequest {
        query: message.content.clone(),
        scope: provider_config.scope.as_str().to_owned(),
        limit: config.max_bells,
    };
    let recall = tokio::time::timeout(
        Duration::from_millis(config.deadline_ms),
        provider.recall(request),
    )
    .await;
    let response = match recall {
        Err(_) => return ShadowOutcome::Timeout,
        Ok(Err(BellProviderError::Transport)) => return ShadowOutcome::ProviderError,
        Ok(Err(BellProviderError::MalformedResponse)) => {
            return ShadowOutcome::MalformedResponse;
        }
        Ok(Ok(response)) => response,
    };

    match parse_bells(
        response,
        provider_config,
        config.max_semantic_distance,
        config.max_bells,
    ) {
        Ok(bells) => ShadowOutcome::Success(bells),
        Err(()) => ShadowOutcome::MalformedResponse,
    }
}

#[derive(Deserialize)]
struct RecallHit {
    id: String,
    name: String,
    scope: String,
    distance: f64,
    #[serde(default)]
    match_type: Option<String>,
}

#[derive(Deserialize)]
struct RecallEnvelope {
    recall_id: String,
    results: Vec<RecallHit>,
}

fn parse_bells(
    response: Value,
    provider: &BellProviderConfig,
    max_distance: f64,
    max_bells: usize,
) -> Result<Vec<Bell>, ()> {
    let envelope: RecallEnvelope = serde_json::from_value(response).map_err(|_| ())?;
    if envelope.recall_id.is_empty() {
        return Err(());
    }
    let mut bells = Vec::new();
    for (provider_rank, hit) in envelope.results.into_iter().enumerate() {
        // memory-mcp's explicit project scope intentionally includes its
        // global overlay. Any other returned scope violates the pre-call
        // authorization boundary and makes the whole response unusable.
        if hit.scope != provider.scope.as_str() && hit.scope != "global" {
            return Err(());
        }
        let match_type = match hit.match_type.as_deref().unwrap_or("semantic") {
            "semantic" => SemanticMatchType::Semantic,
            "both" => SemanticMatchType::Both,
            "lexical" if hit.distance == -1.0 => continue,
            _ => return Err(()),
        };
        if !hit.distance.is_finite() || hit.distance < 0.0 || hit.distance > max_distance {
            continue;
        }
        bells.push(Bell {
            provider_url: provider.url.as_str().to_owned(),
            scope: hit.scope,
            recall_id: envelope.recall_id.clone(),
            memory_id: hit.id,
            memory_name: hit.name,
            match_type,
            distance: hit.distance,
            loudness: loudness(hit.distance, max_distance),
            provider_rank,
        });
    }
    bells.sort_by(|left, right| {
        left.distance
            .total_cmp(&right.distance)
            .then_with(|| left.memory_name.cmp(&right.memory_name))
            .then_with(|| left.memory_id.cmp(&right.memory_id))
    });
    bells.truncate(max_bells);
    Ok(bells)
}

fn loudness(distance: f64, max_distance: f64) -> u8 {
    if max_distance == 0.0 || distance <= max_distance / 3.0 {
        3
    } else if distance <= (2.0 * max_distance) / 3.0 {
        2
    } else {
        1
    }
}

fn record_outcome(outcome: &ShadowOutcome) {
    match outcome {
        ShadowOutcome::Skipped => {}
        ShadowOutcome::Success(bells) => {
            tracing::info!(bell_count = bells.len(), "bell shadow evaluation completed");
        }
        ShadowOutcome::Timeout => {
            tracing::warn!(outcome = "timeout", "bell shadow evaluation degraded");
        }
        ShadowOutcome::ProviderError => {
            tracing::warn!(
                outcome = "provider_error",
                "bell shadow evaluation degraded"
            );
        }
        ShadowOutcome::MalformedResponse => {
            tracing::warn!(
                outcome = "malformed_response",
                "bell shadow evaluation degraded"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{BellProviderConfig, BellProviderUrl, BellScope, Config},
        discord::events::MessageTargeting,
        gate::MentionKind,
        mcp::notifications::IntoNotification,
        timestamp::Timestamp,
    };
    use serenity::model::id::{ChannelId, MessageId, UserId};
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    struct MockProvider {
        calls: AtomicUsize,
        requests: StdMutex<Vec<RecallRequest>>,
        response: MockResponse,
    }

    enum MockResponse {
        Value(Value),
        Error,
        Pending,
    }

    impl MockProvider {
        fn value(value: Value) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
                response: MockResponse::Value(value),
            }
        }
    }

    impl BellProvider for MockProvider {
        fn recall(
            &self,
            request: RecallRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Value, BellProviderError>> + Send + '_>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            Box::pin(async move {
                match &self.response {
                    MockResponse::Value(value) => Ok(value.clone()),
                    MockResponse::Error => Err(BellProviderError::Transport),
                    MockResponse::Pending => std::future::pending().await,
                }
            })
        }
    }

    fn config() -> BellRingsConfig {
        BellRingsConfig {
            enabled: true,
            mode: crate::config::BellMode::Live,
            provider: Some(BellProviderConfig {
                url: BellProviderUrl::parse("http://memory.example/mcp").unwrap(),
                scope: BellScope::parse("syne").unwrap(),
            }),
            max_semantic_distance: 0.3,
            max_bells: 3,
            deadline_ms: 300,
        }
    }

    fn message(targeting: MessageTargeting) -> MessageEvent {
        MessageEvent {
            chat_id: ChannelId::new(1),
            message_id: MessageId::new(2),
            user: "user".to_owned(),
            user_id: UserId::new(3),
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
        let provider = MockProvider::value(envelope(vec![]));
        let outcome = evaluate_message(
            &message(MessageTargeting::GuildDirected(MentionKind::DirectMention)),
            &config(),
            &provider,
        )
        .await;
        assert_eq!(outcome, ShadowOutcome::Success(vec![]));
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].scope, "syne");
        assert_eq!(requests[0].limit, 3);
    }

    #[tokio::test]
    async fn disabled_undirected_and_missing_provider_make_zero_calls() {
        let provider = MockProvider::value(envelope(vec![]));
        let mut disabled = config();
        disabled.enabled = false;
        assert_eq!(
            evaluate_message(
                &message(MessageTargeting::DirectMessage),
                &disabled,
                &provider
            )
            .await,
            ShadowOutcome::Skipped
        );
        assert_eq!(
            evaluate_message(&message(MessageTargeting::Ambient), &config(), &provider).await,
            ShadowOutcome::Skipped
        );
        let mut missing = config();
        missing.provider = None;
        assert_eq!(
            evaluate_message(
                &message(MessageTargeting::DirectMessage),
                &missing,
                &provider
            )
            .await,
            ShadowOutcome::Skipped
        );
        assert_eq!(provider.calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn all_scope_is_rejected_at_config_boundary() {
        let parsed = toml::from_str::<Config>(
            "[bell_rings]\nenabled=true\n[bell_rings.provider]\nurl='http://memory/mcp'\nscope='all'",
        );
        assert!(parsed.is_err());
    }

    #[test]
    fn bell_config_defaults_and_invalid_boundaries_are_non_vacuous() {
        let defaults = toml::from_str::<Config>("[bell_rings]").unwrap();
        assert!(!defaults.bell_rings.enabled);
        assert_eq!(defaults.bell_rings.max_semantic_distance, 0.3);
        assert_eq!(defaults.bell_rings.max_bells, 3);
        assert_eq!(defaults.bell_rings.deadline_ms, 300);

        for invalid in [
            "[bell_rings]\nmax_semantic_distance=nan",
            "[bell_rings]\nmax_semantic_distance=2.1",
            "[bell_rings]\nmax_bells=0",
            "[bell_rings]\nmax_bells=101",
            "[bell_rings]\ndeadline_ms=0",
            "[bell_rings]\ndeadline_ms=301",
            "[bell_rings]\nenabled=true",
            "[bell_rings.provider]\nurl='file:///tmp/memory'\nscope='syne'",
            "[bell_rings.provider]\nurl='https://user:pass@memory/mcp'\nscope='syne'",
        ] {
            assert!(toml::from_str::<Config>(invalid).is_err(), "{invalid}");
        }
    }

    #[tokio::test]
    async fn semantic_distance_maps_inverse_thirds_and_lexical_is_excluded() {
        let provider = MockProvider::value(envelope(vec![
            hit("a", "loud", 0.05, "semantic"),
            hit("b", "middle", 0.15, "both"),
            hit("c", "quiet", 0.28, "semantic"),
            hit("d", "lexical", -1.0, "lexical"),
            hit("e", "far", 0.31, "semantic"),
        ]));
        let outcome = evaluate_message(
            &message(MessageTargeting::DirectMessage),
            &config(),
            &provider,
        )
        .await;
        let ShadowOutcome::Success(bells) = outcome else {
            panic!("expected success");
        };
        assert_eq!(
            bells.iter().map(|bell| bell.loudness).collect::<Vec<_>>(),
            vec![3, 2, 1]
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
            let provider = MockProvider {
                calls: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
                response,
            };
            let original = NotificationEvent::Message(message(MessageTargeting::DirectMessage));
            let before = original.clone().into_notification();
            let (observed, bells) = evaluate_with_provider(original, &config(), &provider).await;
            assert_eq!(observed.into_notification(), before);
            assert!(bells.is_empty());
            assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
        }

        let provider = MockProvider {
            calls: AtomicUsize::new(0),
            requests: StdMutex::new(Vec::new()),
            response: MockResponse::Pending,
        };
        let mut short = config();
        short.deadline_ms = 1;
        let original = NotificationEvent::Message(message(MessageTargeting::DirectMessage));
        let before = original.clone().into_notification();
        let (observed, bells) = evaluate_with_provider(original, &short, &provider).await;
        assert_eq!(observed.into_notification(), before);
        assert!(bells.is_empty());
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn evaluate_returns_bells_without_injecting_metadata() {
        let provider =
            MockProvider::value(envelope(vec![hit("a", "memory-name", 0.1, "semantic")]));
        let original = NotificationEvent::Message(message(MessageTargeting::DirectMessage));
        let before = original.clone().into_notification();
        let (observed, bells) = evaluate_with_provider(original, &config(), &provider).await;
        assert_eq!(observed.into_notification(), before);
        assert_eq!(bells.len(), 1);
        assert_eq!(bells[0].memory_name, "memory-name");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn render_bells_produces_compact_format() {
        let bells = vec![
            Bell {
                provider_url: "http://memory/mcp".to_owned(),
                scope: "lain".to_owned(),
                recall_id: "r_test".to_owned(),
                memory_id: "a".to_owned(),
                memory_name: "person-pace".to_owned(),
                match_type: SemanticMatchType::Semantic,
                distance: 0.12,
                loudness: 3,
                provider_rank: 0,
            },
            Bell {
                provider_url: "http://memory/mcp".to_owned(),
                scope: "lain".to_owned(),
                recall_id: "r_test".to_owned(),
                memory_id: "b".to_owned(),
                memory_name: "feedback-no-platitudes".to_owned(),
                match_type: SemanticMatchType::Both,
                distance: 0.25,
                loudness: 2,
                provider_rank: 1,
            },
        ];
        assert_eq!(render_bells(&bells), "person-pace 12;feedback-no-platitudes 25");
    }

    #[tokio::test]
    async fn max_bells_truncation_is_deterministic() {
        let provider = MockProvider::value(envelope(vec![
            hit("c", "same", 0.1, "semantic"),
            hit("b", "same", 0.1, "semantic"),
            hit("a", "first", 0.05, "semantic"),
        ]));
        let mut limited = config();
        limited.max_bells = 2;
        let outcome = evaluate_message(
            &message(MessageTargeting::DirectMessage),
            &limited,
            &provider,
        )
        .await;
        let ShadowOutcome::Success(bells) = outcome else {
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
}
