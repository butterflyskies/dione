//! Inbound memory bell evaluation.
//!
//! Evaluates incoming directed messages against a configured memory-mcp
//! provider. In shadow mode, results are logged without affecting delivery.
//! In live mode, admitted bells are rendered into compact metadata and
//! injected into the notification event.

use crate::{
    config::{BellProviderConfig, BellRingsConfig, BellTrigger},
    discord::events::{MessageEvent, NotificationEvent},
};
use futures_util::future::join_all;
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
    pub provider_alias: String,
    pub scope: String,
    pub recall_id: String,
    pub memory_id: String,
    pub memory_name: String,
    pub timbre: BellTimbre,
    pub distance: f64,
    pub loudness: u8,
    pub provider_rank: usize,
}

/// Match type / "timbre" of a bell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BellTimbre {
    /// Semantic similarity match.
    Semantic,
    /// Lexical/BM25 match.
    Lexical,
    /// Both semantic and lexical matched.
    Both,
}

impl BellTimbre {
    pub fn code(self) -> char {
        match self {
            BellTimbre::Semantic => 's',
            BellTimbre::Lexical => 'l',
            BellTimbre::Both => 'b',
        }
    }
}

/// Retrieval status for bell evaluation — orthogonal to bell results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BellStatus {
    /// Retrieval completed successfully (bells may still be empty).
    Ok,
    /// Some providers completed, others timed out. Partial results available.
    PartialTimeout,
    /// Some providers completed, others failed (non-timeout). Partial results available.
    PartialError,
    /// Total deadline elapsed before retrieval completed.
    Timeout,
    /// Retrieval failed due to provider error or malformed response.
    Error,
}

impl BellStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            BellStatus::Ok => "ok",
            BellStatus::PartialTimeout => "partial_timeout",
            BellStatus::PartialError => "partial_error",
            BellStatus::Timeout => "timeout",
            BellStatus::Error => "error",
        }
    }
}

/// Result of one bell evaluation.
#[derive(Debug, Clone, PartialEq)]
pub enum BellOutcome {
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

impl BellOutcome {
    /// The retrieval status for this outcome — orthogonal to whether any bells rang.
    pub fn status(&self) -> Option<BellStatus> {
        match self {
            BellOutcome::Skipped => None,
            BellOutcome::Success(_) => Some(BellStatus::Ok),
            BellOutcome::Timeout => Some(BellStatus::Timeout),
            BellOutcome::ProviderError | BellOutcome::MalformedResponse => Some(BellStatus::Error),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecallRequest {
    pub query: String,
    pub scope: String,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BellProviderError {
    TimedOut,
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

    /// Ensure the client is connected. Called outside the per-message deadline
    /// so cold starts don't eat into recall time. Returns true if ready.
    async fn ensure_connected(&self) -> bool {
        let mut client = self.client.lock().await;
        if client.as_ref().is_none_or(RunningService::is_closed) {
            let transport = StreamableHttpClientTransport::from_uri(self.endpoint.clone());
            match ClientInfo::default().serve(transport).await {
                Ok(connected) => {
                    *client = Some(connected);
                    true
                }
                Err(_) => {
                    tracing::warn!(endpoint = %self.endpoint, "bell provider cold-start failed");
                    false
                }
            }
        } else {
            true
        }
    }

    async fn recall_inner(&self, request: RecallRequest) -> Result<Value, BellProviderError> {
        let mut client = self.client.lock().await;
        // At this point the client should already be connected via
        // ensure_connected(). If not (race or prior failure), skip.
        if client.as_ref().is_none_or(RunningService::is_closed) {
            return Err(BellProviderError::Transport);
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
            Ok(_) => {
                // Tool-call error — session is fine, tool returned an error.
                return Err(BellProviderError::MalformedResponse);
            }
            Err(_) => {
                // Transport/session error — tear down so next call reconnects.
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

// Pre-warming was stripped in favor of lazy-init. See commit 76ac05c for the
// WarmState machine if pre-warming is needed later.

/// Cache key for provider sessions. Two config entries with the same URL but
/// different scopes get independent sessions so fan-out is genuinely concurrent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderKey {
    endpoint: String,
    scope: String,
}

struct ProviderSlot {
    key: ProviderKey,
    provider: Arc<MemoryMcpProvider>,
}

/// Render admitted bells into the compact wire format with full provenance.
///
/// Format: `"3s lain/person-pace@personal d=0.12 id=a rid=r_test;1l shared/foo@cc d=-1.00 id=b rid=r_test2"`
/// — `{loudness}{timbre} {scope}/{name}@{provider_alias} d={distance} id={memory_id} rid={recall_id}`.
pub fn render_bells(bells: &[Bell]) -> String {
    bells
        .iter()
        .map(|bell| {
            format!(
                "{}{} {}/{}@{} d={} id={} rid={}",
                bell.loudness,
                bell.timbre.code(),
                bell.scope,
                bell.memory_name,
                bell.provider_alias,
                bell.distance,
                bell.memory_id,
                bell.recall_id,
            )
        })
        .collect::<Vec<_>>()
        .join(";")
}

/// Owns reconnectable providers used by the inbound delivery loop.
pub struct BellEvaluator {
    providers: Mutex<Vec<ProviderSlot>>,
}

impl Default for BellEvaluator {
    fn default() -> Self {
        Self {
            providers: Mutex::new(Vec::new()),
        }
    }
}

impl BellEvaluator {
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluates one event and returns bells (if any) alongside it plus retrieval status.
    pub async fn evaluate(
        &self,
        event: NotificationEvent,
        config: &BellRingsConfig,
    ) -> (NotificationEvent, Vec<Bell>, Option<BellStatus>) {
        let eligible = match &event {
            NotificationEvent::Message(message) if config.enabled => {
                let channel_id = message.chat_id.get().to_string();
                match config.trigger_for_channel(&channel_id) {
                    BellTrigger::Directed => message.targeting.is_directed(),
                    BellTrigger::All => true,
                }
            }
            _ => false,
        };
        if eligible && !config.providers.is_empty() {
            let provider_instances = self.providers_for(config).await;
            if !provider_instances.is_empty() {
                return evaluate_multi_provider(event, config, &provider_instances).await;
            }
        }
        record_outcome(&BellOutcome::Skipped);
        (event, vec![], None)
    }

    async fn providers_for(
        &self,
        config: &BellRingsConfig,
    ) -> Vec<(BellProviderConfig, Arc<MemoryMcpProvider>)> {
        let mut slots = self.providers.lock().await;

        // Build the set of keys the current config needs.
        let wanted: std::collections::HashSet<ProviderKey> = config
            .providers
            .iter()
            .map(|p| ProviderKey {
                endpoint: p.url.as_str().to_owned(),
                scope: p.scope.as_str().to_owned(),
            })
            .collect();

        // Evict slots whose key is no longer in the config.
        slots.retain(|s| wanted.contains(&s.key));

        let mut result = Vec::with_capacity(config.providers.len());
        for provider_config in &config.providers {
            let key = ProviderKey {
                endpoint: provider_config.url.as_str().to_owned(),
                scope: provider_config.scope.as_str().to_owned(),
            };
            let existing = slots.iter().position(|s| s.key == key);
            let provider = if let Some(idx) = existing {
                slots[idx].provider.clone()
            } else {
                let provider = Arc::new(MemoryMcpProvider::new(key.endpoint.clone()));
                slots.push(ProviderSlot {
                    key,
                    provider: provider.clone(),
                });
                provider
            };
            result.push((provider_config.clone(), provider));
        }
        result
    }
}

/// Fan out recall to multiple providers concurrently, merge results.
async fn evaluate_multi_provider(
    event: NotificationEvent,
    config: &BellRingsConfig,
    providers: &[(BellProviderConfig, Arc<MemoryMcpProvider>)],
) -> (NotificationEvent, Vec<Bell>, Option<BellStatus>) {
    let message = match &event {
        NotificationEvent::Message(m) => m,
        _ => {
            record_outcome(&BellOutcome::Skipped);
            return (event, vec![], None);
        }
    };

    // Phase 1: ensure all providers are connected (outside the per-message
    // deadline). Each provider gets a bounded timeout so a hanging handshake
    // cannot stall the live inbound loop. Providers that fail or time out are
    // skipped for this message and reported in aggregate status.
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    let connect_futures: Vec<_> = providers
        .iter()
        .map(|(pc, p)| {
            let p = p.clone();
            let pc = pc.clone();
            async move {
                match tokio::time::timeout(CONNECT_TIMEOUT, p.ensure_connected()).await {
                    Ok(ready) => (pc, ready, p),
                    Err(_) => {
                        tracing::warn!(
                            endpoint = pc.url.as_str(),
                            "bell provider connection timed out after {}s",
                            CONNECT_TIMEOUT.as_secs()
                        );
                        (pc, false, p)
                    }
                }
            }
        })
        .collect();
    let connect_results = join_all(connect_futures).await;
    let timed_out_count = connect_results
        .iter()
        .filter(|(_, ready, _)| !*ready)
        .count();
    let connected: Vec<_> = connect_results
        .into_iter()
        .filter(|(_, ready, _)| *ready)
        .map(|(pc, _, p)| (pc, p))
        .collect();

    if connected.is_empty() {
        record_outcome(&BellOutcome::ProviderError);
        return (event, vec![], Some(BellStatus::Error));
    }

    // Phase 2: recall within the per-message deadline. Init cost is already paid.
    let deadline = Duration::from_millis(config.deadline_ms);
    let futures: Vec<_> = connected
        .iter()
        .map(|(provider_config, provider)| {
            let request = RecallRequest {
                query: message.content.clone(),
                scope: provider_config.scope.as_str().to_owned(),
                limit: config.max_bells,
            };
            let pc = provider_config.clone();
            let p = provider.clone();
            async move {
                let result = tokio::time::timeout(deadline, p.recall(request)).await;
                match result {
                    Err(_) => (pc, Err(BellProviderError::TimedOut)),
                    Ok(r) => (pc, r),
                }
            }
        })
        .collect();

    let results = join_all(futures).await;

    let mut all_bells = Vec::new();
    let mut any_ok = false;
    let mut any_timeout = false;
    let mut any_transport_error = false;
    let mut any_malformed = false;

    for (provider_config, result) in results {
        match result {
            Ok(response) => match parse_bells(
                response,
                &provider_config,
                config.max_semantic_distance,
                config.max_bells,
            ) {
                Ok(bells) => {
                    any_ok = true;
                    all_bells.extend(bells);
                }
                Err(()) => {
                    any_malformed = true;
                }
            },
            Err(BellProviderError::TimedOut) => {
                any_timeout = true;
            }
            Err(BellProviderError::Transport) => {
                any_transport_error = true;
            }
            Err(BellProviderError::MalformedResponse) => {
                any_malformed = true;
            }
        }
    }

    all_bells.sort_by(|left, right| {
        right
            .loudness
            .cmp(&left.loudness)
            .then_with(|| left.distance.total_cmp(&right.distance))
            .then_with(|| left.memory_name.cmp(&right.memory_name))
            .then_with(|| left.memory_id.cmp(&right.memory_id))
    });
    all_bells.truncate(config.max_bells);

    let any_connect_timeout = timed_out_count > 0;
    let any_failure = any_timeout || any_transport_error || any_malformed || any_connect_timeout;
    let status = if any_ok && (any_timeout || any_connect_timeout) {
        Some(BellStatus::PartialTimeout)
    } else if any_ok && any_failure {
        Some(BellStatus::PartialError)
    } else if any_ok {
        Some(BellStatus::Ok)
    } else if any_timeout || any_connect_timeout {
        Some(BellStatus::Timeout)
    } else if any_failure {
        Some(BellStatus::Error)
    } else {
        Some(BellStatus::Ok)
    };

    let outcome = if any_ok || !any_failure {
        BellOutcome::Success(all_bells.clone())
    } else if any_timeout || any_connect_timeout {
        BellOutcome::Timeout
    } else {
        BellOutcome::ProviderError
    };
    record_outcome(&outcome);

    (event, all_bells, status)
}

/// Testable evaluation seam. Returns the event, admitted bells, and retrieval status.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) async fn evaluate_with_provider<P: BellProvider + ?Sized>(
    event: NotificationEvent,
    config: &BellRingsConfig,
    provider: &P,
) -> (NotificationEvent, Vec<Bell>, Option<BellStatus>) {
    let outcome = match &event {
        NotificationEvent::Message(message) => evaluate_message(message, config, provider).await,
        _ => BellOutcome::Skipped,
    };
    record_outcome(&outcome);
    let status = outcome.status();
    let bells = match outcome {
        BellOutcome::Success(bells) => bells,
        _ => vec![],
    };
    (event, bells, status)
}

/// Computes bells for one message against a single provider.
///
/// Used by the test seam; production uses `evaluate_multi_provider`.
pub async fn evaluate_message<P: BellProvider + ?Sized>(
    message: &MessageEvent,
    config: &BellRingsConfig,
    provider: &P,
) -> BellOutcome {
    let provider_config = match config.providers.first() {
        Some(p) => p,
        None => return BellOutcome::Skipped,
    };
    if !config.enabled || config.max_bells == 0 {
        return BellOutcome::Skipped;
    }
    let channel_id = message.chat_id.get().to_string();
    let should_ring = match config.trigger_for_channel(&channel_id) {
        BellTrigger::Directed => message.targeting.is_directed(),
        BellTrigger::All => true,
    };
    if !should_ring {
        return BellOutcome::Skipped;
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
        Err(_) => return BellOutcome::Timeout,
        Ok(Err(BellProviderError::TimedOut)) => return BellOutcome::Timeout,
        Ok(Err(BellProviderError::Transport)) => return BellOutcome::ProviderError,
        Ok(Err(BellProviderError::MalformedResponse)) => {
            return BellOutcome::MalformedResponse;
        }
        Ok(Ok(response)) => response,
    };

    match parse_bells(
        response,
        provider_config,
        config.max_semantic_distance,
        config.max_bells,
    ) {
        Ok(bells) => BellOutcome::Success(bells),
        Err(()) => BellOutcome::MalformedResponse,
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
        let (timbre, effective_loudness) = match hit.match_type.as_deref().unwrap_or("semantic") {
            "semantic" => {
                if !hit.distance.is_finite() || hit.distance < 0.0 || hit.distance > max_distance {
                    continue;
                }
                (BellTimbre::Semantic, loudness(hit.distance, max_distance))
            }
            "both" => {
                if !hit.distance.is_finite() || hit.distance < 0.0 || hit.distance > max_distance {
                    continue;
                }
                let semantic_loudness = loudness(hit.distance, max_distance);
                // Both = semantic + lexical(1), capped at 3.
                (BellTimbre::Both, (semantic_loudness + 1).min(3))
            }
            "lexical" => {
                // Lexical-only: distance -1.0 is a sentinel, not strength.
                // Flat loudness 1 per spec.
                (BellTimbre::Lexical, 1)
            }
            _ => return Err(()),
        };
        bells.push(Bell {
            provider_url: provider.url.as_str().to_owned(),
            provider_alias: provider.alias().to_owned(),
            scope: hit.scope,
            recall_id: envelope.recall_id.clone(),
            memory_id: hit.id,
            memory_name: hit.name,
            timbre,
            distance: if timbre == BellTimbre::Lexical {
                0.0
            } else {
                hit.distance
            },
            loudness: effective_loudness,
            provider_rank,
        });
    }
    bells.sort_by(|left, right| {
        // Sort by loudness descending (loudest first), then distance ascending
        // as tiebreaker among same-loudness bells. This prevents lexical bells
        // (loudness 1, synthetic distance 0.0) from displacing louder semantic
        // hits when max_bells truncation applies.
        right
            .loudness
            .cmp(&left.loudness)
            .then_with(|| left.distance.total_cmp(&right.distance))
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

fn record_outcome(outcome: &BellOutcome) {
    match outcome {
        BellOutcome::Skipped => {}
        BellOutcome::Success(bells) => {
            tracing::info!(bell_count = bells.len(), "bell evaluation completed");
        }
        BellOutcome::Timeout => {
            tracing::warn!(outcome = "timeout", "bell evaluation degraded");
        }
        BellOutcome::ProviderError => {
            tracing::warn!(outcome = "provider_error", "bell evaluation degraded");
        }
        BellOutcome::MalformedResponse => {
            tracing::warn!(outcome = "malformed_response", "bell evaluation degraded");
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
            providers: vec![BellProviderConfig {
                url: BellProviderUrl::parse("http://memory.example/mcp").unwrap(),
                scope: BellScope::parse("syne").unwrap(),
                alias: None,
            }],
            trigger: crate::config::BellTrigger::Directed,
            channel_overrides: vec![],
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
        let provider = MockProvider::value(envelope(vec![]));
        let outcome = evaluate_message(
            &message(MessageTargeting::GuildDirected(MentionKind::DirectMention)),
            &config(),
            &provider,
        )
        .await;
        assert_eq!(outcome, BellOutcome::Success(vec![]));
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
            BellOutcome::Skipped
        );
        assert_eq!(
            evaluate_message(&message(MessageTargeting::Ambient), &config(), &provider).await,
            BellOutcome::Skipped
        );
        let mut missing = config();
        missing.providers = vec![];
        assert_eq!(
            evaluate_message(
                &message(MessageTargeting::DirectMessage),
                &missing,
                &provider
            )
            .await,
            BellOutcome::Skipped
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
        assert_eq!(defaults.bell_rings.max_semantic_distance, 0.3);
        assert_eq!(defaults.bell_rings.max_bells, 3);
        assert_eq!(defaults.bell_rings.deadline_ms, 300);
        assert_eq!(
            defaults.bell_rings.trigger,
            crate::config::BellTrigger::Directed
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
        let provider = MockProvider::value(envelope(vec![
            hit("a", "loud", 0.05, "semantic"),
            hit("b", "middle", 0.15, "both"),
            hit("c", "quiet", 0.28, "semantic"),
            hit("d", "lexical-hit", -1.0, "lexical"),
            hit("e", "far", 0.31, "semantic"),
        ]));
        let outcome = evaluate_message(
            &message(MessageTargeting::DirectMessage),
            &config(),
            &provider,
        )
        .await;
        let BellOutcome::Success(bells) = outcome else {
            panic!("expected success");
        };
        // max_bells=3, sorted by loudness descending then distance ascending.
        // semantic(a)=3 dist=0.05, both(b)=3 dist=0.15, lexical(d)=1 dist=0.0
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
            let provider = MockProvider {
                calls: AtomicUsize::new(0),
                requests: StdMutex::new(Vec::new()),
                response,
            };
            let original = NotificationEvent::Message(message(MessageTargeting::DirectMessage));
            let before = original.clone().into_notification();
            let (observed, bells, _status) =
                evaluate_with_provider(original, &config(), &provider).await;
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
        let (observed, bells, _status) = evaluate_with_provider(original, &short, &provider).await;
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
        let (observed, bells, _status) =
            evaluate_with_provider(original, &config(), &provider).await;
        assert_eq!(observed.into_notification(), before);
        assert_eq!(bells.len(), 1);
        assert_eq!(bells[0].memory_name, "memory-name");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn render_bells_produces_compact_format_with_provenance() {
        let bells = vec![
            Bell {
                provider_url: "http://memory/mcp".to_owned(),
                provider_alias: "personal".to_owned(),
                scope: "lain".to_owned(),
                recall_id: "r_test".to_owned(),
                memory_id: "a".to_owned(),
                memory_name: "person-pace".to_owned(),
                timbre: BellTimbre::Semantic,
                distance: 0.12,
                loudness: 3,
                provider_rank: 0,
            },
            Bell {
                provider_url: "http://cc/mcp".to_owned(),
                provider_alias: "cc".to_owned(),
                scope: "shared".to_owned(),
                recall_id: "r_test2".to_owned(),
                memory_id: "b".to_owned(),
                memory_name: "construct-cosmology".to_owned(),
                timbre: BellTimbre::Both,
                distance: 0.25,
                loudness: 2,
                provider_rank: 0,
            },
        ];
        assert_eq!(
            render_bells(&bells),
            "3s lain/person-pace@personal d=0.12 id=a rid=r_test;2b shared/construct-cosmology@cc d=0.25 id=b rid=r_test2"
        );
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
        let BellOutcome::Success(bells) = outcome else {
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
        let provider = MockProvider::value(envelope(vec![
            hit("semantic-hit", "loud-semantic", 0.05, "semantic"),
            hit("lexical-hit", "quiet-lexical", -1.0, "lexical"),
        ]));
        let mut limited = config();
        limited.max_bells = 1;
        let outcome = evaluate_message(
            &message(MessageTargeting::DirectMessage),
            &limited,
            &provider,
        )
        .await;
        let BellOutcome::Success(bells) = outcome else {
            panic!("expected success");
        };
        assert_eq!(bells.len(), 1);
        assert_eq!(bells[0].memory_id, "semantic-hit");
        assert_eq!(bells[0].loudness, 3);
        assert_eq!(bells[0].timbre, BellTimbre::Semantic);
    }

    #[tokio::test]
    async fn all_trigger_rings_on_ambient_messages() {
        let provider = MockProvider::value(envelope(vec![hit("a", "found", 0.1, "semantic")]));
        let mut all_config = config();
        all_config.trigger = crate::config::BellTrigger::All;
        let outcome =
            evaluate_message(&message(MessageTargeting::Ambient), &all_config, &provider).await;
        let BellOutcome::Success(bells) = outcome else {
            panic!("expected success on ambient message with All trigger");
        };
        assert_eq!(bells.len(), 1);
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn channel_override_trigger_takes_precedence() {
        let provider = MockProvider::value(envelope(vec![hit("a", "found", 0.1, "semantic")]));
        let mut override_config = config();
        override_config.trigger = crate::config::BellTrigger::Directed;
        override_config.channel_overrides = vec![crate::config::BellChannelOverride {
            channel_id: "1".to_owned(),
            trigger: crate::config::BellTrigger::All,
        }];
        let outcome = evaluate_message(
            &message(MessageTargeting::Ambient),
            &override_config,
            &provider,
        )
        .await;
        let BellOutcome::Success(bells) = outcome else {
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
        let provider = MockProvider::value(envelope(vec![hit("a", "found", 0.1, "semantic")]));
        let mut override_config = config();
        override_config.trigger = crate::config::BellTrigger::All;
        override_config.channel_overrides = vec![crate::config::BellChannelOverride {
            channel_id: "1".to_owned(),
            trigger: crate::config::BellTrigger::Directed,
        }];
        let outcome = evaluate_message(
            &message(MessageTargeting::Ambient),
            &override_config,
            &provider,
        )
        .await;
        assert_eq!(
            outcome,
            BellOutcome::Skipped,
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
    fn render_bells_preserves_distance_precision() {
        let bells = vec![Bell {
            loudness: 1,
            timbre: BellTimbre::Semantic,
            scope: "lain".to_owned(),
            memory_name: "test".to_owned(),
            provider_alias: "personal".to_owned(),
            provider_url: "http://localhost/mcp".to_owned(),
            provider_rank: 0,
            distance: 0.004,
            memory_id: "a".to_owned(),
            recall_id: "r_1".to_owned(),
        }];
        let rendered = render_bells(&bells);
        assert!(
            rendered.contains("d=0.004"),
            "distance 0.004 must not be rounded to 0.00, got: {rendered}"
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
        assert_eq!(cfg.bell_rings.trigger, crate::config::BellTrigger::All);
    }

    #[tokio::test]
    async fn multi_provider_hanging_connect_is_bounded() {
        // A provider pointing at a listener that never completes the MCP
        // handshake must not stall the inbound loop. The 5s CONNECT_TIMEOUT
        // should fire and the provider should be skipped.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        // Accept but never send data — simulates a hanging handshake.
        tokio::spawn(async move {
            loop {
                let (_socket, _) = listener.accept().await.unwrap();
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
        });

        let registry = BellEvaluator::default();
        let cfg = BellRingsConfig {
            enabled: true,
            mode: crate::config::BellMode::Live,
            providers: vec![BellProviderConfig {
                url: BellProviderUrl::parse(&format!("http://{addr}/mcp")).unwrap(),
                scope: BellScope::parse("test").unwrap(),
                alias: None,
            }],
            trigger: crate::config::BellTrigger::Directed,
            channel_overrides: vec![],
            max_semantic_distance: 0.3,
            max_bells: 3,
            deadline_ms: 300,
        };
        let providers = registry.providers_for(&cfg).await;
        let event = NotificationEvent::Message(message(MessageTargeting::GuildDirected(
            MentionKind::DirectMention,
        )));

        let start = tokio::time::Instant::now();
        let (_, bells, status) = evaluate_multi_provider(event, &cfg, &providers).await;
        let elapsed = start.elapsed();

        assert!(
            elapsed < Duration::from_secs(10),
            "hanging connect must be bounded, took {:?}",
            elapsed
        );
        assert!(
            bells.is_empty(),
            "no bells from a provider that never connected"
        );
        assert!(
            matches!(status, Some(BellStatus::Timeout) | Some(BellStatus::Error)),
            "hanging/failing provider should produce Timeout or Error, got {status:?}"
        );
    }
}
