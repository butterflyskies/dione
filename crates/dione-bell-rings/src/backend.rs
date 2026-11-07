use crate::diagnostic_http::DiagnosticHttpClient;
use crate::{BellProviderConfig, BellProviderUrl, BellScope, RecallOptions};
use futures_util::future::join_all;
use rmcp::{
    ServiceExt,
    model::{CallToolRequestParams, ClientInfo, RawContent},
    service::{RoleClient, RunningService},
    transport::{
        StreamableHttpClientTransport, streamable_http_client::StreamableHttpClientTransportConfig,
    },
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
    endpoint: BellProviderUrl,
    client: Mutex<Option<RunningService<RoleClient, ClientInfo>>>,
}

impl MemoryMcpProvider {
    fn new(endpoint: BellProviderUrl) -> Self {
        Self {
            endpoint,
            client: Mutex::new(None),
        }
    }

    async fn ensure_connected(&self) -> ConnectOutcome {
        let mut client = self.client.lock().await;
        if client.as_ref().is_none_or(RunningService::is_closed) {
            // Match rmcp's default HTTP client, including disabled idle pooling.
            let http = reqwest_mcp::Client::builder()
                .pool_max_idle_per_host(0)
                .build()
                .expect("failed to build default reqwest client");
            let transport = StreamableHttpClientTransport::with_client(
                DiagnosticHttpClient(http),
                StreamableHttpClientTransportConfig::with_uri(self.endpoint.as_str().to_owned()),
            );
            match ClientInfo::default().serve(transport).await {
                Ok(connected) => {
                    *client = Some(connected);
                    ConnectOutcome::Ready
                }
                Err(_) => {
                    // Transport errors may embed the original credential-bearing URL.
                    tracing::warn!(endpoint = self.endpoint.diagnostic_endpoint(), error = %BellProviderErrorKind::Transport, "bell provider cold-start failed");
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
                            endpoint = pc.url.diagnostic_endpoint(),
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
    use reqwest_mcp::header::{HeaderName, HeaderValue};
    use rmcp::transport::streamable_http_client::{
        StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
    };
    use std::collections::HashMap;
    use std::sync::{
        Mutex as StdMutex,
        atomic::{AtomicUsize, Ordering},
    };

    #[derive(Clone)]
    struct DiagnosticBuffer(Arc<StdMutex<Vec<u8>>>);
    impl std::io::Write for DiagnosticBuffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn capture_diagnostics() -> (tracing::subscriber::DefaultGuard, DiagnosticBuffer) {
        let buffer = DiagnosticBuffer(Arc::new(StdMutex::new(Vec::new())));
        let writer = buffer.clone();
        let subscriber = tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .without_time()
            .with_ansi(false)
            .with_writer(move || writer.clone())
            .finish();
        (tracing::subscriber::set_default(subscriber), buffer)
    }
    fn assert_safe_diagnostics(buffer: &DiagnosticBuffer, category: &str) {
        let bytes = buffer.0.lock().unwrap();
        let output = String::from_utf8_lossy(&bytes);
        assert!(
            output.contains(category),
            "expected diagnostic category must be captured"
        );
        assert!(
            output.contains("127.0.0.1"),
            "safe endpoint location remains"
        );
        assert!(
            !output.contains("SYNTHETIC_PATH_CANARY"),
            "all diagnostics must omit path credentials"
        );
        assert!(
            !output.contains("SYNTHETIC_QUERY_CANARY"),
            "all diagnostics must omit query credentials; targets: {:?}",
            output
                .lines()
                .filter(|line| line.contains("SYNTHETIC_QUERY_CANARY"))
                .map(|line| line.split_whitespace().nth(1).unwrap_or("unknown"))
                .collect::<Vec<_>>()
        );
        assert!(
            !output.contains("SYNTHETIC_FRAGMENT_CANARY"),
            "all diagnostics must omit fragment credentials"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn malformed_json_accepted_response_has_safe_diagnostics() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (_capture, diagnostics) = capture_diagnostics();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            loop {
                let mut byte = [0];
                socket.read_exact(&mut byte).await.unwrap();
                request.push(byte[0]);
                if request.ends_with(b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() < 16_384);
            }
            assert!(String::from_utf8(request).unwrap().starts_with(
                "POST /SYNTHETIC_PATH_CANARY/mcp?token=SYNTHETIC_QUERY_CANARY HTTP/1.1\r\n"
            ));
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1\r\nConnection: close\r\n\r\n{").await.unwrap();
        });
        let client = DiagnosticHttpClient(
            reqwest_mcp::Client::builder()
                .pool_max_idle_per_host(0)
                .build()
                .unwrap(),
        );
        let response = client
            .post_message(
                format!(
                    "http://{address}/SYNTHETIC_PATH_CANARY/mcp?token=SYNTHETIC_QUERY_CANARY#SYNTHETIC_FRAGMENT_CANARY"
                )
                .into(),
                serde_json::from_value(json!({"jsonrpc":"2.0", "id":1, "method":"ping"})).unwrap(),
                None,
                None,
                HashMap::new(),
            )
            .await
            .unwrap();
        assert!(
            matches!(response, StreamableHttpPostResponse::Accepted),
            "malformed JSON retains rmcp accepted-response behavior"
        );
        server.await.unwrap();
        let bytes = diagnostics.0.lock().unwrap();
        let output = String::from_utf8_lossy(&bytes);
        assert!(output.contains("could not parse JSON response"));
        assert!(
            !output.contains("SYNTHETIC_QUERY_CANARY"),
            "accepted malformed JSON diagnostics must omit query credentials"
        );
        assert!(
            !output.contains("SYNTHETIC_FRAGMENT_CANARY"),
            "accepted malformed JSON diagnostics must omit fragment credentials"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn diagnostic_client_preserves_wire_requests_and_error_categories() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        for method in ["POST", "GET", "DELETE"] {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                    assert!(request.len() < 16_384);
                }
                let request = String::from_utf8(request).unwrap();
                assert!(
                    request.starts_with(&format!(
                        "{method} /SYNTHETIC_PATH_CANARY/mcp?token=SYNTHETIC_QUERY_CANARY HTTP/1.1\r\n"
                    )),
                    "original query reaches the actual HTTP request"
                );
                let headers = request.to_ascii_lowercase();
                assert!(headers.contains("authorization: bearer synthetic-auth-control\r\n"));
                assert!(headers.contains("mcp-session-id: synthetic-session-control\r\n"));
                assert!(headers.contains("x-synthetic-control: retained\r\n"));
                assert!(
                    !request.contains("SYNTHETIC_FRAGMENT_CANARY"),
                    "HTTP fragments are not transmitted"
                );
                socket.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
            });
            let url = BellProviderUrl::parse(format!(
                "http://{address}/SYNTHETIC_PATH_CANARY/mcp?token=SYNTHETIC_QUERY_CANARY#SYNTHETIC_FRAGMENT_CANARY"
            ))
            .unwrap();
            assert_eq!(url.diagnostic_endpoint(), format!("http://{address}"));
            assert!(
                url.as_str()
                    .ends_with("?token=SYNTHETIC_QUERY_CANARY#SYNTHETIC_FRAGMENT_CANARY")
            );
            let client = DiagnosticHttpClient(
                reqwest_mcp::Client::builder()
                    .pool_max_idle_per_host(0)
                    .build()
                    .unwrap(),
            );
            let uri: Arc<str> = url.as_str().into();
            let session: Arc<str> = "synthetic-session-control".into();
            let auth = Some("synthetic-auth-control".to_owned());
            let headers = HashMap::from([(
                HeaderName::from_static("x-synthetic-control"),
                HeaderValue::from_static("retained"),
            )]);
            let error = match method {
                "POST" => client
                    .post_message(
                        uri,
                        serde_json::from_value(json!({"jsonrpc":"2.0", "id":1, "method":"ping"}))
                            .unwrap(),
                        Some(session),
                        auth,
                        headers,
                    )
                    .await
                    .unwrap_err(),
                "GET" => client
                    .get_stream(
                        uri,
                        session,
                        Some("event-control".to_owned()),
                        auth,
                        headers,
                    )
                    .await
                    .err()
                    .expect("server status error"),
                _ => client
                    .delete_session(uri, session, auth, headers)
                    .await
                    .unwrap_err(),
            };
            if method == "POST" {
                // rmcp preserves non-JSON HTTP error bodies as this protocol variant.
                let StreamableHttpError::UnexpectedServerResponse(message) = error else {
                    panic!("POST protocol error variant must remain unchanged");
                };
                assert!(message.starts_with("HTTP 500"));
            } else {
                let StreamableHttpError::Client(error) = error else {
                    panic!("client error variant must remain unchanged");
                };
                assert!(error.is_status());
                assert_eq!(
                    error.status(),
                    Some(reqwest_mcp::StatusCode::INTERNAL_SERVER_ERROR)
                );
                assert!(
                    error.url().is_none(),
                    "request URL removed before worker diagnostics"
                );
                assert!(!format!("{error:?} {error}").contains("SYNTHETIC_QUERY_CANARY"));
            }
            server.await.unwrap();
        }
    }

    async fn diagnostic_http_fixture(
        response: String,
    ) -> (BellProviderUrl, tokio::task::JoinHandle<()>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server =
            tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut byte = [0];
                    socket.read_exact(&mut byte).await.unwrap();
                    request.push(byte[0]);
                    if request.ends_with(b"\r\n\r\n") {
                        break;
                    }
                    assert!(request.len() < 16_384);
                }
                let headers = String::from_utf8(request).unwrap();
                assert!(
                    headers.lines().next().unwrap().contains(
                        " /SYNTHETIC_PATH_CANARY/mcp?token=SYNTHETIC_QUERY_CANARY HTTP/1.1"
                    ),
                    "actual request query must be preserved"
                );
                assert!(!headers.contains("SYNTHETIC_FRAGMENT_CANARY"));
                let length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|value| value.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                assert!(length < 16_384);
                socket.read_exact(&mut vec![0; length]).await.unwrap();
                socket.write_all(response.as_bytes()).await.unwrap();
            });
        (
            BellProviderUrl::parse(format!(
                "http://{address}/SYNTHETIC_PATH_CANARY/mcp?token=SYNTHETIC_QUERY_CANARY#SYNTHETIC_FRAGMENT_CANARY"
            ))
            .unwrap(),
            server,
        )
    }

    fn http_response(status: u16, content_type: &str, extra: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Fixture\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n{extra}\r\n{body}",
            body.len()
        )
    }

    #[tokio::test(flavor = "current_thread")]
    async fn diagnostic_post_preserves_json_status_session_auth_and_sse_contracts() {
        use futures_util::StreamExt;
        let (_capture, diagnostics) = capture_diagnostics();
        for (status, content_type, extra, body, expected) in [
            (202, "text/plain", "", "", "accepted"),
            (204, "text/plain", "", "", "accepted"),
            (
                200,
                "application/json",
                "Mcp-Session-Id: fixture-session\r\n",
                r#"{"jsonrpc":"2.0","id":1,"result":{}}"#,
                "json",
            ),
            (200, "application/json", "", "{", "accepted"),
            (
                400,
                "application/json",
                "",
                r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32600,"message":"fixture protocol error"}}"#,
                "protocol-error",
            ),
            (
                500,
                "text/plain",
                "",
                "SYNTHETIC_QUERY_CANARY reflected response",
                "response-error",
            ),
            (500, "application/json", "", "{", "response-error"),
            (
                401,
                "text/plain",
                "WWW-Authenticate: Bearer realm=fixture\r\n",
                "",
                "auth",
            ),
            (
                403,
                "text/plain",
                "WWW-Authenticate: Bearer scope=fixture:read\r\n",
                "",
                "scope",
            ),
            (404, "text/plain", "", "", "expired"),
            (
                200,
                "text/event-stream",
                "Mcp-Session-Id: fixture-session\r\n",
                "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{}}\n\n",
                "sse",
            ),
        ] {
            let (url, server) =
                diagnostic_http_fixture(http_response(status, content_type, extra, body)).await;
            let client = DiagnosticHttpClient(
                reqwest_mcp::Client::builder()
                    .pool_max_idle_per_host(0)
                    .build()
                    .unwrap(),
            );
            let result = client
                .post_message(
                    url.as_str().into(),
                    serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
                        .unwrap(),
                    Some("fixture-session".into()),
                    Some("fixture-auth".to_owned()),
                    HashMap::new(),
                )
                .await;
            match (expected, result) {
                ("accepted", Ok(StreamableHttpPostResponse::Accepted)) => {}
                (
                    "json",
                    Ok(StreamableHttpPostResponse::Json(
                        rmcp::model::ServerJsonRpcMessage::Response(_),
                        session,
                    )),
                ) => assert_eq!(session.as_deref(), Some("fixture-session")),
                (
                    "protocol-error",
                    Ok(StreamableHttpPostResponse::Json(
                        rmcp::model::ServerJsonRpcMessage::Error(error),
                        _,
                    )),
                ) => assert_eq!(error.error.message, "fixture protocol error"),
                ("response-error", Err(StreamableHttpError::UnexpectedServerResponse(message))) => {
                    assert!(message.starts_with("HTTP 500"));
                    assert!(
                        !message.contains("SYNTHETIC_QUERY_CANARY"),
                        "reflected response error must not expose credentials"
                    );
                }
                ("auth", Err(StreamableHttpError::AuthRequired(error))) => {
                    assert_eq!(error.www_authenticate_header, "authentication required")
                }
                ("scope", Err(StreamableHttpError::InsufficientScope(error))) => {
                    assert_eq!(error.required_scope.as_deref(), Some("required scope"))
                }
                ("expired", Err(StreamableHttpError::SessionExpired)) => {}
                ("sse", Ok(StreamableHttpPostResponse::Sse(mut events, session))) => {
                    assert_eq!(session.as_deref(), Some("fixture-session"));
                    let event = events.next().await.unwrap().unwrap();
                    assert_eq!(event.event.as_deref(), Some("message"));
                    assert_eq!(
                        event.data.as_deref(),
                        Some(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#)
                    );
                    assert!(events.next().await.is_none());
                }
                _ => panic!(
                    "response must retain the independently specified protocol variant for {expected}"
                ),
            }
            server.await.unwrap();
        }
        let bytes = diagnostics.0.lock().unwrap();
        let output = String::from_utf8_lossy(&bytes);
        assert!(output.contains("could not parse JSON response"));
        assert!(
            !output.contains("SYNTHETIC_QUERY_CANARY"),
            "all POST branch diagnostics must omit query credentials"
        );
        assert!(!output.contains("SYNTHETIC_FRAGMENT_CANARY"));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reflected_http_error_body_is_safe_before_worker_logging() {
        let (_capture, diagnostics) = capture_diagnostics();
        let (url, server) = diagnostic_http_fixture(http_response(
            500,
            "text/plain",
            "",
            "SYNTHETIC_QUERY_CANARY reflected response",
        ))
        .await;
        let provider = MemoryMcpProvider::new(url);
        assert_eq!(provider.ensure_connected().await, ConnectOutcome::Error);
        server.await.unwrap();
        assert_safe_diagnostics(&diagnostics, "bell provider cold-start failed");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn sse_body_failures_preserve_typed_errors_without_request_urls() {
        use futures_util::StreamExt;
        let (_capture, diagnostics) = capture_diagnostics();
        for method in ["POST", "GET"] {
            let (url, server) = diagnostic_http_fixture("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 1000\r\nConnection: close\r\n\r\ndata: {}\n\n".to_owned()).await;
            let client = DiagnosticHttpClient(
                reqwest_mcp::Client::builder()
                    .pool_max_idle_per_host(0)
                    .build()
                    .unwrap(),
            );
            let mut stream = if method == "POST" {
                let response = client
                    .post_message(
                        url.as_str().into(),
                        serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
                            .unwrap(),
                        None,
                        None,
                        HashMap::new(),
                    )
                    .await
                    .unwrap();
                let StreamableHttpPostResponse::Sse(stream, _) = response else {
                    panic!("SSE response variant");
                };
                stream
            } else {
                client
                    .get_stream(
                        url.as_str().into(),
                        "fixture-session".into(),
                        None,
                        None,
                        HashMap::new(),
                    )
                    .await
                    .unwrap()
            };
            let mut failure = None;
            while let Some(event) = stream.next().await {
                if let Err(error) = event {
                    failure = Some(error);
                    break;
                }
            }
            let error = failure.expect("truncated real HTTP body must fail");
            tracing::warn!(error = ?error, "synthetic SSE body failure");
            let sse_stream::Error::Body(error) = error else {
                panic!("SSE body error variant must remain");
            };
            let client_error = error
                .downcast_ref::<reqwest_mcp::Error>()
                .expect("original reqwest body cause remains");
            assert!(
                client_error.url().is_none(),
                "SSE error must omit request URL before logging"
            );
            server.await.unwrap();
        }
        let bytes = diagnostics.0.lock().unwrap();
        let output = String::from_utf8_lossy(&bytes);
        assert!(output.contains("synthetic SSE body failure"));
        assert!(
            !output.contains("SYNTHETIC_QUERY_CANARY"),
            "all SSE body diagnostics must omit query credentials"
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn reflected_response_headers_are_safe_in_errors_and_worker_logs() {
        let (_capture, diagnostics) = capture_diagnostics();
        for (status, content_type, extra, expected, scope_present) in [
            (
                200,
                "text/plain; reflected=SYNTHETIC_QUERY_CANARY",
                "",
                "content-type",
                false,
            ),
            (
                401,
                "text/plain",
                "WWW-Authenticate: Bearer realm=SYNTHETIC_QUERY_CANARY\r\n",
                "auth",
                false,
            ),
            (
                403,
                "text/plain",
                "WWW-Authenticate: Bearer scope=SYNTHETIC_QUERY_CANARY\r\n",
                "scope",
                true,
            ),
            (
                403,
                "text/plain",
                "WWW-Authenticate: Bearer realm=SYNTHETIC_QUERY_CANARY\r\n",
                "scope",
                false,
            ),
        ] {
            let response = http_response(
                status,
                content_type,
                extra,
                "SYNTHETIC_QUERY_CANARY response body",
            );
            let (url, server) = diagnostic_http_fixture(response.clone()).await;
            assert_eq!(
                MemoryMcpProvider::new(url).ensure_connected().await,
                ConnectOutcome::Error
            );
            server.await.unwrap();
            assert_safe_diagnostics(&diagnostics, "bell provider cold-start failed");
            let (url, server) = diagnostic_http_fixture(response).await;
            let client = DiagnosticHttpClient(
                reqwest_mcp::Client::builder()
                    .pool_max_idle_per_host(0)
                    .build()
                    .unwrap(),
            );
            let error = client
                .post_message(
                    url.as_str().into(),
                    serde_json::from_value(json!({"jsonrpc":"2.0","id":1,"method":"ping"}))
                        .unwrap(),
                    Some("fixture-session".into()),
                    None,
                    HashMap::new(),
                )
                .await
                .unwrap_err();
            assert!(
                !format!("{error:?} {error}").contains("SYNTHETIC_QUERY_CANARY"),
                "returned diagnostic error must omit reflected headers"
            );
            match (expected, error) {
                ("content-type", StreamableHttpError::UnexpectedContentType(value)) => {
                    assert!(value.is_some())
                }
                ("auth", StreamableHttpError::AuthRequired(value)) => {
                    assert!(!value.www_authenticate_header.is_empty())
                }
                ("scope", StreamableHttpError::InsufficientScope(value)) => {
                    assert_eq!(value.required_scope.is_some(), scope_present)
                }
                _ => panic!("response diagnostic must preserve protocol variant and hint presence"),
            }
            server.await.unwrap();
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn delegated_get_content_type_error_is_safe() {
        let (_capture, diagnostics) = capture_diagnostics();
        let response = http_response(200, "text/SYNTHETIC_QUERY_CANARY", "", "");
        let (url, server) = diagnostic_http_fixture(response).await;
        let client = DiagnosticHttpClient(
            reqwest_mcp::Client::builder()
                .pool_max_idle_per_host(0)
                .build()
                .unwrap(),
        );
        let error = client
            .get_stream(
                url.as_str().into(),
                "fixture-session".into(),
                None,
                None,
                HashMap::new(),
            )
            .await
            .err()
            .expect("GET must reject unexpected content type");
        assert!(matches!(
            &error,
            StreamableHttpError::UnexpectedContentType(Some(_))
        ));
        assert!(!format!("{error:?} {error}").contains("SYNTHETIC_QUERY_CANARY"));
        tracing::warn!(endpoint = %url.diagnostic_endpoint(), error = ?error, "delegated GET rejected");
        assert_safe_diagnostics(&diagnostics, "delegated GET rejected");
        server.await.unwrap();
    }

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

    #[tokio::test(flavor = "current_thread")]
    async fn multi_provider_hanging_connect_is_bounded() {
        let (_capture, diagnostics) = capture_diagnostics();
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
            url: BellProviderUrl::parse(format!(
                "http://{addr}/SYNTHETIC_PATH_CANARY/mcp?token=SYNTHETIC_QUERY_CANARY#SYNTHETIC_FRAGMENT_CANARY"
            ))
            .unwrap(),
            scope: BellScope::parse("test").unwrap(),
            alias: None,
        }];
        let start = tokio::time::Instant::now();
        let BellEvaluation { bells, status } = registry
            .evaluate("query text", &configs, &RecallOptions::default())
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert_safe_diagnostics(&diagnostics, "bell provider connection timed out");
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
    #[tokio::test(flavor = "current_thread")]
    async fn multi_provider_connect_error_reports_error_not_timeout() {
        let (_capture, diagnostics) = capture_diagnostics();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let registry = BellEvaluator::default();
        let configs = vec![BellProviderConfig {
            url: BellProviderUrl::parse(format!(
                "http://{addr}/SYNTHETIC_PATH_CANARY/mcp?token=SYNTHETIC_QUERY_CANARY#SYNTHETIC_FRAGMENT_CANARY"
            ))
            .unwrap(),
            scope: BellScope::parse("test").unwrap(),
            alias: None,
        }];
        let BellEvaluation { bells, status } = registry
            .evaluate("query text", &configs, &RecallOptions::default())
            .await
            .unwrap();

        assert_safe_diagnostics(&diagnostics, "bell provider cold-start failed");
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
