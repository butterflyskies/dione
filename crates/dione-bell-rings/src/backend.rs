use crate::{BellProviderConfig, BellProviderUrl, BellScope, RecallOptions};
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
    /// The normalized endpoint used for this observation.
    pub provider_url: String,
    /// The supplied alias, falling back to the configured scope.
    pub provider_alias: String,
    /// The admitted hit scope, either the configured scope or global.
    pub scope: String,
    /// The nonempty recall identifier supplied by the provider.
    pub recall_id: String,
    /// The provider-supplied memory identifier.
    pub memory_id: String,
    /// The provider-supplied memory name.
    pub memory_name: String,
    /// The match type used for ranking and compact rendering.
    pub timbre: BellTimbre,
    /// The semantic distance, or zero for lexical matches.
    pub distance: f64,
    /// The admitted strength tier, between one and three.
    pub loudness: u8,
    /// The original hit index before ranking.
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
    /// Returns the compact match-type code.
    pub fn code(self) -> char {
        match self {
            BellTimbre::Semantic => 's',
            BellTimbre::Lexical => 'l',
            BellTimbre::Both => 'b',
        }
    }
}

/// Retrieval status for bell evaluation - orthogonal to bell results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BellStatus {
    /// Retrieval completed successfully (bells may still be empty).
    Ok,
    /// Some providers completed, others timed out. Partial results available.
    PartialTimeout,
    /// Some providers completed, others failed (non-timeout). Partial results available.
    PartialError,
    /// All retrieval failed with at least one timeout.
    Timeout,
    /// Retrieval failed due to provider error or malformed response.
    Error,
}

impl BellStatus {
    /// Returns the stable retrieval status code.
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

/// Evaluation of one query against a nonempty provider slice.
#[derive(Debug, Clone, PartialEq)]
pub struct BellEvaluation {
    /// The admitted, ranked and truncated observations.
    pub bells: Vec<Bell>,
    /// The retrieval result, independent of whether any bells remain.
    pub status: BellStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectOutcome {
    Ready,
    TimedOut,
    Error,
}

#[derive(Debug, Clone, PartialEq)]
enum BellOutcome {
    Success(Vec<Bell>),
    Timeout,
    ProviderError,
}

#[derive(Debug, Clone)]
struct RecallRequest {
    pub query: String,
    /// The admitted hit scope, either the configured scope or global.
    pub scope: BellScope,
    pub limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("{kind}")]
struct BellProviderError {
    kind: BellProviderErrorKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
enum BellProviderErrorKind {
    #[error("provider recall timed out")]
    TimedOut,
    #[error("provider transport failed")]
    Transport,
    #[error("provider returned malformed recall data")]
    MalformedResponse,
}

trait Provider: Send + Sync {
    fn ensure_connected(&self) -> Pin<Box<dyn Future<Output = ConnectOutcome> + Send + '_>>;
    fn recall(
        &self,
        request: RecallRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Value, BellProviderError>> + Send + '_>>;
}

struct MemoryMcpProvider {
    endpoint: String,
    client: Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
}

impl MemoryMcpProvider {
    fn new(endpoint: BellProviderUrl) -> Self {
        Self {
            endpoint: endpoint.as_str().to_owned(),
            client: Mutex::new(None),
        }
    }

    async fn ensure_connected(&self) -> ConnectOutcome {
        let mut client = self.client.lock().await;
        if client.as_ref().is_none_or(RunningService::is_closed) {
            let transport = StreamableHttpClientTransport::from_uri(self.endpoint.clone());
            match ClientInfo::default().serve(transport).await {
                Ok(connected) => {
                    *client = Some(connected);
                    ConnectOutcome::Ready
                }
                Err(e) => {
                    tracing::warn!(endpoint = %self.endpoint, error = ?e, "bell provider cold-start failed");
                    ConnectOutcome::Error
                }
            }
        } else {
            ConnectOutcome::Ready
        }
    }

    async fn recall_inner(&self, request: RecallRequest) -> Result<Value, BellProviderError> {
        let mut client = self.client.lock().await;
        if client.as_ref().is_none_or(RunningService::is_closed) {
            return Err(BellProviderError {
                kind: BellProviderErrorKind::Transport,
            });
        }

        let arguments = json!({
            "query": request.query,
            "scope": request.scope.as_str(),
            "limit": request.limit,
        });
        let arguments = serde_json::from_value::<Map<String, Value>>(arguments).map_err(|_| {
            BellProviderError {
                kind: BellProviderErrorKind::MalformedResponse,
            }
        })?;
        let result = match client
            .as_ref()
            .ok_or(BellProviderError {
                kind: BellProviderErrorKind::Transport,
            })?
            .call_tool(CallToolRequestParams::new("recall").with_arguments(arguments))
            .await
        {
            Ok(result) if result.is_error != Some(true) => result,
            Ok(_) => {
                return Err(BellProviderError {
                    kind: BellProviderErrorKind::MalformedResponse,
                });
            }
            Err(_) => {
                *client = None;
                return Err(BellProviderError {
                    kind: BellProviderErrorKind::Transport,
                });
            }
        };

        let text = result
            .content
            .iter()
            .find_map(|content| match &content.raw {
                RawContent::Text(text) => Some(text.text.as_str()),
                _ => None,
            })
            .ok_or(BellProviderError {
                kind: BellProviderErrorKind::MalformedResponse,
            })?;
        serde_json::from_str(text).map_err(|_| BellProviderError {
            kind: BellProviderErrorKind::MalformedResponse,
        })
    }
}

impl Provider for MemoryMcpProvider {
    fn ensure_connected(&self) -> Pin<Box<dyn Future<Output = ConnectOutcome> + Send + '_>> {
        Box::pin(MemoryMcpProvider::ensure_connected(self))
    }

    fn recall(
        &self,
        request: RecallRequest,
    ) -> Pin<Box<dyn Future<Output = Result<Value, BellProviderError>> + Send + '_>> {
        Box::pin(self.recall_inner(request))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProviderKey {
    endpoint: String,
    scope: String,
}

struct ProviderSlot {
    key: ProviderKey,
    provider: Arc<MemoryMcpProvider>,
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
    /// Creates an evaluator with an empty client cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Evaluate query text after the application has admitted it for recall.
    /// Returns `None` exactly when the provider slice is empty.
    pub async fn evaluate(
        &self,
        query: &str,
        providers: &[BellProviderConfig],
        options: &RecallOptions,
    ) -> Option<BellEvaluation> {
        if providers.is_empty() {
            return None;
        }
        let instances = self.providers_for(providers).await;
        let instances: Vec<_> = instances
            .into_iter()
            .map(|(config, provider)| (config, provider as Arc<dyn Provider>))
            .collect();
        Some(evaluate_providers(query, options, &instances).await)
    }

    async fn providers_for(
        &self,
        providers: &[BellProviderConfig],
    ) -> Vec<(BellProviderConfig, Arc<MemoryMcpProvider>)> {
        let mut slots = self.providers.lock().await;

        let wanted: std::collections::HashSet<ProviderKey> = providers
            .iter()
            .map(|p| ProviderKey {
                endpoint: p.url.as_str().to_owned(),
                scope: p.scope.as_str().to_owned(),
            })
            .collect();

        slots.retain(|s| wanted.contains(&s.key));

        let mut result = Vec::with_capacity(providers.len());
        for provider_config in providers {
            let key = ProviderKey {
                endpoint: provider_config.url.as_str().to_owned(),
                scope: provider_config.scope.as_str().to_owned(),
            };
            let existing = slots.iter().position(|s| s.key == key);
            let provider = if let Some(idx) = existing {
                slots[idx].provider.clone()
            } else {
                let provider = Arc::new(MemoryMcpProvider::new(provider_config.url.clone()));
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

async fn evaluate_providers(
    query: &str,
    options: &RecallOptions,
    providers: &[(BellProviderConfig, Arc<dyn Provider>)],
) -> BellEvaluation {
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
    let connect_futures: Vec<_> = providers
        .iter()
        .map(|(pc, p)| {
            let p = p.clone();
            let pc = pc.clone();
            async move {
                match tokio::time::timeout(CONNECT_TIMEOUT, p.ensure_connected()).await {
                    Ok(outcome) => (pc, outcome, p),
                    Err(_) => {
                        tracing::warn!(
                            endpoint = pc.url.as_str(),
                            "bell provider connection timed out after {}s",
                            CONNECT_TIMEOUT.as_secs()
                        );
                        (pc, ConnectOutcome::TimedOut, p)
                    }
                }
            }
        })
        .collect();
    let connect_results = join_all(connect_futures).await;
    let connect_timeout_count = connect_results
        .iter()
        .filter(|(_, o, _)| *o == ConnectOutcome::TimedOut)
        .count();
    let connect_error_count = connect_results
        .iter()
        .filter(|(_, o, _)| *o == ConnectOutcome::Error)
        .count();
    let connected: Vec<_> = connect_results
        .into_iter()
        .filter(|(_, o, _)| *o == ConnectOutcome::Ready)
        .map(|(pc, _, p)| (pc, p))
        .collect();

    if connected.is_empty() {
        record_outcome(&BellOutcome::ProviderError);
        let status = if connect_timeout_count > 0 && connect_error_count == 0 {
            BellStatus::Timeout
        } else if connect_error_count > 0 && connect_timeout_count == 0 {
            BellStatus::Error
        } else {
            BellStatus::Timeout
        };
        return BellEvaluation {
            bells: vec![],
            status,
        };
    }

    let deadline = Duration::from_millis(options.deadline_ms());
    let futures: Vec<_> = connected
        .iter()
        .map(|(provider_config, provider)| {
            let request = RecallRequest {
                query: query.to_owned(),
                scope: provider_config.scope.clone(),
                limit: options.max_bells(),
            };
            let pc = provider_config.clone();
            let p = provider.clone();
            async move {
                let result = tokio::time::timeout(deadline, p.recall(request)).await;
                match result {
                    Err(_) => (
                        pc,
                        Err(BellProviderError {
                            kind: BellProviderErrorKind::TimedOut,
                        }),
                    ),
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
                options.max_semantic_distance(),
                options.max_bells(),
            ) {
                Ok(bells) => {
                    any_ok = true;
                    all_bells.extend(bells);
                }
                Err(()) => {
                    any_malformed = true;
                }
            },
            Err(BellProviderError {
                kind: BellProviderErrorKind::TimedOut,
            }) => {
                any_timeout = true;
            }
            Err(BellProviderError {
                kind: BellProviderErrorKind::Transport,
            }) => {
                any_transport_error = true;
            }
            Err(BellProviderError {
                kind: BellProviderErrorKind::MalformedResponse,
            }) => {
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
    all_bells.truncate(options.max_bells());

    let any_connect_timeout = connect_timeout_count > 0;
    let any_connect_error = connect_error_count > 0;
    let any_timed_out = any_timeout || any_connect_timeout;
    let any_errored = any_transport_error || any_malformed || any_connect_error;
    let any_failure = any_timed_out || any_errored;
    let status = if any_ok && any_timed_out {
        BellStatus::PartialTimeout
    } else if any_ok && any_errored {
        BellStatus::PartialError
    } else if any_ok {
        BellStatus::Ok
    } else if any_timed_out && !any_errored {
        BellStatus::Timeout
    } else if any_errored && !any_timed_out {
        BellStatus::Error
    } else if any_failure {
        BellStatus::Timeout
    } else {
        BellStatus::Ok
    };

    let outcome = if any_ok || !any_failure {
        BellOutcome::Success(all_bells.clone())
    } else if any_timeout || any_connect_timeout {
        BellOutcome::Timeout
    } else {
        BellOutcome::ProviderError
    };
    record_outcome(&outcome);

    BellEvaluation {
        bells: all_bells,
        status,
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
        BellOutcome::Success(bells) => {
            tracing::info!(bell_count = bells.len(), "bell evaluation completed");
        }
        BellOutcome::Timeout => {
            tracing::warn!(outcome = "timeout", "bell evaluation degraded");
        }
        BellOutcome::ProviderError => {
            tracing::warn!(outcome = "provider_error", "bell evaluation degraded");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    enum Response {
        Value(Value),
        Error,
        Malformed,
        Pending,
    }
    struct MockProvider {
        connect: ConnectOutcome,
        response: Response,
        calls: AtomicUsize,
        requests: StdMutex<Vec<RecallRequest>>,
    }
    impl MockProvider {
        fn new(connect: ConnectOutcome, response: Response) -> Arc<Self> {
            Arc::new(Self {
                connect,
                response,
                calls: AtomicUsize::new(0),
                requests: StdMutex::new(vec![]),
            })
        }
    }
    impl Provider for MockProvider {
        fn ensure_connected(&self) -> Pin<Box<dyn Future<Output = ConnectOutcome> + Send + '_>> {
            Box::pin(async move { self.connect })
        }
        fn recall(
            &self,
            request: RecallRequest,
        ) -> Pin<Box<dyn Future<Output = Result<Value, BellProviderError>> + Send + '_>> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.requests.lock().unwrap().push(request);
            Box::pin(async move {
                match &self.response {
                    Response::Value(value) => Ok(value.clone()),
                    Response::Error => Err(BellProviderError {
                        kind: BellProviderErrorKind::Transport,
                    }),
                    Response::Malformed => Err(BellProviderError {
                        kind: BellProviderErrorKind::MalformedResponse,
                    }),
                    Response::Pending => std::future::pending().await,
                }
            })
        }
    }
    fn provider(scope: &str) -> BellProviderConfig {
        BellProviderConfig {
            url: BellProviderUrl::parse("http://memory.example/mcp").unwrap(),
            scope: BellScope::parse(scope).unwrap(),
            alias: None,
        }
    }
    fn hit(id: &str, name: &str, distance: f64, match_type: &str) -> Value {
        json!({"id": id, "name": name, "scope": "syne", "distance": distance, "match_type": match_type})
    }
    fn envelope(results: Vec<Value>) -> Value {
        json!({"recall_id": "r_test", "results": results, "count": results.len()})
    }
    async fn evaluate(
        response: Response,
        options: &RecallOptions,
    ) -> (BellEvaluation, Arc<MockProvider>) {
        let mock = MockProvider::new(ConnectOutcome::Ready, response);
        let providers = vec![(provider("syne"), mock.clone() as Arc<dyn Provider>)];
        (
            evaluate_providers("query text", options, &providers).await,
            mock,
        )
    }

    #[tokio::test]
    async fn query_calls_one_provider_with_exact_scope_and_limit() {
        let (result, mock) =
            evaluate(Response::Value(envelope(vec![])), &RecallOptions::default()).await;
        assert_eq!(
            result,
            BellEvaluation {
                bells: vec![],
                status: BellStatus::Ok
            }
        );
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].query, "query text");
        assert_eq!(requests[0].scope.as_str(), "syne");
        assert_eq!(requests[0].limit, 3);
    }

    #[tokio::test]
    async fn empty_provider_input_is_skipped() {
        assert_eq!(
            BellEvaluator::new()
                .evaluate("query text", &[], &RecallOptions::default())
                .await,
            None
        );
    }

    #[tokio::test]
    async fn loudness_tiers_and_timbre_mapping() {
        let (result, _) = evaluate(
            Response::Value(envelope(vec![
                hit("a", "loud", 0.05, "semantic"),
                hit("b", "middle", 0.15, "both"),
                hit("c", "quiet", 0.28, "semantic"),
                hit("d", "lexical-hit", -1.0, "lexical"),
                hit("e", "far", 0.31, "semantic"),
            ])),
            &RecallOptions::default(),
        )
        .await;
        assert_eq!(result.status, BellStatus::Ok);
        assert_eq!(
            result
                .bells
                .iter()
                .map(|bell| (bell.loudness, bell.timbre))
                .collect::<Vec<_>>(),
            vec![
                (3, BellTimbre::Semantic),
                (3, BellTimbre::Both),
                (1, BellTimbre::Lexical)
            ]
        );
        assert!(result.bells.iter().all(|bell| bell.recall_id == "r_test"));
    }

    #[tokio::test]
    async fn timeout_error_and_malformed_fail_open() {
        for response in [
            Response::Error,
            Response::Malformed,
            Response::Value(json!({"not": "recall"})),
            Response::Value(
                json!({"recall_id": "r_cross_scope", "results": [{"id": "x", "name": "cross-scope", "scope": "someone-else", "distance": 0.01, "match_type": "semantic"}]}),
            ),
            Response::Value(envelope(vec![hit("x", "unknown", 0.01, "mystery")])),
        ] {
            let (result, mock) = evaluate(response, &RecallOptions::default()).await;
            assert!(result.bells.is_empty());
            assert_eq!(result.status, BellStatus::Error);
            assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
        }
        let (result, mock) =
            evaluate(Response::Pending, &RecallOptions::new(0.3, 3, 1).unwrap()).await;
        assert!(result.bells.is_empty());
        assert_eq!(result.status, BellStatus::Timeout);
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn evaluate_returns_bells() {
        let (result, mock) = evaluate(
            Response::Value(envelope(vec![hit("a", "memory-name", 0.1, "semantic")])),
            &RecallOptions::default(),
        )
        .await;
        assert_eq!(result.bells.len(), 1);
        assert_eq!(result.bells[0].memory_name, "memory-name");
        assert_eq!(mock.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn max_bells_truncation_is_deterministic() {
        let (result, _) = evaluate(
            Response::Value(envelope(vec![
                hit("c", "same", 0.1, "semantic"),
                hit("b", "same", 0.1, "semantic"),
                hit("a", "first", 0.05, "semantic"),
            ])),
            &RecallOptions::new(0.3, 2, 300).unwrap(),
        )
        .await;
        assert_eq!(
            result
                .bells
                .iter()
                .map(|bell| bell.memory_id.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "b"]
        );
    }

    #[tokio::test]
    async fn lexical_bell_does_not_displace_louder_semantic_on_truncation() {
        let (result, _) = evaluate(
            Response::Value(envelope(vec![
                hit("semantic-hit", "loud-semantic", 0.05, "semantic"),
                hit("lexical-hit", "quiet-lexical", -1.0, "lexical"),
            ])),
            &RecallOptions::new(0.3, 1, 300).unwrap(),
        )
        .await;
        assert_eq!(result.bells.len(), 1);
        assert_eq!(result.bells[0].memory_id, "semantic-hit");
        assert_eq!(result.bells[0].loudness, 3);
        assert_eq!(result.bells[0].timbre, BellTimbre::Semantic);
    }

    #[tokio::test]
    async fn mixed_success_errors_and_timeouts_use_real_two_phase_algorithm() {
        for (connect, response, expected) in [
            (
                ConnectOutcome::Error,
                Response::Error,
                BellStatus::PartialError,
            ),
            (
                ConnectOutcome::TimedOut,
                Response::Error,
                BellStatus::PartialTimeout,
            ),
            (
                ConnectOutcome::Ready,
                Response::Error,
                BellStatus::PartialError,
            ),
            (
                ConnectOutcome::Ready,
                Response::Pending,
                BellStatus::PartialTimeout,
            ),
            (
                ConnectOutcome::Ready,
                Response::Malformed,
                BellStatus::PartialError,
            ),
        ] {
            let good = MockProvider::new(
                ConnectOutcome::Ready,
                Response::Value(envelope(vec![hit(
                    "survivor",
                    "surviving-memory",
                    0.1,
                    "semantic",
                )])),
            );
            let bad = MockProvider::new(connect, response);
            let providers = vec![
                (provider("syne"), good.clone() as Arc<dyn Provider>),
                (provider("other"), bad.clone() as Arc<dyn Provider>),
            ];
            let result = evaluate_providers(
                "query text",
                &RecallOptions::new(0.3, 3, 1).unwrap(),
                &providers,
            )
            .await;
            assert_eq!(result.status, expected);
            assert_eq!(result.bells.len(), 1);
            assert_eq!(result.bells[0].memory_id, "survivor");
            assert_eq!(good.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                bad.calls.load(Ordering::SeqCst),
                usize::from(connect == ConnectOutcome::Ready)
            );
        }
    }

    #[tokio::test]
    async fn total_mixed_failures_timeout_and_partial_timeout_precedes_error() {
        for successful in [false, true] {
            let mut providers: Vec<(BellProviderConfig, Arc<dyn Provider>)> = vec![
                (
                    provider("timeout"),
                    MockProvider::new(ConnectOutcome::TimedOut, Response::Error),
                ),
                (
                    provider("error"),
                    MockProvider::new(ConnectOutcome::Error, Response::Error),
                ),
            ];
            if successful {
                providers.push((
                    provider("syne"),
                    MockProvider::new(ConnectOutcome::Ready, Response::Value(envelope(vec![]))),
                ));
            }
            let result =
                evaluate_providers("query text", &RecallOptions::default(), &providers).await;
            assert_eq!(
                result.status,
                if successful {
                    BellStatus::PartialTimeout
                } else {
                    BellStatus::Timeout
                }
            );
            assert!(result.bells.is_empty());
        }
        let providers: Vec<(BellProviderConfig, Arc<dyn Provider>)> = vec![
            (
                provider("timeout"),
                MockProvider::new(ConnectOutcome::Ready, Response::Pending),
            ),
            (
                provider("error"),
                MockProvider::new(ConnectOutcome::Ready, Response::Error),
            ),
        ];
        assert_eq!(
            evaluate_providers(
                "query text",
                &RecallOptions::new(0.3, 3, 1).unwrap(),
                &providers
            )
            .await
            .status,
            BellStatus::Timeout
        );
    }

    #[tokio::test]
    async fn tied_provider_results_preserve_input_order_and_global_truncation() {
        let mut first = provider("syne");
        first.alias = Some("first".to_owned());
        let mut second = provider("syne");
        second.alias = Some("second".to_owned());
        let providers: Vec<(BellProviderConfig, Arc<dyn Provider>)> = vec![
            (
                first,
                MockProvider::new(
                    ConnectOutcome::Ready,
                    Response::Value(envelope(vec![hit("same", "same", 0.1, "semantic")])),
                ),
            ),
            (
                second,
                MockProvider::new(
                    ConnectOutcome::Ready,
                    Response::Value(envelope(vec![hit("same", "same", 0.1, "semantic")])),
                ),
            ),
        ];
        let result = evaluate_providers("query text", &RecallOptions::default(), &providers).await;
        assert_eq!(
            result
                .bells
                .iter()
                .map(|b| b.provider_alias.as_str())
                .collect::<Vec<_>>(),
            vec!["first", "second"]
        );
        let result = evaluate_providers(
            "query text",
            &RecallOptions::new(0.3, 1, 300).unwrap(),
            &providers,
        )
        .await;
        assert_eq!(result.bells.len(), 1);
        assert_eq!(result.bells[0].provider_alias, "first");
    }

    #[tokio::test]
    async fn cache_reuses_endpoint_scope_retains_current_alias_and_evicts() {
        let registry = BellEvaluator::new();
        let a = provider("syne");
        let b = provider("other");
        let original = registry
            .providers_for(&[a.clone(), b.clone(), a.clone()])
            .await;
        assert!(Arc::ptr_eq(&original[0].1, &original[2].1));
        assert!(!Arc::ptr_eq(&original[0].1, &original[1].1));
        let mut renamed = a.clone();
        renamed.alias = Some("renamed".to_owned());
        let reordered = registry.providers_for(&[b.clone(), renamed]).await;
        assert!(Arc::ptr_eq(&original[1].1, &reordered[0].1));
        assert!(Arc::ptr_eq(&original[0].1, &reordered[1].1));
        assert_eq!(reordered[1].0.alias(), "renamed");
        registry.providers_for(&[a]).await;
        let recreated = registry.providers_for(&[b]).await;
        assert!(!Arc::ptr_eq(&original[1].1, &recreated[0].1));
        assert_eq!(registry.providers.lock().await.len(), 1);
    }

    #[tokio::test]
    async fn multi_provider_hanging_connect_is_bounded() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let hanging = tokio::spawn(async move {
            loop {
                let (_socket, _) = listener.accept().await.unwrap();
                tokio::time::sleep(Duration::from_secs(600)).await;
            }
        });

        let registry = BellEvaluator::default();
        let configs = vec![BellProviderConfig {
            url: BellProviderUrl::parse(format!("http://{addr}/mcp")).unwrap(),
            scope: BellScope::parse("test").unwrap(),
            alias: None,
        }];
        let start = tokio::time::Instant::now();
        let BellEvaluation { bells, status } = registry
            .evaluate("query text", &configs, &RecallOptions::default())
            .await
            .unwrap();
        let elapsed = start.elapsed();
        hanging.abort();
        assert!(hanging.await.unwrap_err().is_cancelled());

        assert!(
            elapsed < Duration::from_secs(10),
            "hanging connect must be bounded, took {:?}",
            elapsed
        );
        assert!(
            bells.is_empty(),
            "no bells from a provider that never connected"
        );
        assert_eq!(
            status,
            BellStatus::Timeout,
            "hanging provider (timeout, not error) should produce Timeout, got {status:?}"
        );
    }
    #[tokio::test]
    async fn multi_provider_connect_error_reports_error_not_timeout() {
        let registry = BellEvaluator::default();
        let configs = vec![BellProviderConfig {
            url: BellProviderUrl::parse("http://127.0.0.1:1/mcp").unwrap(),
            scope: BellScope::parse("test").unwrap(),
            alias: None,
        }];
        let BellEvaluation { bells, status } = registry
            .evaluate("query text", &configs, &RecallOptions::default())
            .await
            .unwrap();

        assert!(bells.is_empty(), "no bells from a refused connection");
        assert_eq!(
            status,
            BellStatus::Error,
            "immediate connection failure should produce Error, got {status:?}"
        );
    }
    #[tokio::test]
    async fn multi_provider_single_refused_scope_reports_error() {
        let registry = BellEvaluator::default();
        let configs = vec![BellProviderConfig {
            url: BellProviderUrl::parse("http://127.0.0.1:1/mcp").unwrap(),
            scope: BellScope::parse("error-scope").unwrap(),
            alias: None,
        }];
        let status = registry
            .evaluate("query text", &configs, &RecallOptions::default())
            .await
            .unwrap()
            .status;

        assert_eq!(
            status,
            BellStatus::Error,
            "all-error (no timeouts) should produce Error, got {status:?}"
        );
    }
}
