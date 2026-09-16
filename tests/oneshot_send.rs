//! Integration test for the one-shot send (#426): a mock Discord REST
//! server answers `GET /users/@me` and `POST /channels/{id}/messages`, and
//! the library driver must bind identity, send exactly the chunks, and
//! produce the contract JSON — with no writes under the state dir.
//!
//! The mock-driven tests need the `oneshot-test-seam` cargo feature (the
//! only way to point the library or the binary at a mock); without it only
//! the config-path oracles and the seam-absence guard compile.
#![cfg_attr(not(feature = "oneshot-test-seam"), allow(unused_imports, dead_code))]

#[cfg(feature = "oneshot-test-seam")]
use dione::oneshot::run_with_api_base as run;
use dione::{
    config::{ChannelConfig, ChunkMode, Config, LoadedConfig},
    oneshot::{Outcome, Reason, SendRequest, TokenSource},
};
use serde_json::json;
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt as _, AsyncWriteExt as _},
    net::TcpListener,
    sync::Mutex,
    task::JoinHandle,
};

const BOT_ID: u64 = 1549533370038362205;
const CHANNEL: u64 = 1549533156150087812;

/// One captured request: method, path, body.
#[derive(Debug, Clone)]
struct Captured {
    method: String,
    path: String,
    body: String,
}

/// A minimal HTTP/1.1 mock that routes by method+path. `me_id` is the id
/// returned by `/users/@me`; `post_status` is the status for message posts.
async fn mock_discord(
    me_id: u64,
    post_status: u16,
) -> (String, Arc<Mutex<Vec<Captured>>>, JoinHandle<()>) {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        let mut next_message_id: u64 = 9000;
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0_u8; 4096];
            loop {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
                if bytes.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            let header_end = bytes
                .windows(4)
                .position(|window| window == b"\r\n\r\n")
                .map(|position| position + 4)
                .unwrap();
            let headers = String::from_utf8_lossy(&bytes[..header_end]).to_string();
            let content_length = headers.lines().find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().unwrap())
            });
            while content_length.is_some_and(|length| bytes.len() < header_end + length) {
                let read = stream.read(&mut buffer).await.unwrap();
                if read == 0 {
                    break;
                }
                bytes.extend_from_slice(&buffer[..read]);
            }
            let request_line = headers.lines().next().unwrap_or_default();
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default().to_string();
            let target = parts.next().unwrap_or_default().to_string();
            let path = target
                .split_once("://")
                .and_then(|(_, rest)| rest.find('/').map(|i| rest[i..].to_string()))
                .unwrap_or(target);
            let body = String::from_utf8_lossy(&bytes[header_end..]).to_string();
            captured.lock().await.push(Captured {
                method: method.clone(),
                path: path.clone(),
                body,
            });

            let (status, reason, body) = if method == "GET" && path.ends_with("/users/@me") {
                (
                    200,
                    "OK",
                    json!({
                        "id": me_id.to_string(),
                        "username": "trundle",
                        "discriminator": "0",
                        "avatar": null,
                        "bot": true,
                    })
                    .to_string(),
                )
            } else if method == "POST" && path.ends_with(&format!("/channels/{CHANNEL}/messages")) {
                if post_status == 200 {
                    next_message_id += 1;
                    (
                        200,
                        "OK",
                        json!({
                            "id": next_message_id.to_string(),
                            "channel_id": CHANNEL.to_string(),
                            "content": "",
                            "timestamp": "2026-09-15T00:00:00.000000+00:00",
                            "edited_timestamp": null,
                            "tts": false,
                            "mention_everyone": false,
                            "mentions": [],
                            "mention_roles": [],
                            "attachments": [],
                            "embeds": [],
                            "pinned": false,
                            "type": 0,
                            "author": {
                                "id": me_id.to_string(),
                                "username": "trundle",
                                "discriminator": "0",
                                "avatar": null,
                                "bot": true,
                            },
                        })
                        .to_string(),
                    )
                } else if post_status == 429 {
                    (
                        429,
                        "Too Many Requests",
                        json!({"message": "You are being rate limited.", "retry_after": 5.0, "global": false, "code": 0}).to_string(),
                    )
                } else {
                    (
                        post_status,
                        "Error",
                        json!({"message": "Missing Access", "code": 50001}).to_string(),
                    )
                }
            } else {
                (
                    404,
                    "Not Found",
                    json!({"message": "Unknown route", "code": 0}).to_string(),
                )
            };
            let extra = if status == 429 {
                "retry-after: 5\r\nx-ratelimit-remaining: 0\r\nx-ratelimit-reset-after: 5\r\n"
            } else {
                ""
            };
            let response = format!(
                "HTTP/1.1 {status} {reason}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n{extra}connection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    (format!("http://{address}"), requests, server)
}

fn config(mutate: impl FnOnce(&mut Config)) -> LoadedConfig {
    let mut raw = Config {
        token: Some("config-token".into()),
        ..Default::default()
    };
    raw.channels.push(ChannelConfig {
        id: CHANNEL.to_string(),
        ..Default::default()
    });
    mutate(&mut raw);
    LoadedConfig::try_from_raw(raw).expect("test configuration generation")
}

fn request(message: &str) -> SendRequest {
    SendRequest {
        channel_id: CHANNEL,
        expect_identity: BOT_ID,
        message: message.into(),
        token_source: TokenSource::Config,
        allow_multi_chunk: false,
        dry_run: false,
        nonce: None,
    }
}

/// Name + length + mtime of every entry under `dir`, recursively.
fn dir_snapshot(dir: &std::path::Path) -> Vec<(String, u64, std::time::SystemTime)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let meta = entry.metadata().unwrap();
        out.push((
            entry.path().display().to_string(),
            meta.len(),
            meta.modified().unwrap(),
        ));
        if meta.is_dir() {
            out.extend(dir_snapshot(&entry.path()));
        }
    }
    out.sort();
    out
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn sends_one_chunk_and_reports_contract_json() {
    let (proxy, requests, server) = mock_discord(BOT_ID, 200).await;
    // Load through the binary's real seam from an on-disk config so the
    // zero-write assertion covers config loading too.
    let state_dir = tempfile::TempDir::new().unwrap();
    let config_path =
        camino::Utf8PathBuf::from_path_buf(state_dir.path().join("config.toml")).unwrap();
    std::fs::write(
        &config_path,
        format!("token = \"config-token\"\n\n[[channels]]\nid = \"{CHANNEL}\"\n"),
    )
    .unwrap();
    let before = dir_snapshot(state_dir.path());
    assert_eq!(before.len(), 1);
    let cfg = dione::oneshot::load_config_for_send(&config_path).expect("config loads");

    let outcome = run(&cfg, &request("drumbeat"), &proxy).await;
    server.abort();

    let json = outcome.to_json();
    assert_eq!(json["ok"], json!(true), "{json}");
    assert_eq!(json["status"], json!("sent"));
    assert_eq!(json["retryable"], json!(false));
    assert_eq!(json["delivery_ambiguous"], json!(false));
    assert_eq!(json["channel_id"], json!(CHANNEL.to_string()));
    assert_eq!(json["message_ids"], json!(["9001"]));
    assert_eq!(json["identity"]["bot_user_id"], json!(BOT_ID.to_string()));
    assert_eq!(json["identity"]["username"], json!("trundle"));
    assert_eq!(json["identity"]["token_source"], json!("config"));
    assert_eq!(outcome.exit_code(), 0);

    let requests = requests.lock().await.clone();
    assert_eq!(requests.len(), 2, "{requests:#?}");
    assert_eq!(requests[0].method, "GET");
    assert!(
        requests[0].path.ends_with("/users/@me"),
        "{}",
        requests[0].path
    );
    assert_eq!(requests[1].method, "POST");
    assert!(
        requests[1]
            .path
            .ends_with(&format!("/channels/{CHANNEL}/messages")),
        "{}",
        requests[1].path
    );
    let body: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
    assert_eq!(body["content"], json!("drumbeat"));

    // Nothing under the state dir was created, replaced, or touched: no
    // default template, no .lkg, no quarantine, no diary, no journal.
    assert_eq!(dir_snapshot(state_dir.path()), before);
}

#[test]
fn malformed_config_refuses_without_echoing_token_bytes() {
    let state_dir = tempfile::TempDir::new().unwrap();
    let config_path =
        camino::Utf8PathBuf::from_path_buf(state_dir.path().join("config.toml")).unwrap();
    std::fs::write(&config_path, "token = \"SECRETTOKENBYTES\n").unwrap();
    let before = dir_snapshot(state_dir.path());
    let refusal = dione::oneshot::load_config_for_send(&config_path).unwrap_err();
    let stdout = Outcome::Refused(refusal).to_json().to_string();
    assert!(!stdout.contains("SECRETTOKENBYTES"), "{stdout}");
    assert!(stdout.contains("\"reason\":\"config_invalid\""), "{stdout}");
    assert!(stdout.contains("\"retryable\":false"), "{stdout}");
    assert_eq!(dir_snapshot(state_dir.path()), before);
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn identity_mismatch_refuses_before_any_post() {
    let (proxy, requests, server) = mock_discord(BOT_ID + 1, 200).await;
    let outcome = run(&config(|_| {}), &request("drumbeat"), &proxy).await;
    server.abort();

    let Outcome::Refused(refusal) = &outcome else {
        panic!("expected refusal, got {outcome:?}");
    };
    assert_eq!(refusal.reason, Reason::IdentityMismatch);
    assert!(!refusal.retryable);
    assert_eq!(outcome.exit_code(), 2);
    let requests = requests.lock().await.clone();
    assert!(requests.iter().all(|r| r.method == "GET"), "{requests:#?}");
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn dry_run_stops_after_preflight() {
    let (proxy, requests, server) = mock_discord(BOT_ID, 200).await;
    let mut req = request("one\n\ntwo");
    req.dry_run = true;
    req.allow_multi_chunk = true;
    let cfg = config(|raw| {
        raw.delivery.text_chunk_limit = 4;
        raw.delivery.chunk_mode = ChunkMode::Paragraph;
    });
    let outcome = run(&cfg, &req, &proxy).await;
    server.abort();

    let json = outcome.to_json();
    assert_eq!(json["status"], json!("preflight_ok"), "{json}");
    assert_eq!(json["chunks"], json!(2));
    assert_eq!(json["retryable"], json!(false));
    assert_eq!(json["identity"]["bot_user_id"], json!(BOT_ID.to_string()));
    assert_eq!(outcome.exit_code(), 0);
    let requests = requests.lock().await.clone();
    assert!(requests.iter().all(|r| r.method == "GET"), "{requests:#?}");
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn multi_chunk_sends_every_chunk_in_order() {
    let (proxy, requests, server) = mock_discord(BOT_ID, 200).await;
    let mut req = request("one\n\ntwo");
    req.allow_multi_chunk = true;
    let cfg = config(|raw| raw.delivery.text_chunk_limit = 4);
    let outcome = run(&cfg, &req, &proxy).await;
    server.abort();

    let json = outcome.to_json();
    assert_eq!(json["status"], json!("sent"), "{json}");
    assert_eq!(json["message_ids"], json!(["9001", "9002"]));
    let requests = requests.lock().await.clone();
    let bodies: Vec<String> = requests
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| {
            serde_json::from_str::<serde_json::Value>(&r.body).unwrap()["content"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect();
    // Paragraph mode keeps the separator on the first chunk, exactly as
    // `deliver_reply` would send it.
    assert_eq!(bodies, vec!["one\n", "two"]);
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn discord_403_is_send_failed_not_retryable() {
    let (proxy, _requests, server) = mock_discord(BOT_ID, 403).await;
    let outcome = run(&config(|_| {}), &request("drumbeat"), &proxy).await;
    server.abort();

    let Outcome::Refused(refusal) = &outcome else {
        panic!("expected refusal, got {outcome:?}");
    };
    assert_eq!(refusal.reason, Reason::SendFailed);
    assert!(!refusal.retryable);
    assert!(refusal.detail.contains("HTTP 403"), "{}", refusal.detail);
    assert!(
        refusal.detail.contains("Missing Access"),
        "{}",
        refusal.detail
    );
    assert!(!refusal.detail.contains("config-token"));
    assert_eq!(outcome.exit_code(), 3);
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn discord_503_without_nonce_is_ambiguous_not_retryable() {
    let (proxy, _requests, server) = mock_discord(BOT_ID, 503).await;
    let outcome = run(&config(|_| {}), &request("drumbeat"), &proxy).await;
    server.abort();

    let Outcome::Refused(refusal) = &outcome else {
        panic!("expected refusal, got {outcome:?}");
    };
    assert_eq!(refusal.reason, Reason::SendFailed);
    assert!(!refusal.retryable, "{}", refusal.detail);
    assert!(refusal.delivery_ambiguous);
    assert!(refusal.detail.contains("HTTP 503"), "{}", refusal.detail);
    assert!(
        refusal
            .detail
            .contains("delivery ambiguous; reconcile before resending"),
        "{}",
        refusal.detail
    );
    let json = outcome.to_json();
    assert_eq!(json["retryable"], json!(false));
    assert_eq!(json["delivery_ambiguous"], json!(true));
    assert_eq!(outcome.exit_code(), 3);
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn discord_503_with_nonce_is_still_ambiguous_not_retryable() {
    let (proxy, _requests, server) = mock_discord(BOT_ID, 503).await;
    let mut req = request("drumbeat");
    req.nonce = Some("trundle-beat-1".into());
    let outcome = run(&config(|_| {}), &req, &proxy).await;
    server.abort();

    let Outcome::Refused(refusal) = &outcome else {
        panic!("expected refusal, got {outcome:?}");
    };
    assert_eq!(refusal.reason, Reason::SendFailed);
    assert!(!refusal.retryable);
    assert!(refusal.delivery_ambiguous);
    assert_eq!(outcome.to_json()["retryable"], json!(false));
    assert_eq!(outcome.to_json()["delivery_ambiguous"], json!(true));
    assert_eq!(outcome.exit_code(), 3);
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn discord_429_surfaces_promptly_as_retryable_with_one_post() {
    let (proxy, requests, server) = mock_discord(BOT_ID, 429).await;
    let started = std::time::Instant::now();
    // Generous for slow CI runners; serenity's ratelimiter would sleep for
    // the mock's retry-after (5 s) and then retry, so 20 s still catches it.
    let outcome = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        run(&config(|_| {}), &request("drumbeat"), &proxy),
    )
    .await
    .expect("one-shot must not sleep on retry-after");
    server.abort();
    assert!(started.elapsed() < std::time::Duration::from_secs(20));

    let Outcome::Refused(refusal) = &outcome else {
        panic!("expected refusal, got {outcome:?}");
    };
    assert_eq!(refusal.reason, Reason::SendFailed);
    assert!(refusal.retryable, "{}", refusal.detail);
    assert!(refusal.detail.contains("HTTP 429"), "{}", refusal.detail);
    assert!(!refusal.delivery_ambiguous);
    assert_eq!(outcome.to_json()["retryable"], json!(true));
    assert_eq!(outcome.to_json()["delivery_ambiguous"], json!(false));
    assert_eq!(outcome.exit_code(), 3);
    let posts = requests
        .lock()
        .await
        .iter()
        .filter(|r| r.method == "POST")
        .count();
    assert_eq!(posts, 1, "no internal retry");
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn nonce_is_sent_with_enforce_nonce() {
    let (proxy, requests, server) = mock_discord(BOT_ID, 200).await;
    let mut req = request("one\n\ntwo");
    req.allow_multi_chunk = true;
    req.nonce = Some("beat-7".into());
    let cfg = config(|raw| raw.delivery.text_chunk_limit = 4);
    let outcome = run(&cfg, &req, &proxy).await;
    server.abort();
    assert_eq!(outcome.to_json()["status"], json!("sent"));
    let bodies: Vec<serde_json::Value> = requests
        .lock()
        .await
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect();
    assert_eq!(bodies.len(), 2);
    assert_eq!(bodies[0]["nonce"], json!("beat-7-0"));
    assert_eq!(bodies[1]["nonce"], json!("beat-7-1"));
    assert_eq!(bodies[0]["enforce_nonce"], json!(true));
    assert_eq!(bodies[1]["enforce_nonce"], json!(true));
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn dry_run_rejects_chunk_limit_above_discord_max() {
    let (proxy, requests, server) = mock_discord(BOT_ID, 200).await;
    let mut req = request(&"x".repeat(2500));
    req.dry_run = true;
    let cfg = config(|raw| raw.delivery.text_chunk_limit = 4000);
    let outcome = run(&cfg, &req, &proxy).await;
    server.abort();
    let Outcome::Refused(refusal) = &outcome else {
        panic!("expected refusal, got {outcome:?}");
    };
    assert_eq!(refusal.reason, Reason::ConfigInvalid);
    assert_eq!(outcome.exit_code(), 1);
    assert!(requests.lock().await.iter().all(|r| r.method == "GET"));
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn unreachable_discord_is_send_failed_retryable() {
    // Bind then drop: nothing listens, so the connect fails fast.
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let outcome = run(
        &config(|_| {}),
        &request("drumbeat"),
        &format!("http://{address}"),
    )
    .await;
    let Outcome::Refused(refusal) = &outcome else {
        panic!("expected refusal, got {outcome:?}");
    };
    assert_eq!(refusal.reason, Reason::SendFailed);
    assert!(refusal.retryable, "{}", refusal.detail);
    assert!(
        refusal.detail.contains("identity lookup"),
        "{}",
        refusal.detail
    );
    assert!(!refusal.detail.contains("config-token"));
}

#[cfg(feature = "oneshot-test-seam")]
#[tokio::test]
async fn unlisted_channel_is_refused_after_identity() {
    let (proxy, requests, server) = mock_discord(BOT_ID, 200).await;
    let mut req = request("drumbeat");
    req.channel_id = CHANNEL + 1;
    let outcome = run(&config(|_| {}), &req, &proxy).await;
    server.abort();
    let Outcome::Refused(refusal) = &outcome else {
        panic!("expected refusal, got {outcome:?}");
    };
    assert_eq!(refusal.reason, Reason::NotPermittedTarget);
    assert!(requests.lock().await.iter().all(|r| r.method == "GET"));
}

/// A `dione-send` child with a clean environment: no inherited HOME/XDG,
/// DIONE_*, DISCORD_*, or RUST_LOG; the state dir and log level are set
/// explicitly. `Command::output()` drains both pipes concurrently, so a
/// large trace stderr cannot deadlock the test.
fn dione_send_command(state_dir: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_dione-send"));
    command.env_clear();
    if let Some(path) = std::env::var_os("PATH") {
        command.env("PATH", path);
    }
    command
        .env("RUST_LOG", "trace")
        .env("DIONE_STATE_DIR", state_dir)
        .env("HOME", state_dir)
        .env("TMPDIR", state_dir);
    command
}

/// Real-binary oracle: a malformed config carrying the token must not leak
/// the token on stdout OR stderr, at the most verbose log level.
#[test]
fn binary_never_leaks_token_bytes_on_either_stream() {
    let state_dir = tempfile::TempDir::new().unwrap();
    let config_path = state_dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        "token = \"SECRETTOKENBYTES\nthis line is = = broken\n",
    )
    .unwrap();
    let before = dir_snapshot(state_dir.path());

    let output = dione_send_command(state_dir.path())
        .args([
            "--channel",
            "1",
            "--expect-identity",
            "2",
            "--message",
            "x",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .env("DISCORD_BOT_TOKEN", "AMBIENTTOKENBYTES")
        .output()
        .expect("dione-send runs");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    let mut lines = stdout.lines();
    let json: serde_json::Value = serde_json::from_str(lines.next().expect("one JSON line"))
        .unwrap_or_else(|e| panic!("stdout must be one JSON object: {e}\n{stdout}"));
    assert!(lines.next().is_none(), "exactly one stdout line: {stdout}");
    assert_eq!(json["ok"], json!(false));
    assert_eq!(json["status"], json!("refused"));
    assert_eq!(json["reason"], json!("config_invalid"));
    assert_eq!(json["retryable"], json!(false));
    assert_eq!(json["delivery_ambiguous"], json!(false));
    for sentinel in ["SECRETTOKENBYTES", "AMBIENTTOKENBYTES", "broken"] {
        assert!(
            !stdout.contains(sentinel),
            "stdout leaks {sentinel}: {stdout}"
        );
        assert!(
            !stderr.contains(sentinel),
            "stderr leaks {sentinel}: {stderr}"
        );
    }
    assert_eq!(
        dir_snapshot(state_dir.path()),
        before,
        "binary wrote into the state dir"
    );
}

/// Real-binary oracle for a VALID config: every warning-producing field
/// carries a sentinel, and at RUST_LOG=trace none of them may reach either
/// stream. The token is omitted so the run stops at `no_token` (exit 2)
/// after the whole compose path has run — no network needed.
#[test]
fn binary_never_logs_config_values_from_a_valid_config() {
    let state_dir = tempfile::TempDir::new().unwrap();
    let config_path = state_dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        r#"
timezone = "SENTINEL_TZ_VALUE_8f31"

[access]
ignore_from = ["SENTINEL_IGNORE_VALUE_8f31"]
trusted_webhook_creators = ["SENTINEL_CREATOR_VALUE_8f31", "0"]

[[channels]]
id = "1"
allow_pk_systems = ["SENTINEL_PK_VALUE_8f31"]

[rate_limit]
overflow = "SENTINEL_CONFIG_VALUE_8f31"

[pre_send]
enabled = true
construct_id = "SENTINEL CONSTRUCT VALUE 8f31"

[contradictionary]
enabled = true
sidecar_path = "sidecar.toml"
"#,
    )
    .unwrap();
    std::fs::write(
        state_dir.path().join("sidecar.toml"),
        "[[entry]]\npattern = \"SENTINEL_PATTERN_VALUE_8f31\"\naction = \"warn\"\n",
    )
    .unwrap();
    let before = dir_snapshot(state_dir.path());

    let output = dione_send_command(state_dir.path())
        .args([
            "--channel",
            "1",
            "--expect-identity",
            "2",
            "--message",
            "x",
            "--dry-run",
            "--config",
            config_path.to_str().unwrap(),
        ])
        .output()
        .expect("dione-send runs");

    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let json: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|e| panic!("stdout must be one JSON object: {e}\n{stdout}\n{stderr}"));
    assert_eq!(json["reason"], json!("no_token"), "{stdout}\n{stderr}");
    assert_eq!(output.status.code(), Some(2));
    for sentinel in [
        "SENTINEL_TZ_VALUE_8f31",
        "SENTINEL_IGNORE_VALUE_8f31",
        "SENTINEL_CREATOR_VALUE_8f31",
        "SENTINEL_PK_VALUE_8f31",
        "SENTINEL_CONFIG_VALUE_8f31",
        "SENTINEL CONSTRUCT VALUE 8f31",
        "SENTINEL_PATTERN_VALUE_8f31",
        "8f31",
    ] {
        assert!(
            !stdout.contains(sentinel),
            "stdout leaks {sentinel}: {stdout}"
        );
        assert!(
            !stderr.contains(sentinel),
            "stderr leaks {sentinel}: {stderr}"
        );
    }
    assert_eq!(dir_snapshot(state_dir.path()), before);
}

/// Real-binary SUCCESS oracle at RUST_LOG=trace: the message text must not
/// appear on stderr (serenity's request spans would render the POST body
/// if third-party tracing were not clamped) nor on stdout (ids only).
#[cfg(feature = "oneshot-test-seam")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn binary_success_never_logs_message_text() {
    let (proxy, requests, server) = mock_discord(BOT_ID, 200).await;
    let state_dir = tempfile::TempDir::new().unwrap();
    let config_path = state_dir.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!("token = \"config-token\"\n\n[[channels]]\nid = \"{CHANNEL}\"\n"),
    )
    .unwrap();
    let before = dir_snapshot(state_dir.path());

    let dir = state_dir.path().to_path_buf();
    let config_arg = config_path.to_str().unwrap().to_string();
    let output = tokio::task::spawn_blocking(move || {
        dione_send_command(&dir)
            .args([
                "--channel",
                &CHANNEL.to_string(),
                "--expect-identity",
                &BOT_ID.to_string(),
                "--message",
                "hello MESSAGE_SENTINEL_5c2a",
                "--nonce",
                "NONCE_SENTINEL_5c2a",
                "--config",
                &config_arg,
                "--discord-api-base",
                &proxy,
            ])
            .output()
            .expect("dione-send runs")
    })
    .await
    .unwrap();
    server.abort();

    let stdout = String::from_utf8(output.stdout).unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert_eq!(
        output.status.code(),
        Some(0),
        "stdout={stdout}\nstderr={stderr}"
    );
    let mut lines = stdout.lines();
    let json: serde_json::Value = serde_json::from_str(lines.next().expect("one JSON line"))
        .unwrap_or_else(|e| panic!("stdout must be one JSON object: {e}\n{stdout}"));
    assert!(lines.next().is_none(), "exactly one stdout line: {stdout}");
    assert_eq!(json["status"], json!("sent"));
    assert_eq!(json["message_ids"], json!(["9001"]));
    assert_eq!(json["identity"]["bot_user_id"], json!(BOT_ID.to_string()));
    for sentinel in [
        "MESSAGE_SENTINEL_5c2a",
        "NONCE_SENTINEL_5c2a",
        "config-token",
    ] {
        assert!(
            !stdout.contains(sentinel),
            "stdout leaks {sentinel}: {stdout}"
        );
        assert!(
            !stderr.contains(sentinel),
            "stderr leaks {sentinel}: {stderr}"
        );
    }
    let posts: Vec<serde_json::Value> = requests
        .lock()
        .await
        .iter()
        .filter(|r| r.method == "POST")
        .map(|r| serde_json::from_str(&r.body).unwrap())
        .collect();
    assert_eq!(posts.len(), 1);
    assert_eq!(posts[0]["content"], json!("hello MESSAGE_SENTINEL_5c2a"));
    assert_eq!(posts[0]["nonce"], json!("NONCE_SENTINEL_5c2a"));
    assert_eq!(dir_snapshot(state_dir.path()), before);
}

/// Guard, compiled only WITHOUT the seam: the production binary must not
/// accept an endpoint override at all — the flag is an unknown argument,
/// refused as `usage` before any config or network work.
#[cfg(not(feature = "oneshot-test-seam"))]
#[test]
fn production_binary_has_no_endpoint_override() {
    let state_dir = tempfile::TempDir::new().unwrap();
    let output = dione_send_command(state_dir.path())
        .args([
            "--channel",
            "1",
            "--expect-identity",
            "2",
            "--message",
            "x",
            "--discord-api-base",
            "http://127.0.0.1:1",
        ])
        .output()
        .expect("dione-send runs");
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    let json: serde_json::Value = serde_json::from_str(stdout.trim()).unwrap();
    assert_eq!(json["reason"], json!("usage"), "{stdout}");
    assert!(
        json["detail"]
            .as_str()
            .unwrap()
            .contains("--discord-api-base"),
        "{stdout}"
    );
}
