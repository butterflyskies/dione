//! Minimal live Microsoft Bot Connector edge used by the Teams canary.

use super::{ActivityId, ApplicationId, ConversationId, TenantId};
use crate::teams::{
    AdmissionPolicy, AuthenticatedTeamsEnvelope, CorrelatedReplyRequest, ProbeError,
    authenticate_activity, correlated_reply, parse_connector_metadata,
};
use reqwest::{Client, Url};
use secrecy::{ExposeSecret, SecretString};
use serde::{Deserialize, Serialize};
use std::{future::Future, time::Duration};
use thiserror::Error;

const OPENID_URL: &str = "https://login.botframework.com/v1/.well-known/openidconfiguration";
const SCOPE: &str = "https://api.botframework.com/.default";
const MAX_METADATA_BYTES: usize = 64 * 1024;
const MAX_JWKS_BYTES: usize = 256 * 1024;

#[derive(Clone, Debug)]
/// An acquired Bot Connector token, kept secret until an outbound request is prepared.
pub struct BotAccessToken {
    value: SecretString,
    pub expires_in: Duration,
}

/// Supplies short-lived Bot Connector credentials for outbound replies.
pub trait BotTokenProvider: Send + Sync {
    /// Acquire a token without exposing its value to the resident notification.
    fn access_token(&self) -> impl Future<Output = Result<BotAccessToken, TeamsEdgeError>> + Send;
}

#[derive(Clone, Debug)]
/// Acquires Bot Connector tokens with a client-secret OAuth credential.
pub struct ClientSecretTokenProvider {
    client: Client,
    endpoint: Url,
    client_id: ApplicationId,
    client_secret: SecretString,
}

impl ClientSecretTokenProvider {
    /// Build a token provider for the specified Microsoft tenant.
    pub fn new(
        client: Client,
        tenant_id: &TenantId,
        client_id: ApplicationId,
        client_secret: SecretString,
    ) -> Result<Self, TeamsEdgeError> {
        let mut endpoint = Url::parse("https://login.microsoftonline.com/")?;
        endpoint
            .path_segments_mut()
            .map_err(|_| TeamsEdgeError {
                kind: TeamsEdgeErrorKind::InvalidTokenEndpoint,
            })?
            .extend([tenant_id.as_str(), "oauth2", "v2.0", "token"]);
        Ok(Self {
            client,
            endpoint,
            client_id,
            client_secret,
        })
    }

    #[cfg(test)]
    fn with_endpoint(
        client: Client,
        endpoint: Url,
        client_id: ApplicationId,
        client_secret: SecretString,
    ) -> Self {
        Self {
            client,
            endpoint,
            client_id,
            client_secret,
        }
    }
}

#[derive(Serialize)]
struct TokenForm<'a> {
    grant_type: &'static str,
    client_id: &'a str,
    client_secret: &'a str,
    scope: &'static str,
}
#[derive(Deserialize)]
struct TokenResponse {
    access_token: SecretString,
    expires_in: u64,
}

impl BotTokenProvider for ClientSecretTokenProvider {
    async fn access_token(&self) -> Result<BotAccessToken, TeamsEdgeError> {
        let body = self
            .client
            .post(self.endpoint.clone())
            .form(&TokenForm {
                grant_type: "client_credentials",
                client_id: self.client_id.as_str(),
                client_secret: self.client_secret.expose_secret(),
                scope: SCOPE,
            })
            .send()
            .await?
            .error_for_status()?
            .json::<TokenResponse>()
            .await?;
        Ok(BotAccessToken {
            value: body.access_token,
            expires_in: Duration::from_secs(body.expires_in),
        })
    }
}

#[derive(Clone)]
/// Authenticates inbound Teams Activities and sends replies through the trusted connector.
pub struct TeamsEdge<P> {
    client: Client,
    metadata_client: Client,
    token_provider: P,
    policy: AdmissionPolicy,
    openid_url: Url,
}

impl<P: BotTokenProvider> TeamsEdge<P> {
    pub(crate) fn admission_policy(&self) -> &AdmissionPolicy {
        &self.policy
    }

    /// Create the production edge with HTTPS-only, nonredirecting metadata retrieval.
    pub fn new(
        client: Client,
        token_provider: P,
        policy: AdmissionPolicy,
    ) -> Result<Self, TeamsEdgeError> {
        let metadata_client = Client::builder()
            .use_rustls_tls()
            .https_only(true)
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            metadata_client,
            token_provider,
            policy,
            openid_url: Url::parse(OPENID_URL)?,
        })
    }

    #[cfg(test)]
    fn with_openid_url(
        client: Client,
        token_provider: P,
        policy: AdmissionPolicy,
        openid_url: Url,
    ) -> Self {
        Self {
            client: client.clone(),
            metadata_client: client,
            token_provider,
            policy,
            openid_url,
        }
    }

    /// Authenticate an Activity and send one correlated reply.
    pub async fn authenticate_and_reply(
        &self,
        authorization: &str,
        raw_activity: &[u8],
        now: u64,
        reply_text: &str,
    ) -> Result<LiveReplyReceipt, TeamsEdgeError> {
        let envelope = self.authenticate(authorization, raw_activity, now).await?;
        self.reply(&envelope, reply_text).await
    }

    /// Authenticate one inbound Activity and seal every field later reply code may use.
    pub(crate) async fn authenticate(
        &self,
        authorization: &str,
        raw_activity: &[u8],
        now: u64,
    ) -> Result<AuthenticatedTeamsEnvelope, TeamsEdgeError> {
        let openid = self
            .read_metadata(self.openid_url.clone(), MAX_METADATA_BYTES)
            .await?;
        let value: serde_json::Value = serde_json::from_slice(&openid)?;
        let jwks_uri = value
            .get("jwks_uri")
            .and_then(serde_json::Value::as_str)
            .ok_or(TeamsEdgeError {
                kind: TeamsEdgeErrorKind::MissingJwksUri,
            })?;
        let jwks_url = Url::parse(jwks_uri)?;
        if jwks_url.scheme() != "https" || jwks_url.host_str() != Some("login.botframework.com") {
            return Err(TeamsEdgeError {
                kind: TeamsEdgeErrorKind::InvalidJwksUri,
            });
        }
        let jwks = self.read_metadata(jwks_url, MAX_JWKS_BYTES).await?;
        let metadata = parse_connector_metadata(&openid, &jwks)?;
        Ok(authenticate_activity(
            authorization,
            raw_activity,
            &metadata,
            &self.policy,
            now,
        )?)
    }

    async fn read_metadata(&self, url: Url, limit: usize) -> Result<Vec<u8>, TeamsEdgeError> {
        let response = self.metadata_client.get(url.clone()).send().await?;
        if response.status().is_redirection() {
            return Err(TeamsEdgeError {
                kind: TeamsEdgeErrorKind::MetadataRedirect,
            });
        }
        let mut response = response.error_for_status()?;
        if response.url().scheme() != url.scheme()
            || response.url().host_str() != url.host_str()
            || response.url().port_or_known_default() != url.port_or_known_default()
        {
            return Err(TeamsEdgeError {
                kind: TeamsEdgeErrorKind::MetadataRedirect,
            });
        }
        if response
            .content_length()
            .is_some_and(|size| size > limit as u64)
        {
            return Err(TeamsEdgeError {
                kind: TeamsEdgeErrorKind::MetadataTooLarge,
            });
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            if chunk.len() > limit.saturating_sub(body.len()) {
                return Err(TeamsEdgeError {
                    kind: TeamsEdgeErrorKind::MetadataTooLarge,
                });
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Send one reply using only a previously authenticated, sealed envelope.
    pub(crate) async fn reply(
        &self,
        envelope: &AuthenticatedTeamsEnvelope,
        reply_text: &str,
    ) -> Result<LiveReplyReceipt, TeamsEdgeError> {
        let prepared = self.prepare_reply(envelope, reply_text).await?;
        self.send_prepared_reply(prepared).await
    }

    /// Complete all fallible work that is known to happen before a Connector
    /// request can be sent. A caller holding a one-use authority can leave it
    /// available if this phase fails or is cancelled.
    pub(crate) async fn prepare_reply(
        &self,
        envelope: &AuthenticatedTeamsEnvelope,
        reply_text: &str,
    ) -> Result<PreparedReply, TeamsEdgeError> {
        let request = correlated_reply(envelope, reply_text)?;
        let token = self.token_provider.access_token().await?;
        Ok(PreparedReply {
            request,
            token,
            incoming_activity_id: envelope.reference.incoming_activity_id.clone(),
            conversation_id: envelope.reference.conversation_id.clone(),
        })
    }

    /// Once this phase begins, a failed or cancelled request may already have
    /// reached Connector. The caller must consume its one-use authority first.
    pub(crate) async fn send_prepared_reply(
        &self,
        prepared: PreparedReply,
    ) -> Result<LiveReplyReceipt, TeamsEdgeError> {
        let response = self
            .client
            .request(prepared.request.method, prepared.request.url)
            .bearer_auth(prepared.token.value.expose_secret())
            .json(&prepared.request.body)
            .send()
            .await?
            .error_for_status()?
            .json::<ConnectorReceipt>()
            .await?;
        Ok(LiveReplyReceipt {
            incoming_activity_id: prepared.incoming_activity_id,
            outgoing_activity_id: response.id,
            conversation_id: prepared.conversation_id,
        })
    }
}

pub(crate) struct PreparedReply {
    request: CorrelatedReplyRequest,
    token: BotAccessToken,
    incoming_activity_id: ActivityId,
    conversation_id: ConversationId,
}

#[derive(Deserialize)]
struct ConnectorReceipt {
    id: ActivityId,
}

#[derive(Debug, PartialEq, Eq)]
/// Connector identifiers returned after an outbound reply is accepted.
pub struct LiveReplyReceipt {
    pub incoming_activity_id: ActivityId,
    pub outgoing_activity_id: ActivityId,
    pub conversation_id: ConversationId,
}

/// A failed Teams connector or credential operation.
#[derive(Debug, Error)]
#[error("{kind}")]
pub struct TeamsEdgeError {
    /// The semantic classification and retained original cause.
    #[source]
    pub kind: TeamsEdgeErrorKind,
}

impl From<TeamsEdgeErrorKind> for TeamsEdgeError {
    fn from(kind: TeamsEdgeErrorKind) -> Self {
        Self { kind }
    }
}
impl From<ProbeError> for TeamsEdgeError {
    fn from(source: ProbeError) -> Self {
        Self {
            kind: source.into(),
        }
    }
}
impl From<reqwest::Error> for TeamsEdgeError {
    fn from(source: reqwest::Error) -> Self {
        Self {
            kind: source.into(),
        }
    }
}
impl From<url::ParseError> for TeamsEdgeError {
    fn from(source: url::ParseError) -> Self {
        Self {
            kind: source.into(),
        }
    }
}
impl From<serde_json::Error> for TeamsEdgeError {
    fn from(source: serde_json::Error) -> Self {
        Self {
            kind: source.into(),
        }
    }
}

#[derive(Debug, Error)]
/// Failure at the Teams authentication, metadata, token, or reply boundary.
pub enum TeamsEdgeErrorKind {
    /// Teams authentication or correlation failed.
    #[error("teams authentication or correlation failed")]
    Probe(#[from] ProbeError),
    /// Microsoft HTTP request failed.
    #[error("microsoft HTTP request failed")]
    Http(#[from] reqwest::Error),
    /// Microsoft URL is invalid.
    #[error("microsoft URL is invalid")]
    Url(#[from] url::ParseError),
    /// Token endpoint cannot be constructed.
    #[error("token endpoint cannot be constructed")]
    InvalidTokenEndpoint,
    /// OpenID metadata has no JWKS URI.
    #[error("openID metadata has no JWKS URI")]
    MissingJwksUri,
    /// OpenID metadata supplied an untrusted JWKS URI.
    #[error("openID metadata supplied an untrusted JWKS URI")]
    InvalidJwksUri,
    /// OpenID metadata JSON is invalid.
    #[error("openID metadata JSON is invalid")]
    MetadataJson(#[from] serde_json::Error),
    /// Teams metadata response exceeds its size limit.
    #[error("teams metadata response exceeds its size limit")]
    MetadataTooLarge,
    /// Teams metadata response redirected to an untrusted origin.
    #[error("teams metadata response redirected to an untrusted origin")]
    MetadataRedirect,
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::teams::runtime::TeamsRuntimeError;
    use axum::{
        Form, Json, Router,
        body::Bytes,
        extract::{OriginalUri, State},
        http::{HeaderMap, StatusCode},
        response::IntoResponse,
        routing::{get, post},
    };
    use axum_server::tls_rustls::RustlsConfig;
    use jsonwebtoken::{Algorithm, EncodingKey, Header, encode};
    use rcgen::{CertifiedKey, generate_simple_self_signed};
    use serde_json::json;
    use std::{
        collections::{BTreeMap, BTreeSet, HashMap},
        net::TcpListener,
        sync::{Arc, Mutex},
    };
    use tempfile::TempDir;
    use tokio::sync::mpsc;

    const APP_ID: &str = "00000000-1111-2222-3333-444444444444";
    const TENANT_ID: &str = "aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee";
    const NOW: u64 = 1_800_000_000;
    const PRIVATE_KEY: &[u8] = include_bytes!("../../tests/fixtures/teams/connector-private.pem");
    const JWKS_DOCUMENT: &[u8] = include_bytes!("../../tests/fixtures/teams/connector-jwks.json");

    #[derive(Clone, Default)]
    struct FixtureState {
        token_forms: Arc<Mutex<Vec<HashMap<String, String>>>>,
        replies: Arc<Mutex<Vec<CapturedReply>>>,
    }

    #[derive(Debug)]
    struct CapturedReply {
        authorization: String,
        path: String,
        body: serde_json::Value,
    }

    #[tokio::test]
    async fn client_secret_provider_uses_single_tenant_v2_scope_without_exposing_secret() {
        let (captured_tx, mut captured_rx) = mpsc::unbounded_channel();
        let app = Router::new().route(
            "/tenant/oauth2/v2.0/token",
            post(
                move |Form(form): Form<std::collections::HashMap<String, String>>| async move {
                    captured_tx.send(form).expect("capture token form");
                    Json(json!({
                        "access_token": "test-access-token",
                        "expires_in": 3599
                    }))
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fixture server");
        let address = listener.local_addr().expect("fixture address");
        let server = tokio::spawn(async move { axum::serve(listener, app).await });
        let endpoint =
            Url::parse(&format!("http://{address}/tenant/oauth2/v2.0/token")).expect("fixture URL");
        let provider = ClientSecretTokenProvider::with_endpoint(
            Client::new(),
            endpoint,
            "app-id".parse().unwrap(),
            SecretString::from("short-lived-secret".to_owned()),
        );

        assert!(!format!("{provider:?}").contains("short-lived-secret"));
        let token = provider
            .access_token()
            .await
            .expect("token acquisition succeeds");
        assert_eq!(token.value.expose_secret(), "test-access-token");
        assert_eq!(token.expires_in, Duration::from_secs(3599));
        let form = captured_rx.recv().await.expect("token form captured");
        assert_eq!(
            form.get("grant_type").map(String::as_str),
            Some("client_credentials")
        );
        assert_eq!(form.get("client_id").map(String::as_str), Some("app-id"));
        assert_eq!(
            form.get("client_secret").map(String::as_str),
            Some("short-lived-secret")
        );
        assert_eq!(form.get("scope").map(String::as_str), Some(SCOPE));
        server.abort();
    }

    #[tokio::test]
    async fn oversized_openid_document_is_rejected_before_activity_admission() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let CertifiedKey { cert, key_pair } =
            generate_simple_self_signed(vec!["login.botframework.com".to_owned()])
                .expect("generate fixture certificate");
        let tls = RustlsConfig::from_pem(
            cert.pem().into_bytes(),
            key_pair.serialize_pem().into_bytes(),
        )
        .await
        .expect("load fixture TLS certificate");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind TLS fixture server");
        listener
            .set_nonblocking(true)
            .expect("configure nonblocking TLS fixture listener");
        let address = listener.local_addr().expect("fixture address");
        let app = Router::new()
            .route(
                "/openid",
                get(|| async {
                    Json(json!({
                        "issuer": "https://api.botframework.com",
                        "jwks_uri": "https://login.botframework.com/v1/keys",
                        "id_token_signing_alg_values_supported": ["RS256"],
                        "padding": "x".repeat(128 * 1024)
                    }))
                }),
            )
            .route("/v1/keys", get(jwks));
        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .expect("configure TLS fixture server")
                .serve(app.into_make_service())
                .await
        });
        let client = Client::builder()
            .use_rustls_tls()
            .add_root_certificate(
                reqwest::Certificate::from_pem(cert.pem().as_bytes())
                    .expect("parse fixture certificate"),
            )
            .resolve("login.botframework.com", address)
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build fixture client");
        let edge = TeamsEdge::with_openid_url(
            client.clone(),
            ClientSecretTokenProvider::with_endpoint(
                client,
                Url::parse("https://login.botframework.com/unused-token")
                    .expect("fixture token URL"),
                APP_ID.parse().unwrap(),
                SecretString::from("unused-fixture-secret".to_owned()),
            ),
            fixture_policy(),
            Url::parse("https://login.botframework.com/openid").expect("fixture OpenID URL"),
        );
        let result = edge
            .authenticate(
                &signed_authorization("https://connector.test/"),
                &activity(TENANT_ID),
                NOW,
            )
            .await;
        assert!(matches!(
            result,
            Err(TeamsEdgeError {
                kind: TeamsEdgeErrorKind::MetadataTooLarge
            })
        ));
        server.abort();
    }

    #[tokio::test]
    async fn jwks_redirect_to_a_different_host_is_rejected() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let CertifiedKey { cert, key_pair } = generate_simple_self_signed(vec![
            "connector.test".to_owned(),
            "login.botframework.com".to_owned(),
        ])
        .expect("generate fixture certificate");
        let tls = RustlsConfig::from_pem(
            cert.pem().into_bytes(),
            key_pair.serialize_pem().into_bytes(),
        )
        .await
        .expect("load fixture TLS certificate");
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind TLS fixture server");
        listener
            .set_nonblocking(true)
            .expect("configure nonblocking TLS fixture listener");
        let address = listener.local_addr().expect("fixture address");
        let app = Router::new()
            .route("/openid", get(openid))
            .route(
                "/v1/keys",
                get(|| async {
                    (
                        StatusCode::FOUND,
                        [("location", "https://connector.test/redirected-keys")],
                    )
                }),
            )
            .route("/redirected-keys", get(jwks));
        let server = tokio::spawn(async move {
            axum_server::from_tcp_rustls(listener, tls)
                .expect("configure TLS fixture server")
                .serve(app.into_make_service())
                .await
        });
        let client = Client::builder()
            .use_rustls_tls()
            .add_root_certificate(
                reqwest::Certificate::from_pem(cert.pem().as_bytes())
                    .expect("parse fixture certificate"),
            )
            .resolve("connector.test", address)
            .resolve("login.botframework.com", address)
            .timeout(Duration::from_secs(2))
            .build()
            .expect("build fixture client");
        let edge = TeamsEdge::with_openid_url(
            client.clone(),
            ClientSecretTokenProvider::with_endpoint(
                client,
                Url::parse("https://login.botframework.com/unused-token")
                    .expect("fixture token URL"),
                APP_ID.parse().unwrap(),
                SecretString::from("unused-fixture-secret".to_owned()),
            ),
            fixture_policy(),
            Url::parse("https://login.botframework.com/openid").expect("fixture OpenID URL"),
        );
        let result = edge
            .authenticate(
                &signed_authorization("https://connector.test/"),
                &activity(TENANT_ID),
                NOW,
            )
            .await;
        assert!(matches!(
            result,
            Err(TeamsEdgeError {
                kind: TeamsEdgeErrorKind::MetadataRedirect
            })
        ));
        server.abort();
    }

    #[tokio::test]
    async fn composed_edge_confines_reply_and_stops_before_token_on_failed_admission() {
        let connector_fixture = ConnectorFixture::start().await;
        let fixture = connector_fixture.state.clone();
        let edge = connector_fixture.edge.clone();
        let edge_for_legacy = edge.clone();
        let edge_for_poison = edge.clone();
        let edge_for_failures = edge.clone();
        let authorization = signed_authorization("https://connector.test/");

        let queue_dir = TempDir::new().expect("create queue directory");
        let queue_path = camino::Utf8PathBuf::from_path_buf(queue_dir.path().to_owned())
            .expect("UTF-8 queue path");
        let queue = crate::codex::CodexEventQueue::load(
            &queue_path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .expect("load resident queue");
        let resident_thread =
            crate::codex::CodexThreadId::parse("thread-janus").expect("parse resident thread");
        let bridge = crate::teams::runtime::TeamsResidentBridge::new_durable(
            edge.clone(),
            crate::composition::teams_admission::from_codex(queue.clone()),
            &queue_path,
        )
        .await
        .expect("restore production reply authority");

        let unavailable = bridge
            .admit(&authorization, &activity(TENANT_ID), NOW)
            .await
            .expect_err("admission fails without a bound live resident");
        assert!(matches!(
            unavailable,
            crate::teams::runtime::TeamsRuntimeError { kind: crate::teams::TeamsRuntimeErrorKind::Resident(message) }
                if matches!(message.kind, crate::teams::delivery::TeamsDeliveryErrorKind::RouteUnavailable(_))
        ));
        assert_eq!(queue.status().await.queued, 0);

        queue
            .binding_controller()
            .bind_live_thread(Some(resident_thread.clone()))
            .await
            .expect("bind resident thread");
        let unavailable = bridge
            .admit(&authorization, &activity(TENANT_ID), NOW)
            .await
            .expect_err("admission fails before the live consumer registers");
        assert!(matches!(
            unavailable,
            crate::teams::runtime::TeamsRuntimeError { kind: crate::teams::TeamsRuntimeErrorKind::Resident(message) }
                if matches!(message.kind, crate::teams::delivery::TeamsDeliveryErrorKind::RouteUnavailable(_))
        ));
        assert_eq!(queue.status().await.queued, 0);

        let consumer = queue
            .register_live_consumer()
            .await
            .expect("register live resident");

        bridge
            .admit(&authorization, &activity(TENANT_ID), NOW)
            .await
            .expect("admit authenticated Activity");
        assert!(
            consumer
                .next_event(
                    &crate::codex::CodexThreadId::parse("thread-other").unwrap(),
                    Duration::ZERO,
                    Duration::from_secs(30),
                )
                .await
                .expect("check other thread")
                .is_none(),
            "Teams event must not cross into another resident thread"
        );
        let delivered = consumer
            .next_event(&resident_thread, Duration::ZERO, Duration::from_secs(30))
            .await
            .expect("lease resident event")
            .expect("event reaches exact resident thread");
        assert_eq!(delivered.event["params"]["content"], "hello");
        assert_eq!(delivered.event["params"]["meta"]["provider"], "teams");
        assert_eq!(delivered.event["params"]["meta"]["tenant_id"], TENANT_ID);
        bridge
            .admit(&authorization, &activity(TENANT_ID), NOW)
            .await
            .expect("replayed valid Activity is recognized");
        assert!(
            consumer
                .next_event(&resident_thread, Duration::ZERO, Duration::from_secs(30),)
                .await
                .expect("inspect duplicate admission")
                .is_none(),
            "replayed signed Activity must not create a second deliverable event"
        );
        let reply_handle = delivered.event["params"]["meta"]["reply_handle"]
            .as_str()
            .expect("opaque reply handle");
        assert!(reply_handle.starts_with("teams-"));
        assert!(!reply_handle.contains("conversation/id"));

        // A durable queue lease can outlive its original process. Reopening
        // authority under the same current admission policy must preserve the
        // exact handle carried by that event.
        drop(bridge);
        let bridge = crate::teams::runtime::TeamsResidentBridge::new_durable(
            edge,
            crate::composition::teams_admission::from_codex(queue.clone()),
            &queue_path,
        )
        .await
        .expect("restore leased Activity reply authority");

        let receipt = bridge
            .reply(
                &crate::teams::ReplyHandle::parse(reply_handle).unwrap(),
                "resident reply",
            )
            .await
            .expect("resident speech uses sealed authority");
        assert_eq!(receipt.incoming_activity_id.as_str(), "activity/id");
        assert_eq!(
            receipt.outgoing_activity_id.as_str(),
            "outgoing-activity-id"
        );
        assert_eq!(receipt.conversation_id.as_str(), "conversation/id");

        {
            let forms = fixture.token_forms.lock().expect("token forms lock");
            assert_eq!(forms.len(), 1);
            assert_eq!(
                forms[0].get("grant_type").map(String::as_str),
                Some("client_credentials")
            );
            assert_eq!(forms[0].get("client_id").map(String::as_str), Some(APP_ID));
            assert_eq!(
                forms[0].get("client_secret").map(String::as_str),
                Some("short-lived-secret")
            );
            assert_eq!(forms[0].get("scope").map(String::as_str), Some(SCOPE));
        }

        {
            let replies = fixture.replies.lock().expect("replies lock");
            assert_eq!(replies.len(), 1);
            assert_eq!(replies[0].authorization, "Bearer fixture-access-token");
            assert_eq!(
                replies[0].path,
                "/v3/conversations/conversation%2Fid/activities/activity%2Fid"
            );
            assert_eq!(
                replies[0].body,
                json!({
                    "type": "message",
                    "from": {"id": "bot-id"},
                    "recipient": {"id": "sender-id"},
                    "conversation": {"id": "conversation/id"},
                    "replyToId": "activity/id",
                    "text": "resident reply"
                })
            );
        }

        let replay = bridge
            .reply(
                &crate::teams::ReplyHandle::parse(reply_handle).unwrap(),
                "must not send twice",
            )
            .await
            .expect_err("reply handle is single-use");
        assert!(matches!(
            replay,
            crate::teams::runtime::TeamsRuntimeError {
                kind: crate::teams::TeamsRuntimeErrorKind::UnknownReplyHandle
            }
        ));

        consumer
            .acknowledge(&delivered.delivery_token)
            .await
            .expect("acknowledge accepted resident event");
        bridge
            .admit(&authorization, &activity(TENANT_ID), NOW)
            .await
            .expect("acknowledged Activity remains a duplicate");
        assert_eq!(queue.status().await.queued, 0);
        assert_eq!(fixture.replies.lock().expect("replies lock").len(), 1);

        bridge
            .admit(
                &authorization,
                &activity_with_id(TENANT_ID, "other/activity"),
                NOW,
            )
            .await
            .expect("admit a distinct Activity");
        let second = consumer
            .next_event(&resident_thread, Duration::ZERO, Duration::from_secs(30))
            .await
            .expect("lease second event")
            .expect("distinct Activity is delivered");
        let second_handle = second.event["params"]["meta"]["reply_handle"]
            .as_str()
            .expect("second opaque reply handle");
        let uncertain = bridge
            .reply(
                &crate::teams::ReplyHandle::parse(second_handle).unwrap(),
                "simulate uncertain outcome",
            )
            .await
            .expect_err("fixture Connector returns an error after recording POST");
        assert!(matches!(
            uncertain,
            crate::teams::runtime::TeamsRuntimeError {
                kind: crate::teams::TeamsRuntimeErrorKind::ReplyOutcomeUncertain(TeamsEdgeError {
                    kind: TeamsEdgeErrorKind::Http(_)
                })
            }
        ));
        assert!(matches!(
            bridge
                .reply(
                    &crate::teams::ReplyHandle::parse(second_handle).unwrap(),
                    "must not resend"
                )
                .await,
            Err(crate::teams::runtime::TeamsRuntimeError {
                kind: crate::teams::TeamsRuntimeErrorKind::UnknownReplyHandle
            })
        ));
        assert_eq!(fixture.replies.lock().expect("replies lock").len(), 2);

        let error = bridge
            .admit(&authorization, &activity("wrong-tenant"), NOW)
            .await
            .expect_err("tenant denial stops the edge");
        assert!(matches!(
            error,
            crate::teams::runtime::TeamsRuntimeError {
                kind: crate::teams::TeamsRuntimeErrorKind::Edge(TeamsEdgeError {
                    kind: TeamsEdgeErrorKind::Probe(ProbeError {
                        kind: crate::teams::ProbeErrorKind::TenantDenied
                    })
                })
            }
        ));
        assert_eq!(
            fixture.token_forms.lock().expect("token forms lock").len(),
            2
        );
        assert_eq!(fixture.replies.lock().expect("replies lock").len(), 2);

        let queued_before = queue.status().await.queued;
        let mut excluded: serde_json::Value = serde_json::from_slice(&activity(TENANT_ID)).unwrap();
        excluded["conversation"]["id"] = serde_json::json!("excluded/native room");
        let error = bridge
            .admit(&authorization, &serde_json::to_vec(&excluded).unwrap(), NOW)
            .await
            .expect_err("conversation denial stops admission");
        assert!(matches!(
            error,
            crate::teams::runtime::TeamsRuntimeError {
                kind: crate::teams::TeamsRuntimeErrorKind::Edge(TeamsEdgeError {
                    kind: TeamsEdgeErrorKind::Probe(ProbeError {
                        kind: crate::teams::ProbeErrorKind::ConversationDenied
                    })
                })
            }
        ));
        assert_eq!(queue.status().await.queued, queued_before);
        assert_eq!(fixture.token_forms.lock().unwrap().len(), 2);
        assert_eq!(fixture.replies.lock().unwrap().len(), 2);

        // Local snapshot of a pre-kind inbox whose public pull consumer had
        // claimed the old resident label. It must not authenticate a Teams
        // route or expose a reply handle until a genuine live registration.
        let legacy_dir = TempDir::new().expect("create legacy state directory");
        let legacy_path = camino::Utf8PathBuf::from_path_buf(legacy_dir.path().to_owned())
            .expect("UTF-8 legacy state path");
        let legacy_queue = crate::codex::CodexEventQueue::load(
            &legacy_path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .expect("create legacy fixture inbox");
        let legacy_thread = crate::codex::CodexThreadId::parse("legacy-thread").unwrap();
        legacy_queue
            .binding_controller()
            .bind_live_thread(Some(legacy_thread.clone()))
            .await
            .expect("persist fixture thread binding");
        drop(legacy_queue);
        let inbox_path = legacy_path.join("codex-inbox.json");
        let mut disk: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(&inbox_path)
                .await
                .expect("read local fixture inbox"),
        )
        .expect("parse fixture inbox");
        disk["next_consumer_generation"] = json!(1);
        disk["primary_consumer"] = json!("codex-consumer-0");
        disk["consumers"] = json!([{
            "id": "codex-consumer-0",
            "label": "dione-live-app-server",
            "expires_at": chrono::Utc::now() + chrono::TimeDelta::hours(1),
            "ttl_seconds": 3600
        }]);
        tokio::fs::write(&inbox_path, serde_json::to_vec_pretty(&disk).unwrap())
            .await
            .expect("write local pre-kind fixture");
        let legacy_queue = crate::codex::CodexEventQueue::load(
            &legacy_path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .expect("reload pre-kind fixture");
        let legacy_bridge = crate::teams::runtime::TeamsResidentBridge::new_durable(
            edge_for_legacy.clone(),
            crate::composition::teams_admission::from_codex(legacy_queue.clone()),
            &legacy_path,
        )
        .await
        .expect("load bridge with pre-kind fixture");
        let denied = legacy_bridge
            .admit(
                &authorization,
                &activity_with_id(TENANT_ID, "legacy/activity"),
                NOW,
            )
            .await
            .expect_err("ambiguous legacy label cannot be a resident route");
        assert!(matches!(
            denied,
            crate::teams::runtime::TeamsRuntimeError {
                kind: crate::teams::TeamsRuntimeErrorKind::Resident(_)
            }
        ));
        assert_eq!(legacy_queue.status().await.queued, 0);
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            authorization.parse().expect("signed Authorization header"),
        );
        let response = crate::teams::runtime::messages(
            State(Arc::new(legacy_bridge.clone())),
            headers,
            Bytes::from(activity_with_id(TENANT_ID, "legacy/activity")),
        )
        .await
        .into_response();
        assert_eq!(
            response.status(),
            StatusCode::SERVICE_UNAVAILABLE,
            "blocked legacy registration must tell Teams to retry"
        );
        assert_eq!(legacy_queue.status().await.queued, 0);
        let authority_state: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(legacy_path.join("teams-reply-authorities.json"))
                .await
                .expect("read local authority fixture"),
        )
        .expect("parse local authority fixture");
        assert_eq!(
            authority_state["pending"]
                .as_object()
                .map(|pending| pending.len()),
            Some(0),
            "rejected Activity must not leave an exposed reply authority"
        );
        assert!(matches!(
            legacy_queue.register_live_consumer().await,
            Err(crate::codex::CodexQueueError {
                kind: crate::codex::CodexQueueErrorKind::Durable(crate::durable::QueueError {
                    kind: crate::durable::QueueErrorKind::PrimaryConsumerExists,
                    ..
                })
            })
        ));
        drop(legacy_bridge);
        drop(legacy_queue);

        // The fixture has no old-ID pending work. Expiring its ambiguous
        // record permits a new Live-kind registration without promoting it.
        disk["consumers"][0]["expires_at"] =
            json!(chrono::Utc::now() - chrono::TimeDelta::seconds(1));
        tokio::fs::write(&inbox_path, serde_json::to_vec_pretty(&disk).unwrap())
            .await
            .expect("expire local fixture registration");
        let recovered_queue = crate::codex::CodexEventQueue::load(
            &legacy_path,
            crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            )),
        )
        .expect("reload expired legacy fixture");
        let resident = recovered_queue
            .register_live_consumer()
            .await
            .expect("register genuine live resident");
        let recovered_bridge = crate::teams::runtime::TeamsResidentBridge::new_durable(
            edge_for_legacy,
            crate::composition::teams_admission::from_codex(recovered_queue.clone()),
            &legacy_path,
        )
        .await
        .expect("restore recovered bridge");
        recovered_bridge
            .admit(
                &authorization,
                &activity_with_id(TENANT_ID, "legacy/activity"),
                NOW,
            )
            .await
            .expect("fresh signed Activity reaches real resident");
        let recovered_event = resident
            .next_event(&legacy_thread, Duration::ZERO, Duration::from_secs(30))
            .await
            .expect("lease recovered resident event")
            .expect("real resident receives Teams notification");
        assert_eq!(
            recovered_event.event["params"]["meta"]["message_id"],
            "legacy/activity"
        );

        let poisoned = crate::teams::runtime::TeamsResidentBridge::new(
            edge_for_poison,
            crate::composition::teams_admission::from_codex(queue.clone()),
        );
        poisoned.poison_pending_for_test();
        let token_count = fixture.token_forms.lock().expect("token forms lock").len();
        let reply_error = poisoned
            .reply(
                &crate::teams::ReplyHandle::parse("teams-00000000-0000-4000-8000-000000000099")
                    .unwrap(),
                "reply",
            )
            .await
            .expect_err("poisoned registry must return a tool error");
        assert!(matches!(
            reply_error,
            crate::teams::runtime::TeamsRuntimeError {
                kind: crate::teams::TeamsRuntimeErrorKind::RegistryPoisoned
            }
        ));
        assert_eq!(
            fixture.token_forms.lock().expect("token forms lock").len(),
            token_count,
            "registry poison must fail before token acquisition"
        );
        let queued_before = queue.status().await.queued;
        let mut headers = HeaderMap::new();
        headers.insert(
            "authorization",
            authorization.parse().expect("signed Authorization header"),
        );
        let response = crate::teams::runtime::messages(
            State(Arc::new(poisoned)),
            headers,
            Bytes::from(activity_with_id(TENANT_ID, "poisoned/activity")),
        )
        .await
        .into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(queue.status().await.queued, queued_before);

        consumer.acknowledge(&second.delivery_token).await.unwrap();
        bridge
            .admit(
                &authorization,
                &activity_with_id(TENANT_ID, "invalid-receipt/activity"),
                NOW,
            )
            .await
            .unwrap();
        let invalid_receipt_event = consumer
            .next_event(&resident_thread, Duration::ZERO, Duration::from_secs(30))
            .await
            .unwrap()
            .unwrap();
        let invalid_receipt_handle = crate::teams::ReplyHandle::parse(
            invalid_receipt_event.event["params"]["meta"]["reply_handle"]
                .as_str()
                .unwrap(),
        )
        .unwrap();
        let malformed_receipt = bridge
            .reply(&invalid_receipt_handle, "simulate malformed receipt")
            .await
            .unwrap_err();
        assert!(
            matches!(malformed_receipt, crate::teams::runtime::TeamsRuntimeError { kind: crate::teams::TeamsRuntimeErrorKind::ReplyOutcomeUncertain(TeamsEdgeError { kind: TeamsEdgeErrorKind::Http(error) }) } if error.is_decode())
        );
        assert_eq!(fixture.replies.lock().unwrap().len(), 3);
        assert_eq!(
            fixture.replies.lock().unwrap()[2].body["text"],
            "simulate malformed receipt"
        );
        assert_eq!(fixture.token_forms.lock().unwrap().len(), 3);
        assert!(matches!(
            bridge
                .reply(&invalid_receipt_handle, "must not retry malformed receipt")
                .await,
            Err(crate::teams::runtime::TeamsRuntimeError {
                kind: crate::teams::TeamsRuntimeErrorKind::UnknownReplyHandle
            })
        ));
        assert_eq!(fixture.replies.lock().unwrap().len(), 3);
        assert_eq!(fixture.token_forms.lock().unwrap().len(), 3);
        let stored: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(queue_path.join("teams-reply-authorities.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        assert!(
            stored["pending"]
                .get(invalid_receipt_handle.as_str())
                .is_none()
        );
        for outcome in [
            SinkFailure::Duplicate,
            SinkFailure::Rejected,
            SinkFailure::CommittedUncertain,
            SinkFailure::CleanupFailure,
        ] {
            let directory = TempDir::new().unwrap();
            let path = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
            let failed_bridge = crate::teams::runtime::TeamsResidentBridge::new_durable(
                edge_for_failures.clone(),
                crate::teams::delivery::TeamsDelivery::new(FailingSink {
                    path: path.clone(),
                    outcome,
                }),
                &path,
            )
            .await
            .unwrap();
            let result = failed_bridge
                .admit(
                    &authorization,
                    &activity_with_id(TENANT_ID, "classified/admission"),
                    NOW,
                )
                .await;
            let state_file = path.join("teams-reply-authorities.json");
            match outcome {
                SinkFailure::Duplicate => {
                    result.unwrap();
                    let state: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(state_file).unwrap()).unwrap();
                    assert_eq!(state["pending"].as_object().unwrap().len(), 0);
                }
                SinkFailure::Rejected => {
                    let error = result.unwrap_err();
                    assert!(
                        matches!(&error.kind, crate::teams::TeamsRuntimeErrorKind::Resident(source) if matches!(&source.kind, crate::teams::delivery::TeamsDeliveryErrorKind::Storage(cause) if cause.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied)))
                    );
                    assert_eq!(error.to_string(), "resident delivery failed");
                    let state: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(state_file).unwrap()).unwrap();
                    assert_eq!(state["pending"].as_object().unwrap().len(), 0);
                }
                SinkFailure::CommittedUncertain => {
                    let error = result.unwrap_err();
                    assert!(matches!(
                        error.kind,
                        crate::teams::TeamsRuntimeErrorKind::ResidentCommittedUncertain(_)
                    ));
                    let state: serde_json::Value =
                        serde_json::from_slice(&std::fs::read(state_file).unwrap()).unwrap();
                    let pending = state["pending"].as_object().unwrap();
                    assert_eq!(pending.len(), 1);
                    let handle =
                        crate::teams::ReplyHandle::parse(pending.keys().next().unwrap()).unwrap();
                    assert!(
                        failed_bridge
                            .reply(&handle, "retained after uncertain admission")
                            .await
                            .is_ok()
                    );
                }
                SinkFailure::CleanupFailure => {
                    let error = result.unwrap_err();
                    assert_eq!(
                        error.to_string(),
                        "resident delivery rejected and authority cleanup failed"
                    );
                    let crate::teams::TeamsRuntimeErrorKind::AdmissionCleanup { delivery, cleanup } =
                        error.kind
                    else {
                        panic!("both independent failures must survive")
                    };
                    assert!(
                        matches!(delivery.kind, crate::teams::delivery::TeamsDeliveryErrorKind::Storage(source) if source.downcast_ref::<std::io::Error>().is_some_and(|io| io.kind() == std::io::ErrorKind::PermissionDenied))
                    );
                    assert!(matches!(
                        cleanup.kind,
                        crate::teams::runtime::AuthorityCleanupErrorKind::Rejected(
                            crate::teams::ReplyStoreError {
                                kind: crate::teams::ReplyStoreErrorKind::Io(_)
                            }
                        )
                    ));
                    let state: serde_json::Value = serde_json::from_slice(
                        &std::fs::read(state_file.with_extension("before-cleanup")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(state["pending"].as_object().unwrap().len(), 1);
                }
            }
        }
        assert_owned_admission_completion(edge_for_failures.clone(), &authorization, &fixture)
            .await;
        assert_authority_commit_phases(edge_for_failures, &authorization, &fixture).await;
        connector_fixture.task.abort();
    }

    async fn assert_owned_admission_completion(
        edge: TeamsEdge<ClientSecretTokenProvider>,
        authorization: &str,
        fixture: &FixtureState,
    ) {
        for lose_response in [false, true] {
            let directory = TempDir::new().unwrap();
            let path = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
            let scope = crate::codex::LegacyDiscordScope::new(crate::core::ModuleInstanceId::new(
                crate::core::ProviderId::new("discord").unwrap(),
                crate::core::ModuleSlotId::new("primary").unwrap(),
            ));
            let queue = crate::codex::CodexEventQueue::open(&path, scope.clone())
                .await
                .unwrap();
            let consumer = queue.register_live_consumer().await.unwrap();
            let consumer_id = consumer.consumer_id().clone();
            let thread = crate::codex::CodexThreadId::parse("owned-admission").unwrap();
            consumer
                .bind_live_thread(Some(thread.clone()))
                .await
                .unwrap();
            let bridge = Arc::new(
                crate::teams::runtime::TeamsResidentBridge::new_durable(
                    edge.clone(),
                    crate::composition::teams_admission::from_codex(queue.clone()),
                    &path,
                )
                .await
                .unwrap(),
            );
            let (reached, release) = queue.pause_owned_completion_for_test(lose_response);
            let submitter = bridge.clone();
            let authorization = authorization.to_owned();
            let admission = tokio::spawn(async move {
                submitter
                    .admit(
                        &authorization,
                        &activity_with_id(TENANT_ID, "owned/admission"),
                        NOW,
                    )
                    .await
            });
            tokio::time::timeout(Duration::from_secs(2), reached)
                .await
                .unwrap()
                .unwrap();
            if lose_response {
                release.send(()).unwrap();
                let error = admission.await.unwrap().unwrap_err();
                assert!(matches!(error.kind,
                    crate::teams::TeamsRuntimeErrorKind::ResidentCompletionUnknown(
                        crate::teams::delivery::TeamsDeliveryError {
                            kind: crate::teams::delivery::TeamsDeliveryErrorKind::CompletionUnknown(source)
                        }
                    ) if source.is_panic()));
            } else {
                admission.abort();
                assert!(admission.await.unwrap_err().is_cancelled());
                release.send(()).unwrap();
            }
            queue.drain_owned().await;
            drop(bridge);
            drop(consumer);
            drop(queue);
            let queue = crate::codex::CodexEventQueue::open(&path, scope)
                .await
                .unwrap();
            let consumer = queue.register_live_consumer().await.unwrap();
            assert_eq!(consumer.consumer_id(), &consumer_id);
            let bridge = crate::teams::runtime::TeamsResidentBridge::new_durable(
                edge.clone(),
                crate::composition::teams_admission::from_codex(queue.clone()),
                &path,
            )
            .await
            .unwrap();
            let event = consumer
                .next_event(&thread, Duration::ZERO, Duration::from_secs(30))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(
                event.event["params"]["meta"]["message_id"],
                "owned/admission"
            );
            let handle = crate::teams::ReplyHandle::parse(
                event.event["params"]["meta"]["reply_handle"]
                    .as_str()
                    .unwrap(),
            )
            .unwrap();
            let tokens_before = fixture.token_forms.lock().unwrap().len();
            let replies_before = fixture.replies.lock().unwrap().len();
            bridge
                .reply(&handle, "reply after owned admission restart")
                .await
                .unwrap();
            assert_eq!(fixture.replies.lock().unwrap().len(), replies_before + 1);
            assert_eq!(fixture.token_forms.lock().unwrap().len(), tokens_before + 1);
            assert!(matches!(
                bridge.reply(&handle, "must not send twice").await,
                Err(TeamsRuntimeError {
                    kind: crate::teams::TeamsRuntimeErrorKind::UnknownReplyHandle
                })
            ));
            assert_eq!(fixture.replies.lock().unwrap().len(), replies_before + 1);
            assert_eq!(fixture.token_forms.lock().unwrap().len(), tokens_before + 1);
        }
    }

    #[derive(Clone, Copy)]
    enum SinkFailure {
        Duplicate,
        Rejected,
        CommittedUncertain,
        CleanupFailure,
    }

    struct FailingSink {
        path: camino::Utf8PathBuf,
        outcome: SinkFailure,
    }

    impl crate::teams::delivery::TeamsEventSink for FailingSink {
        fn deliver<'a>(
            &'a self,
            _event: crate::teams::delivery::TeamsResidentEvent,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::teams::delivery::TeamsAdmissionOutcome,
                            crate::teams::delivery::TeamsDeliveryError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                if matches!(self.outcome, SinkFailure::Duplicate) {
                    return Ok(crate::teams::delivery::TeamsAdmissionOutcome::Duplicate);
                }
                if matches!(self.outcome, SinkFailure::CleanupFailure) {
                    let file = self.path.join("teams-reply-authorities.json");
                    std::fs::rename(&file, file.with_extension("before-cleanup")).unwrap();
                    std::fs::create_dir(&file).unwrap();
                }
                let error = crate::teams::delivery::TeamsDeliveryError {
                    kind: crate::teams::delivery::TeamsDeliveryErrorKind::Storage(Box::new(
                        std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "private delivery cause sentinel",
                        ),
                    )),
                };
                if matches!(self.outcome, SinkFailure::CommittedUncertain) {
                    Ok(
                        crate::teams::delivery::TeamsAdmissionOutcome::CommittedDurabilityUncertain(
                            error,
                        ),
                    )
                } else {
                    Err(error)
                }
            })
        }
    }

    #[derive(Clone, Copy)]
    enum StoreFaultOutcome {
        Pass,
        DuplicateAfterRename,
        RejectedAfterRename,
    }

    struct StoreFaultSink {
        store: Arc<std::sync::OnceLock<crate::teams::reply_store::AsyncTeamsReplyStore>>,
        outcome: StoreFaultOutcome,
        handle: Arc<std::sync::OnceLock<crate::teams::ReplyHandle>>,
    }

    impl crate::teams::delivery::TeamsEventSink for StoreFaultSink {
        fn deliver<'a>(
            &'a self,
            event: crate::teams::delivery::TeamsResidentEvent,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<
                        Output = Result<
                            crate::teams::delivery::TeamsAdmissionOutcome,
                            crate::teams::delivery::TeamsDeliveryError,
                        >,
                    > + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                assert!(self.handle.set(event.reply_handle).is_ok());
                if matches!(self.outcome, StoreFaultOutcome::Pass) {
                    return Ok(crate::teams::delivery::TeamsAdmissionOutcome::Committed);
                }
                self.store
                    .get()
                    .unwrap()
                    .set_persist_failure_for_test(
                        Some(crate::teams::reply_store::PersistFailure::AfterRename),
                        false,
                    )
                    .await;
                match self.outcome {
                    StoreFaultOutcome::Pass => unreachable!(),
                    StoreFaultOutcome::DuplicateAfterRename => {
                        Ok(crate::teams::delivery::TeamsAdmissionOutcome::Duplicate)
                    }
                    StoreFaultOutcome::RejectedAfterRename => {
                        Err(crate::teams::delivery::TeamsDeliveryError {
                            kind: crate::teams::delivery::TeamsDeliveryErrorKind::Storage(
                                Box::new(std::io::Error::new(
                                    std::io::ErrorKind::PermissionDenied,
                                    "retained admission failure",
                                )),
                            ),
                        })
                    }
                }
            })
        }
    }

    async fn stored_handle(path: &camino::Utf8Path) -> crate::teams::ReplyHandle {
        let state: serde_json::Value = serde_json::from_slice(
            &tokio::fs::read(path.join("teams-reply-authorities.json"))
                .await
                .unwrap(),
        )
        .unwrap();
        let pending = state["pending"].as_object().unwrap();
        assert_eq!(pending.len(), 1);
        crate::teams::ReplyHandle::parse(pending.keys().next().unwrap()).unwrap()
    }

    async fn assert_authority_commit_phases(
        edge: TeamsEdge<ClientSecretTokenProvider>,
        authorization: &str,
        fixture: &FixtureState,
    ) {
        for prior_commit in [false, true] {
            let directory = TempDir::new().unwrap();
            let path = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
            let slot = Arc::new(std::sync::OnceLock::new());
            let captured_handle = Arc::new(std::sync::OnceLock::new());
            let bridge = crate::teams::runtime::TeamsResidentBridge::new_durable(
                edge.clone(),
                crate::teams::delivery::TeamsDelivery::new(StoreFaultSink {
                    store: slot.clone(),
                    handle: captured_handle.clone(),
                    outcome: StoreFaultOutcome::Pass,
                }),
                &path,
            )
            .await
            .unwrap();
            let store = bridge.authority_store_for_test();
            assert!(slot.set(store.clone()).is_ok());
            let before_sends = fixture.replies.lock().unwrap().len();
            if prior_commit {
                store
                    .set_persist_failure_for_test(
                        Some(crate::teams::reply_store::PersistFailure::AfterRename),
                        true,
                    )
                    .await;
                let mint = bridge
                    .admit(
                        authorization,
                        &activity_with_id(TENANT_ID, "mint/uncertain"),
                        NOW,
                    )
                    .await
                    .unwrap_err();
                assert!(
                    matches!(mint.kind, crate::teams::TeamsRuntimeErrorKind::AuthorityCommittedUncertain { operation: crate::teams::runtime::ReplyAuthorityOperation::Mint, source } if source.kind() == std::io::ErrorKind::Other)
                );
            } else {
                bridge
                    .admit(
                        authorization,
                        &activity_with_id(TENANT_ID, "take/uncertain"),
                        NOW,
                    )
                    .await
                    .unwrap();
                store
                    .set_persist_failure_for_test(
                        Some(crate::teams::reply_store::PersistFailure::AfterRename),
                        false,
                    )
                    .await;
            }
            let handle = stored_handle(&path).await;
            let error = bridge
                .reply(&handle, "must not send before durable consume")
                .await
                .unwrap_err();
            if prior_commit {
                assert!(
                    matches!(error.kind, crate::teams::TeamsRuntimeErrorKind::ReplyAuthority(crate::teams::ReplyStoreError { kind: crate::teams::ReplyStoreErrorKind::DurabilityUncertain(source) }) if source.kind() == std::io::ErrorKind::Other)
                );
            } else {
                assert!(
                    matches!(error.kind, crate::teams::TeamsRuntimeErrorKind::AuthorityCommittedUncertain { operation: crate::teams::runtime::ReplyAuthorityOperation::Consume, source } if source.kind() == std::io::ErrorKind::Other)
                );
            }
            assert_eq!(fixture.replies.lock().unwrap().len(), before_sends);
            store.set_persist_failure_for_test(None, false).await;
            let recovered = bridge
                .reply(&handle, "safe only after rejected consumption")
                .await;
            if prior_commit {
                assert_eq!(
                    recovered.unwrap().outgoing_activity_id.as_str(),
                    "outgoing-activity-id"
                );
                assert_eq!(fixture.replies.lock().unwrap().len(), before_sends + 1);
            } else {
                assert!(matches!(
                    recovered,
                    Err(crate::teams::runtime::TeamsRuntimeError {
                        kind: crate::teams::TeamsRuntimeErrorKind::UnknownReplyHandle
                    })
                ));
                assert_eq!(fixture.replies.lock().unwrap().len(), before_sends);
            }
        }
        for outcome in [
            StoreFaultOutcome::DuplicateAfterRename,
            StoreFaultOutcome::RejectedAfterRename,
        ] {
            let directory = TempDir::new().unwrap();
            let path = camino::Utf8PathBuf::from_path_buf(directory.path().to_owned()).unwrap();
            let slot = Arc::new(std::sync::OnceLock::new());
            let captured_handle = Arc::new(std::sync::OnceLock::new());
            let bridge = crate::teams::runtime::TeamsResidentBridge::new_durable(
                edge.clone(),
                crate::teams::delivery::TeamsDelivery::new(StoreFaultSink {
                    store: slot.clone(),
                    handle: captured_handle.clone(),
                    outcome,
                }),
                &path,
            )
            .await
            .unwrap();
            let store = bridge.authority_store_for_test();
            assert!(slot.set(store.clone()).is_ok());
            let before_sends = fixture.replies.lock().unwrap().len();
            let error = bridge
                .admit(
                    authorization,
                    &activity_with_id(TENANT_ID, "cleanup/uncertain"),
                    NOW,
                )
                .await
                .unwrap_err();
            match outcome {
                StoreFaultOutcome::DuplicateAfterRename => assert!(
                    matches!(error.kind, crate::teams::TeamsRuntimeErrorKind::AuthorityCommittedUncertain { operation: crate::teams::runtime::ReplyAuthorityOperation::Discard, source } if source.kind() == std::io::ErrorKind::Other)
                ),
                StoreFaultOutcome::RejectedAfterRename => {
                    let crate::teams::TeamsRuntimeErrorKind::AdmissionCleanup { delivery, cleanup } =
                        error.kind
                    else {
                        panic!("retain both delivery and cleanup failures")
                    };
                    assert!(
                        matches!(delivery.kind, crate::teams::delivery::TeamsDeliveryErrorKind::Storage(source) if source.downcast_ref::<std::io::Error>().is_some_and(|cause| cause.kind() == std::io::ErrorKind::PermissionDenied))
                    );
                    assert!(
                        matches!(cleanup.kind, crate::teams::runtime::AuthorityCleanupErrorKind::CommittedDurabilityUncertain(source) if source.kind() == std::io::ErrorKind::Other)
                    );
                }
                StoreFaultOutcome::Pass => unreachable!(),
            }
            assert_eq!(fixture.replies.lock().unwrap().len(), before_sends);
            let state: serde_json::Value = serde_json::from_slice(
                &tokio::fs::read(path.join("teams-reply-authorities.json"))
                    .await
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(state["pending"].as_object().unwrap().len(), 0);
            store.set_persist_failure_for_test(None, false).await;
            let handle = captured_handle.get().unwrap().clone();
            let still_owned =
                crate::teams::reply_store::AsyncTeamsReplyStore::load(&path, fixture_policy())
                    .await
                    .err()
                    .expect("live authority owners must retain the lock");
            assert!(matches!(
                still_owned.kind,
                crate::teams::ReplyStoreErrorKind::Owned(ref source)
                    if source.kind() == std::io::ErrorKind::WouldBlock
            ));
            drop(bridge);
            drop(store);
            drop(slot);
            let reopened =
                crate::teams::reply_store::AsyncTeamsReplyStore::load(&path, fixture_policy())
                    .await
                    .unwrap();
            assert!(matches!(
                reopened.get(handle, chrono::Utc::now()).await,
                Err(crate::teams::ReplyStoreError {
                    kind: crate::teams::ReplyStoreErrorKind::UnknownHandle
                })
            ));
        }
    }

    fn fixture_policy() -> AdmissionPolicy {
        AdmissionPolicy {
            app_id: APP_ID.parse().unwrap(),
            tenant_id: TENANT_ID.parse().unwrap(),
            allowed_conversations: BTreeSet::from(["conversation/id".parse().unwrap()]),
            allowed_service_hosts: BTreeSet::from(["connector.test".to_owned()]),
            channels: BTreeMap::from([(
                "msteams".to_owned(),
                crate::teams::ChannelPolicy {
                    requires_key_endorsement: true,
                },
            )]),
        }
    }

    fn signed_authorization(service_url: &str) -> String {
        signed_authorization_at(service_url, NOW)
    }

    fn signed_authorization_at(service_url: &str, now: u64) -> String {
        let mut header = Header::new(Algorithm::RS256);
        header.kid = Some("fixture-rsa-1".to_owned());
        let token = encode(
            &header,
            &json!({
                "iss": "https://api.botframework.com",
                "aud": APP_ID,
                "exp": now + 600,
                "serviceurl": service_url
            }),
            &EncodingKey::from_rsa_pem(PRIVATE_KEY).expect("fixture private key"),
        )
        .expect("sign fixture connector token");
        format!("Bearer {token}")
    }

    fn activity(tenant_id: &str) -> Vec<u8> {
        activity_with_id(tenant_id, "activity/id")
    }

    fn activity_with_id(tenant_id: &str, activity_id: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({
            "type": "message",
            "id": activity_id,
            "serviceUrl": "https://connector.test/",
            "channelId": "msteams",
            "from": {"id": "sender-id"},
            "recipient": {"id": "bot-id"},
            "conversation": {"id": "conversation/id"},
            "channelData": {"tenant": {"id": tenant_id}},
            "text": "hello"
        }))
        .expect("serialize fixture Activity")
    }

    async fn openid() -> Json<serde_json::Value> {
        Json(json!({
            "issuer": "https://api.botframework.com",
            "jwks_uri": "https://login.botframework.com/v1/keys",
            "id_token_signing_alg_values_supported": ["RS256"]
        }))
    }

    async fn jwks() -> Json<serde_json::Value> {
        Json(serde_json::from_slice(JWKS_DOCUMENT).expect("fixture JWKS JSON"))
    }

    async fn token(
        State(state): State<FixtureState>,
        Form(form): Form<HashMap<String, String>>,
    ) -> Json<serde_json::Value> {
        state
            .token_forms
            .lock()
            .expect("token forms lock")
            .push(form);
        Json(json!({
            "access_token": "fixture-access-token",
            "expires_in": 3599
        }))
    }

    async fn reply(
        State(state): State<FixtureState>,
        OriginalUri(uri): OriginalUri,
        headers: HeaderMap,
        body: Bytes,
    ) -> (StatusCode, Json<serde_json::Value>) {
        let body: serde_json::Value = serde_json::from_slice(&body).expect("reply body JSON");
        let uncertain = body["text"] == "simulate uncertain outcome";
        let malformed_receipt = body["text"] == "simulate malformed receipt";
        state
            .replies
            .lock()
            .expect("replies lock")
            .push(CapturedReply {
                authorization: headers
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_owned(),
                path: uri.path().to_owned(),
                body,
            });
        if uncertain {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": "fixture accepted request"})),
            )
        } else if malformed_receipt {
            (StatusCode::OK, Json(json!({"id": ""})))
        } else {
            (StatusCode::OK, Json(json!({"id": "outgoing-activity-id"})))
        }
    }
    pub(crate) struct ConnectorFixture {
        pub(crate) edge: TeamsEdge<ClientSecretTokenProvider>,
        state: FixtureState,
        task: tokio::task::JoinHandle<std::io::Result<()>>,
    }
    impl ConnectorFixture {
        pub(crate) async fn start() -> Self {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let fixture = FixtureState::default();
            let CertifiedKey { cert, key_pair } = generate_simple_self_signed(vec![
                "connector.test".to_owned(),
                "login.botframework.com".to_owned(),
            ])
            .expect("generate fixture certificate");
            let tls = RustlsConfig::from_pem(
                cert.pem().into_bytes(),
                key_pair.serialize_pem().into_bytes(),
            )
            .await
            .expect("load fixture TLS certificate");
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind TLS fixture server");
            listener
                .set_nonblocking(true)
                .expect("configure nonblocking TLS fixture listener");
            let address = listener.local_addr().expect("fixture address");
            let app = Router::new()
                .route("/openid", get(openid))
                .route("/v1/keys", get(jwks))
                .route("/tenant/oauth2/v2.0/token", post(token))
                .route("/v3/conversations/{*rest}", post(reply))
                .with_state(fixture.clone());
            let server = tokio::spawn(async move {
                axum_server::from_tcp_rustls(listener, tls)
                    .expect("configure TLS fixture server")
                    .serve(app.into_make_service())
                    .await
            });

            let certificate = reqwest::Certificate::from_pem(cert.pem().as_bytes())
                .expect("parse fixture certificate");
            let client = Client::builder()
                .use_rustls_tls()
                .add_root_certificate(certificate)
                .resolve("connector.test", address)
                .resolve("login.botframework.com", address)
                .timeout(Duration::from_secs(2))
                .build()
                .expect("build fixture client");
            let provider = ClientSecretTokenProvider::with_endpoint(
                client.clone(),
                Url::parse("https://connector.test/tenant/oauth2/v2.0/token").expect("token URL"),
                APP_ID.parse().unwrap(),
                SecretString::from("short-lived-secret".to_owned()),
            );
            let edge = TeamsEdge::with_openid_url(
                client,
                provider,
                fixture_policy(),
                Url::parse("https://connector.test/openid").expect("OpenID URL"),
            );

            Self {
                edge,
                state: fixture,
                task: server,
            }
        }
        pub(crate) fn authorization(&self) -> String {
            signed_authorization_at("https://connector.test/", self.now())
        }
        pub(crate) fn now(&self) -> u64 {
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        }
        pub(crate) fn activity(&self, activity_id: &str) -> Vec<u8> {
            activity_with_id(TENANT_ID, activity_id)
        }
        pub(crate) fn replies(&self) -> Vec<(String, serde_json::Value)> {
            self.state
                .replies
                .lock()
                .unwrap()
                .iter()
                .map(|reply| (reply.path.clone(), reply.body.clone()))
                .collect()
        }
        pub(crate) fn token_requests(&self) -> usize {
            self.state.token_forms.lock().unwrap().len()
        }
    }
    impl Drop for ConnectorFixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }
}
