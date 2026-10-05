use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::post,
};
use dione::{
    teams::{AdmissionPolicy, ChannelPolicy},
    teams_edge::{ClientSecretTokenProvider, TeamsEdge, TeamsEdgeError},
};
use reqwest::Client;
use secrecy::SecretString;
use std::{
    collections::{BTreeMap, BTreeSet},
    env,
    net::SocketAddr,
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

#[derive(Clone)]
struct AppState {
    edge: Arc<TeamsEdge<ClientSecretTokenProvider>>,
    reply_text: Arc<str>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .try_init()
        .map_err(|error| format!("failed to initialize tracing: {error}"))?;

    let app_id = required("TEAMS_APP_ID")?;
    let tenant_id = required("TEAMS_TENANT_ID")?;
    let client_secret = SecretString::from(required("TEAMS_CLIENT_SECRET")?);
    let allowed_host = required("TEAMS_ALLOWED_SERVICE_HOST")?;
    let listen: SocketAddr = env::var("TEAMS_LISTEN_ADDR")
        .unwrap_or_else(|_| "127.0.0.1:3978".to_owned())
        .parse()?;
    let reply_text = env::var("TEAMS_REPLY_TEXT")
        .unwrap_or_else(|_| "Dione Teams edge canary received this message.".to_owned());

    let client = Client::builder()
        .https_only(true)
        .timeout(Duration::from_secs(15))
        .build()?;
    let provider =
        ClientSecretTokenProvider::new(client.clone(), &tenant_id, app_id.clone(), client_secret)?;
    let policy = AdmissionPolicy {
        app_id,
        tenant_id,
        allowed_service_hosts: BTreeSet::from([allowed_host]),
        channels: BTreeMap::from([(
            "msteams".to_owned(),
            ChannelPolicy {
                requires_key_endorsement: true,
            },
        )]),
    };
    let state = AppState {
        edge: Arc::new(TeamsEdge::new(client, provider, policy)?),
        reply_text: reply_text.into(),
    };
    let app = Router::new()
        .route("/api/messages", post(messages))
        .layer(DefaultBodyLimit::max(256 * 1024))
        .with_state(state);
    let listener = tokio::net::TcpListener::bind(listen).await?;
    tracing::info!(listen = %listener.local_addr()?, "Teams edge probe listener bound");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> impl IntoResponse {
    let Some(authorization) = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
    else {
        return StatusCode::UNAUTHORIZED;
    };
    let Ok(now) = SystemTime::now().duration_since(UNIX_EPOCH) else {
        tracing::error!("system clock is before the Unix epoch");
        return StatusCode::INTERNAL_SERVER_ERROR;
    };
    match state
        .edge
        .authenticate_and_reply(authorization, &body, now.as_secs(), &state.reply_text)
        .await
    {
        Ok(receipt) => {
            tracing::info!(
                incoming_activity_id = %receipt.incoming_activity_id,
                outgoing_activity_id = %receipt.outgoing_activity_id,
                conversation_id = %receipt.conversation_id,
                "Teams edge canary replied"
            );
            StatusCode::ACCEPTED
        }
        Err(TeamsEdgeError::Probe(_)) => StatusCode::UNAUTHORIZED,
        Err(error) => {
            tracing::error!(%error, "Teams edge canary failed");
            StatusCode::BAD_GATEWAY
        }
    }
}

fn required(name: &'static str) -> Result<String, Box<dyn std::error::Error>> {
    env::var(name).map_err(|_| format!("required environment variable {name} is unset").into())
}
