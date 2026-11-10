// SPDX-License-Identifier: Apache-2.0
// POST and its header/JSON helpers are adapted from rmcp 1.8.0 (Apache-2.0):
// https://github.com/modelcontextprotocol/rust-sdk
// crates/rmcp/src/transport/common/reqwest/streamable_http_client.rs
// and crates/rmcp/src/transport/common/http_header.rs
// Response bounds and optional-session/SSE-size hooks follow rmcp 3.9.0.
// Preserve under-limit protocol behavior and diagnostic privacy when updating this copy.
// Intentional diagnostic-only changes: omit raw decode errors and reflected
// non-protocol error bodies, strip request URLs from client errors.
use crate::http_bounds::{
    DEFAULT_MAX_SSE_EVENT_SIZE, MAX_ERROR_RESPONSE_SIZE, MAX_JSON_RESPONSE_SIZE,
    bounded_sse_response, read_bounded_body,
};
use reqwest::header::{ACCEPT, HeaderName, HeaderValue, WWW_AUTHENTICATE};
use reqwest_mcp as reqwest;
use rmcp::{
    model::{ClientJsonRpcMessage, JsonRpcMessage, ServerJsonRpcMessage},
    transport::{
        common::{
            client_side_sse::BoxedSseResponse,
            http_header::{
                EVENT_STREAM_MIME_TYPE, HEADER_LAST_EVENT_ID, HEADER_MCP_PROTOCOL_VERSION,
                HEADER_SESSION_ID, JSON_MIME_TYPE,
            },
        },
        streamable_http_client::{
            AuthRequiredError, InsufficientScopeError, StreamableHttpClient, StreamableHttpError,
            StreamableHttpPostResponse,
        },
    },
};
use std::{borrow::Cow, collections::HashMap, sync::Arc};
use tokio_util::sync::CancellationToken;

/// Reserved headers that must not be overridden by user-supplied custom headers.
/// `MCP-Protocol-Version` is in this list but is allowed through because the worker
/// injects it after initialization.
const RESERVED_HEADERS: &[&str] = &[
    "accept",
    HEADER_SESSION_ID,
    HEADER_MCP_PROTOCOL_VERSION, // allowed through by validate_custom_header; worker injects it post-init
    HEADER_LAST_EVENT_ID,
];

/// Checks whether a custom header name is allowed.
/// Returns `Ok(())` if allowed, `Err(name)` if rejected as reserved.
/// `MCP-Protocol-Version` is reserved but allowed through (the worker injects it post-init).
fn validate_custom_header(name: &HeaderName) -> Result<(), String> {
    if RESERVED_HEADERS
        .iter()
        .any(|&r| name.as_str().eq_ignore_ascii_case(r))
    {
        if name
            .as_str()
            .eq_ignore_ascii_case(HEADER_MCP_PROTOCOL_VERSION)
        {
            return Ok(());
        }
        return Err(name.to_string());
    }
    Ok(())
}

/// Extracts the `scope=` parameter from a `WWW-Authenticate` header value.
/// Handles both quoted (`scope="files:read files:write"`) and unquoted (`scope=read:data`) forms.
fn extract_scope_from_header(header: &str) -> Option<String> {
    let header_lowercase = header.to_ascii_lowercase();
    let scope_key = "scope=";

    if let Some(pos) = header_lowercase.find(scope_key) {
        let start = pos + scope_key.len();
        let value_slice = &header[start..];

        if let Some(stripped) = value_slice.strip_prefix('"') {
            if let Some(end_quote) = stripped.find('"') {
                return Some(stripped[..end_quote].to_string());
            }
        } else {
            let end = value_slice
                .find(|c: char| c == ',' || c == ';' || c.is_whitespace())
                .unwrap_or(value_slice.len());
            if end > 0 {
                return Some(value_slice[..end].to_string());
            }
        }
    }

    None
}

/// Applies custom headers to a request builder, rejecting reserved headers.
fn apply_custom_headers(
    mut builder: reqwest::RequestBuilder,
    custom_headers: HashMap<HeaderName, HeaderValue>,
) -> Result<reqwest::RequestBuilder, StreamableHttpError<reqwest::Error>> {
    for (name, value) in custom_headers {
        validate_custom_header(&name).map_err(StreamableHttpError::ReservedHeaderConflict)?;
        builder = builder.header(name, value);
    }
    Ok(builder)
}

/// Attempts to parse `body` as a JSON-RPC error message.
/// Returns `None` if the body is not parseable or is not a `JsonRpcMessage::Error`.
fn parse_json_rpc_error(body: &str) -> Option<ServerJsonRpcMessage> {
    match serde_json::from_str::<ServerJsonRpcMessage>(body) {
        Ok(message @ JsonRpcMessage::Error(_)) => Some(message),
        _ => None,
    }
}

// rmcp logs worker failures before returning them to the provider. Remove the
// request URL from reqwest errors at that boundary, without changing requests.
#[derive(Clone)]
pub(crate) struct DiagnosticHttpClient(reqwest_mcp::Client, CancellationToken);

fn without_request_url(
    error: StreamableHttpError<reqwest_mcp::Error>,
) -> StreamableHttpError<reqwest_mcp::Error> {
    match error {
        StreamableHttpError::Client(error) => StreamableHttpError::Client(error.without_url()),
        StreamableHttpError::UnexpectedContentType(value) => {
            StreamableHttpError::UnexpectedContentType(
                value.map(|_| "unsupported content type".to_owned()),
            )
        }
        // Bell discards connection failures and has no OAuth recovery consumer.
        // Preserve hint presence and protocol category, never reflected contents.
        StreamableHttpError::AuthRequired(_) => StreamableHttpError::AuthRequired(
            AuthRequiredError::new("authentication required".to_owned()),
        ),
        StreamableHttpError::InsufficientScope(value) => {
            StreamableHttpError::InsufficientScope(InsufficientScopeError::new(
                "insufficient scope".to_owned(),
                value.required_scope.map(|_| "required scope".to_owned()),
            ))
        }
        other => other,
    }
}

impl StreamableHttpClient for DiagnosticHttpClient {
    type Error = reqwest_mcp::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_inner(
            uri,
            message,
            session_id,
            auth_token,
            custom_headers,
            DEFAULT_MAX_SSE_EVENT_SIZE,
        )
        .await
        .map_err(without_request_url)
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post_message_inner(
            uri,
            message,
            session_id,
            auth_token,
            custom_headers,
            max_sse_event_size,
        )
        .await
        .map_err(without_request_url)
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        self.0
            .delete_session(uri, session_id, auth_header, custom_headers)
            .await
            .map_err(without_request_url)
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            DEFAULT_MAX_SSE_EVENT_SIZE,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
        self.get_stream_inner(
            uri,
            session_id,
            last_event_id,
            auth_header,
            custom_headers,
            max_sse_event_size,
        )
        .await
        .map_err(without_request_url)
    }
}

impl DiagnosticHttpClient {
    pub(crate) fn new(http: reqwest_mcp::Client) -> Self {
        Self(http, CancellationToken::new())
    }

    pub(crate) fn into_transport(
        self,
        config: rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig,
    ) -> rmcp::transport::StreamableHttpClientTransport<Self> {
        let cancellation = self.1.clone();
        let worker =
            rmcp::transport::streamable_http_client::StreamableHttpClientWorker::new(self, config);
        rmcp::transport::worker::WorkerTransport::spawn_with_ct(worker, cancellation)
    }

    fn ensure_connection_open(&self) -> Result<(), StreamableHttpError<reqwest_mcp::Error>> {
        if self.1.is_cancelled() {
            Err(StreamableHttpError::TransportChannelClosed)
        } else {
            Ok(())
        }
    }

    // Preserve the existing reqwest status-error category rather than adopting
    // upstream3.9's new HttpStatus variant for GET failures. SSE accounting is
    // bounded and session-less resumption follows the3.9 trait contract.
    async fn get_stream_inner(
        &self,
        uri: Arc<str>,
        session_id: Option<Arc<str>>,
        last_event_id: Option<String>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<BoxedSseResponse, StreamableHttpError<reqwest::Error>> {
        self.ensure_connection_open()?;
        let mut request = self
            .0
            .get(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(session) = session_id {
            request = request.header(HEADER_SESSION_ID, session.as_ref());
        }
        if let Some(event) = last_event_id {
            request = request.header(HEADER_LAST_EVENT_ID, event);
        }
        if let Some(auth) = auth_header {
            request = request.bearer_auth(auth);
        }
        request = apply_custom_headers(request, custom_headers)?;
        let response = request.send().await?;
        if response.status() == reqwest::StatusCode::METHOD_NOT_ALLOWED {
            return Err(StreamableHttpError::ServerDoesNotSupportSse);
        }
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        {
            let header = header.to_str().map_err(|_| {
                StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                    "invalid www-authenticate header value",
                ))
            })?;
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                header.to_owned(),
            )));
        }
        if response.status() == reqwest::StatusCode::FORBIDDEN
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        {
            let header = header.to_str().map_err(|_| {
                StreamableHttpError::UnexpectedServerResponse(Cow::Borrowed(
                    "invalid www-authenticate header value",
                ))
            })?;
            return Err(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(header.to_owned(), extract_scope_from_header(header)),
            ));
        }
        let response = response.error_for_status()?;
        match response.headers().get(reqwest::header::CONTENT_TYPE) {
            Some(value)
                if value
                    .as_bytes()
                    .starts_with(EVENT_STREAM_MIME_TYPE.as_bytes())
                    || value.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {}
            value => {
                return Err(StreamableHttpError::UnexpectedContentType(value.map(
                    |value| String::from_utf8_lossy(value.as_bytes()).into_owned(),
                )));
            }
        }
        Ok(bounded_sse_response(
            response,
            max_sse_event_size,
            self.1.clone(),
        ))
    }

    async fn post_message_inner(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_token: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<reqwest::Error>> {
        self.ensure_connection_open()?;
        let mut request = self
            .0
            .post(uri.as_ref())
            .header(ACCEPT, [EVENT_STREAM_MIME_TYPE, JSON_MIME_TYPE].join(", "));
        if let Some(auth_header) = auth_token {
            request = request.bearer_auth(auth_header);
        }

        request = apply_custom_headers(request, custom_headers)?;
        let session_was_attached = session_id.is_some();
        if let Some(session_id) = session_id {
            request = request.header(HEADER_SESSION_ID, session_id.as_ref());
        }
        let response = request.json(&message).send().await?;
        if response.status() == reqwest::StatusCode::UNAUTHORIZED
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        {
            let header = header
                .to_str()
                .map_err(|_| {
                    StreamableHttpError::UnexpectedServerResponse(Cow::from(
                        "invalid www-authenticate header value",
                    ))
                })?
                .to_string();
            return Err(StreamableHttpError::AuthRequired(AuthRequiredError::new(
                header,
            )));
        }
        if response.status() == reqwest::StatusCode::FORBIDDEN
            && let Some(header) = response.headers().get(WWW_AUTHENTICATE)
        {
            let header_str = header.to_str().map_err(|_| {
                StreamableHttpError::UnexpectedServerResponse(Cow::from(
                    "invalid www-authenticate header value",
                ))
            })?;
            let scope = extract_scope_from_header(header_str);
            return Err(StreamableHttpError::InsufficientScope(
                InsufficientScopeError::new(header_str.to_string(), scope),
            ));
        }
        let status = response.status();
        if matches!(
            status,
            reqwest::StatusCode::ACCEPTED | reqwest::StatusCode::NO_CONTENT
        ) {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        if status == reqwest::StatusCode::NOT_FOUND && session_was_attached {
            return Err(StreamableHttpError::SessionExpired);
        }
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|ct| String::from_utf8_lossy(ct.as_bytes()).to_string());
        let content_length = response.content_length();
        let session_id = response
            .headers()
            .get(HEADER_SESSION_ID)
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        // Spec requires 202 Accepted for these, but some servers return an empty 200.
        // Treat empty success responses as equivalent to Accepted.
        if status.is_success()
            && content_length == Some(0)
            && matches!(
                message,
                ClientJsonRpcMessage::Notification(_)
                    | ClientJsonRpcMessage::Response(_)
                    | ClientJsonRpcMessage::Error(_)
            )
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        // Non-success responses may carry valid JSON-RPC error payloads that
        // should be surfaced as McpError rather than lost in TransportSend.
        if !status.is_success() {
            let body = match read_bounded_body(response, MAX_ERROR_RESPONSE_SIZE).await {
                Ok(body) => String::from_utf8_lossy(&body).into_owned(),
                Err(error @ StreamableHttpError::ResponseBodyTooLarge { .. }) => return Err(error),
                Err(_) => "<failed to read response body>".to_owned(),
            };
            if content_type
                .as_deref()
                .is_some_and(|ct| ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()))
            {
                match parse_json_rpc_error(&body) {
                    Some(message) => {
                        return Ok(StreamableHttpPostResponse::Json(message, session_id));
                    }
                    None => tracing::warn!(
                        "HTTP {status}: could not parse JSON body as a JSON-RPC error"
                    ),
                }
            }
            return Err(StreamableHttpError::UnexpectedServerResponse(Cow::Owned(
                format!("HTTP {status}: provider returned an invalid error response"),
            )));
        }
        match content_type.as_deref() {
            Some(ct) if ct.as_bytes().starts_with(EVENT_STREAM_MIME_TYPE.as_bytes()) => {
                let event_stream =
                    bounded_sse_response(response, max_sse_event_size, self.1.clone());
                Ok(StreamableHttpPostResponse::Sse(event_stream, session_id))
            }
            Some(ct) if ct.as_bytes().starts_with(JSON_MIME_TYPE.as_bytes()) => {
                // Try to parse as a valid JSON-RPC message. If the body is
                // malformed (e.g. a 200 response to a notification that lacks
                // an `id` field), treat it as accepted rather than failing.
                let body = match read_bounded_body(response, MAX_JSON_RESPONSE_SIZE).await {
                    Ok(body) => body,
                    Err(error @ StreamableHttpError::ResponseBodyTooLarge { .. }) => {
                        return Err(error);
                    }
                    Err(_) => {
                        tracing::warn!(
                            "could not read JSON response as ServerJsonRpcMessage, treating as accepted"
                        );
                        return Ok(StreamableHttpPostResponse::Accepted);
                    }
                };
                match serde_json::from_slice::<ServerJsonRpcMessage>(&body) {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session_id)),
                    Err(_) => {
                        tracing::warn!(
                            "could not parse JSON response as ServerJsonRpcMessage, treating as accepted"
                        );
                        Ok(StreamableHttpPostResponse::Accepted)
                    }
                }
            }
            _ => {
                // unexpected content type
                tracing::error!(present = content_type.is_some(), "unexpected content type");
                Err(StreamableHttpError::UnexpectedContentType(content_type))
            }
        }
    }
}
