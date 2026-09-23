//! Narrow Vaelii bridge for explicit Discord message-read receipts.
//!
//! The bridge deliberately records provenance, not message content or a truth
//! claim. It is disabled unless a Vaelii server URL is configured.

use reqwest::{StatusCode, Url, redirect::Policy};
use serde::Deserialize;
use std::time::Duration;
use thiserror::Error;

const DEFAULT_TIMEOUT_MS: u64 = 3_000;
const RECEIPT_CONTEXT: &str = "CxWell";
const GET_MESSAGE_TOOL: &str = "DioneGetMessageTool";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct VaeliiConfig {
    /// Vaelii daemon base URL, or its explicit `/op` endpoint.
    pub server_url: Option<String>,
    /// Vaelii term used as the actor in `(performedBy INVOCATION ACTOR)`.
    /// Defaults to the configured Dione construct identity when absent.
    pub actor_term: Option<String>,
    /// Total request timeout. Clamped to at least one millisecond.
    pub timeout_ms: u64,
}

impl Default for VaeliiConfig {
    fn default() -> Self {
        Self {
            server_url: None,
            actor_term: None,
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }
}

impl VaeliiConfig {
    pub fn is_enabled(&self) -> bool {
        self.server_url
            .as_deref()
            .is_some_and(|url| !url.trim().is_empty())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GetMessageReceipt {
    pub invocation: String,
    pub receipt: String,
}

#[derive(Debug, Error)]
pub enum VaeliiError {
    #[error("invalid Vaelii server URL: {0}")]
    InvalidUrl(&'static str),
    #[error("invalid Vaelii symbol for {field}: {value:?}")]
    InvalidSymbol { field: &'static str, value: String },
    #[error("Vaelii receipt request failed")]
    Request(#[from] reqwest::Error),
    #[error("Vaelii receipt write returned HTTP {0}")]
    Http(StatusCode),
    #[error("Vaelii receipt batch violates the naked_term invariant: {0}")]
    NakedTermInvariant(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Sentex(Vec<Atom>);

#[derive(Debug, Clone, PartialEq, Eq)]
enum Atom {
    Symbol(String),
    String(String),
}

impl Sentex {
    fn relation(&self) -> Option<&str> {
        match self.0.first() {
            Some(Atom::Symbol(symbol)) => Some(symbol),
            _ => None,
        }
    }

    fn mentions_symbol(&self, term: &str) -> bool {
        self.0
            .iter()
            .any(|atom| matches!(atom, Atom::Symbol(symbol) if symbol == term))
    }

    fn to_edn(&self) -> String {
        let atoms = self
            .0
            .iter()
            .map(Atom::to_edn)
            .collect::<Vec<_>>()
            .join(" ");
        format!("({atoms})")
    }
}

impl Atom {
    fn to_edn(&self) -> String {
        match self {
            Self::Symbol(value) => value.clone(),
            Self::String(value) => format!("\"{}\"", escape_edn_string(value)),
        }
    }
}

/// Record the six provenance-only sentexes for an explicit `get_message`.
///
/// Returns `Ok(None)` when no Vaelii URL is configured. The caller controls
/// whether and how a configured-write failure is surfaced to the MCP client.
pub async fn write_get_message_receipt(
    config: &VaeliiConfig,
    message_id: u64,
    construct_id: &str,
) -> Result<Option<GetMessageReceipt>, VaeliiError> {
    let Some(server_url) = config
        .server_url
        .as_deref()
        .map(str::trim)
        .filter(|url| !url.is_empty())
    else {
        return Ok(None);
    };

    let actor_term = config.actor_term.as_deref().unwrap_or(construct_id);
    validate_symbol("actor_term", actor_term)?;
    let endpoint = operation_endpoint(server_url)?;
    let receipt = GetMessageReceipt {
        invocation: format!("Check{message_id}"),
        receipt: format!("Receipt{message_id}"),
    };
    let sentexes = get_message_sentexes(message_id, actor_term, &receipt);
    validate_naked_term_invariant(&sentexes)?;
    let body = edit_body(&sentexes);

    let client = reqwest::Client::builder()
        .redirect(Policy::none())
        .timeout(Duration::from_millis(config.timeout_ms.max(1)))
        .build()?;
    let mut request = client
        .post(endpoint)
        .header(reqwest::header::CONTENT_TYPE, "application/edn")
        .body(body);
    if let Ok(token) = std::env::var("VAELII_API_TOKEN")
        && !token.is_empty()
    {
        request = request.bearer_auth(token);
    }
    let response = request.send().await?;
    if !response.status().is_success() {
        return Err(VaeliiError::Http(response.status()));
    }

    Ok(Some(receipt))
}

fn get_message_sentexes(
    message_id: u64,
    construct_id: &str,
    receipt: &GetMessageReceipt,
) -> Vec<Sentex> {
    let invocation = receipt.invocation.clone();
    let receipt_term = receipt.receipt.clone();
    vec![
        sentex([symbol("tool_invocation"), symbol(&invocation)]),
        sentex([
            symbol("invokesTool"),
            symbol(&invocation),
            symbol(GET_MESSAGE_TOOL),
        ]),
        sentex([
            symbol("toolInvocationArg"),
            symbol(&invocation),
            string("message_id"),
            string(&message_id.to_string()),
        ]),
        sentex([
            symbol("performedBy"),
            symbol(&invocation),
            symbol(construct_id),
        ]),
        sentex([symbol("tool_receipt"), symbol(&receipt_term)]),
        sentex([
            symbol("receipt"),
            symbol(&invocation),
            symbol(&receipt_term),
        ]),
    ]
}

fn edit_body(sentexes: &[Sentex]) -> String {
    let additions = sentexes
        .iter()
        .map(|sentex| format!("[{} {RECEIPT_CONTEXT}]", sentex.to_edn()))
        .collect::<Vec<_>>()
        .join(" ");
    format!("{{:op :edit :args [{{:add [{additions}]}}]}}")
}

/// Enforce the client-side tombstone rule before any assertion batch leaves
/// Dione: a naked term may occur only in its sole `(naked_term TERM)` sentex.
fn validate_naked_term_invariant(sentexes: &[Sentex]) -> Result<(), VaeliiError> {
    for declaration in sentexes
        .iter()
        .filter(|sentex| sentex.relation() == Some("naked_term"))
    {
        let Some(Atom::Symbol(term)) = declaration.0.get(1) else {
            return Err(VaeliiError::NakedTermInvariant(
                "naked_term requires one symbolic term".to_owned(),
            ));
        };
        if declaration.0.len() != 2 {
            return Err(VaeliiError::NakedTermInvariant(format!(
                "(naked_term {term}) must be unary"
            )));
        }
        let mentioning = sentexes
            .iter()
            .filter(|sentex| sentex.mentions_symbol(term))
            .count();
        if mentioning != 1 {
            return Err(VaeliiError::NakedTermInvariant(format!(
                "{term} occurs in {mentioning} asserted sentexes"
            )));
        }
    }
    Ok(())
}

fn operation_endpoint(server_url: &str) -> Result<Url, VaeliiError> {
    let mut url = Url::parse(server_url).map_err(|_| VaeliiError::InvalidUrl("not a URL"))?;
    if !matches!(url.scheme(), "http" | "https") {
        return Err(VaeliiError::InvalidUrl("scheme must be http or https"));
    }
    if !url.username().is_empty() || url.password().is_some() {
        return Err(VaeliiError::InvalidUrl(
            "embedded credentials are forbidden",
        ));
    }
    if url.query().is_some() || url.fragment().is_some() {
        return Err(VaeliiError::InvalidUrl(
            "query strings and fragments are forbidden",
        ));
    }
    let path = url.path().trim_end_matches('/');
    if !path.ends_with("/op") && path != "op" {
        let new_path = if path.is_empty() {
            "/op".to_owned()
        } else {
            format!("{path}/op")
        };
        url.set_path(&new_path);
    }
    Ok(url)
}

fn validate_symbol(field: &'static str, value: &str) -> Result<(), VaeliiError> {
    let valid = value
        .bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'?' | b'!'));
    if valid {
        Ok(())
    } else {
        Err(VaeliiError::InvalidSymbol {
            field,
            value: value.to_owned(),
        })
    }
}

fn sentex<const N: usize>(atoms: [Atom; N]) -> Sentex {
    Sentex(Vec::from(atoms))
}

fn symbol(value: &str) -> Atom {
    Atom::Symbol(value.to_owned())
}

fn string(value: &str) -> Atom {
    Atom::String(value.to_owned())
}

fn escape_edn_string(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| match character {
            '\\' => "\\\\".chars().collect::<Vec<_>>(),
            '"' => "\\\"".chars().collect(),
            '\n' => "\\n".chars().collect(),
            '\r' => "\\r".chars().collect(),
            '\t' => "\\t".chars().collect(),
            other => vec![other],
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    async fn one_shot_server(
        status: &'static str,
    ) -> (
        String,
        Arc<Mutex<Option<String>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind fake Vaelii API");
        let address = listener.local_addr().expect("fake Vaelii API address");
        let captured = Arc::new(Mutex::new(None));
        let request_slot = Arc::clone(&captured);
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept Vaelii request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.expect("read Vaelii request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
                let Some(header_end) = request.windows(4).position(|window| window == b"\r\n\r\n")
                else {
                    continue;
                };
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let content_length = headers
                    .lines()
                    .find_map(|line| {
                        line.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .and_then(|value| value.trim().parse::<usize>().ok())
                    })
                    .unwrap_or_default();
                if request.len() >= header_end + 4 + content_length {
                    break;
                }
            }
            *request_slot.lock().expect("request capture lock") =
                Some(String::from_utf8(request).expect("HTTP request is UTF-8"));
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/edn\r\nContent-Length: 10\r\nConnection: close\r\n\r\n{{:ok true}}"
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write Vaelii response");
        });
        (format!("http://{address}"), captured, server)
    }

    #[test]
    fn exact_get_message_batch_has_six_provenance_sentexes_and_no_content() {
        let receipt = GetMessageReceipt {
            invocation: "Check1546542838244843671".to_owned(),
            receipt: "Receipt1546542838244843671".to_owned(),
        };
        let sentexes = get_message_sentexes(1546542838244843671, "Syne", &receipt);
        assert_eq!(sentexes.len(), 6);
        assert_eq!(
            edit_body(&sentexes),
            concat!(
                "{:op :edit :args [{:add [",
                "[(tool_invocation Check1546542838244843671) CxWell] ",
                "[(invokesTool Check1546542838244843671 DioneGetMessageTool) CxWell] ",
                "[(toolInvocationArg Check1546542838244843671 \"message_id\" \"1546542838244843671\") CxWell] ",
                "[(performedBy Check1546542838244843671 Syne) CxWell] ",
                "[(tool_receipt Receipt1546542838244843671) CxWell] ",
                "[(receipt Check1546542838244843671 Receipt1546542838244843671) CxWell]",
                "]}]}"
            )
        );
    }

    #[test]
    fn naked_term_must_be_the_only_sentex_mentioning_its_term() {
        let valid = vec![sentex([symbol("naked_term"), symbol("oldTerm")])];
        assert!(validate_naked_term_invariant(&valid).is_ok());

        let invalid = vec![
            sentex([symbol("naked_term"), symbol("oldTerm")]),
            sentex([symbol("comment"), symbol("oldTerm"), string("still here")]),
        ];
        assert!(matches!(
            validate_naked_term_invariant(&invalid),
            Err(VaeliiError::NakedTermInvariant(_))
        ));
    }

    #[test]
    fn operation_url_is_scoped_and_rejects_credential_bearing_urls() {
        assert_eq!(
            operation_endpoint("https://vaelii.example/base")
                .unwrap()
                .as_str(),
            "https://vaelii.example/base/op"
        );
        assert_eq!(
            operation_endpoint("http://127.0.0.1:4200/op")
                .unwrap()
                .as_str(),
            "http://127.0.0.1:4200/op"
        );
        assert!(operation_endpoint("https://secret@vaelii.example").is_err());
    }

    #[test]
    fn actor_term_must_serialize_as_an_unambiguous_symbol() {
        assert!(validate_symbol("actor_term", "Syne").is_ok());
        assert!(validate_symbol("actor_term", "mnemosyne-2").is_ok());
        assert!(validate_symbol("actor_term", "123").is_err());
        assert!(validate_symbol("actor_term", "not a symbol").is_err());
    }

    #[tokio::test]
    async fn no_configured_url_means_no_receipt_write() {
        let result = write_get_message_receipt(&VaeliiConfig::default(), 42, "Syne")
            .await
            .unwrap();
        assert_eq!(result, None);
    }

    #[tokio::test]
    async fn configured_writer_posts_exact_atomic_batch_to_operation_endpoint() {
        let (server_url, captured, server) = one_shot_server("200 OK").await;
        let config = VaeliiConfig {
            server_url: Some(server_url),
            actor_term: Some("Syne".to_owned()),
            ..VaeliiConfig::default()
        };

        let receipt = write_get_message_receipt(&config, 42, "ignored")
            .await
            .unwrap()
            .unwrap();
        server.await.unwrap();
        assert_eq!(receipt.invocation, "Check42");
        assert_eq!(receipt.receipt, "Receipt42");

        let request = captured
            .lock()
            .expect("request capture lock")
            .clone()
            .expect("captured request");
        assert!(request.starts_with("POST /op HTTP/1.1\r\n"));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("content-type: application/edn\r\n")
        );
        let body = request.split_once("\r\n\r\n").unwrap().1;
        assert_eq!(
            body,
            concat!(
                "{:op :edit :args [{:add [",
                "[(tool_invocation Check42) CxWell] ",
                "[(invokesTool Check42 DioneGetMessageTool) CxWell] ",
                "[(toolInvocationArg Check42 \"message_id\" \"42\") CxWell] ",
                "[(performedBy Check42 Syne) CxWell] ",
                "[(tool_receipt Receipt42) CxWell] ",
                "[(receipt Check42 Receipt42) CxWell]",
                "]}]}"
            )
        );
    }

    #[tokio::test]
    async fn configured_write_refusal_is_reported_without_response_body_leakage() {
        let (server_url, _captured, server) = one_shot_server("503 Service Unavailable").await;
        let config = VaeliiConfig {
            server_url: Some(server_url),
            actor_term: Some("Syne".to_owned()),
            ..VaeliiConfig::default()
        };

        let error = write_get_message_receipt(&config, 42, "ignored")
            .await
            .unwrap_err();
        server.await.unwrap();
        assert!(matches!(
            error,
            VaeliiError::Http(StatusCode::SERVICE_UNAVAILABLE)
        ));
        assert_eq!(
            error.to_string(),
            "Vaelii receipt write returned HTTP 503 Service Unavailable"
        );
    }
}
