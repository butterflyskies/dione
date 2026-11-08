//! Private SDK fixture helpers retained from the original read tests.
use crate::QueryCtx;
use serenity::model::channel::MessageFlags;
const MESSAGE_FLAG_NOT_IN_SDK: u64 = 1 << 14;
use dione_config::{Config, LoadedConfig};
use dione_discord_state::new_state;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
pub(crate) fn test_config() -> LoadedConfig {
    let mut raw = Config::default();
    raw.delivery.evidence_markers_enabled = true;
    LoadedConfig::try_from_raw(raw).expect("valid test configuration")
}

pub(crate) fn messaging_ctx(config: LoadedConfig) -> QueryCtx {
    messaging_ctx_with_http(config, Arc::new(serenity::http::Http::new("fake")))
}

pub(crate) fn messaging_ctx_with_http(
    config: LoadedConfig,
    http: Arc<serenity::http::Http>,
) -> QueryCtx {
    let construct_id = config.pre_send_construct_id.clone();
    QueryCtx::new(
        http,
        new_state(),
        Arc::new(config),
        "/tmp".into(),
        construct_id,
    )
}

pub(crate) async fn fake_discord_http() -> (
    Arc<serenity::http::Http>,
    Arc<std::sync::Mutex<Vec<(String, String)>>>,
    tokio::task::JoinHandle<()>,
) {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind fake Discord API");
    let address = listener.local_addr().expect("fake Discord API address");
    let requests = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured = Arc::clone(&requests);
    let server = tokio::spawn(async move {
        // Each posted message gets its own id (9001, 9002, ...), so a
        // test can tell one chunk's id from another's.
        let mut next_post_id = 9001u64;
        let mut rejected_known_once = false;
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0u8; 4096];
            loop {
                let Ok(read) = stream.read(&mut buffer).await else {
                    return;
                };
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

            let request_text = String::from_utf8_lossy(&request);
            let request_line = request_text.lines().next().unwrap_or_default();
            let path = request_line.split_whitespace().nth(1).unwrap_or_default();
            let body = request_text
                .split_once("\r\n\r\n")
                .map_or("", |(_, body)| body);
            captured
                .lock()
                .expect("request capture lock")
                .push((path.to_owned(), body.to_owned()));

            let (status, response_body) = if path.ends_with("/users/@me/channels")
                && serde_json::from_str::<Value>(body).unwrap()["recipient_id"] == "78"
            {
                (
                    "403 Forbidden",
                    json!({"code": 50007, "message": "Cannot send messages to this user"})
                        .to_string(),
                )
            } else if path.ends_with("/users/@me/channels") {
                (
                    "200 OK",
                    json!({
                        "id": "4242",
                        "last_message_id": null,
                        "last_pin_timestamp": null,
                        "type": 1,
                        "recipients": [{
                            "id": "77",
                            "username": "recipient",
                            "global_name": null,
                            "avatar": null,
                            "discriminator": "0",
                            "public_flags": 0,
                            "bot": false
                        }]
                    })
                    .to_string(),
                )
            } else if path.ends_with("/typing") {
                ("204 No Content", String::new())
            } else if request_line.starts_with("GET ")
                    // 7001 is a source message whose id no posted reply
                    // shares (posted ids start at 9001).
                    && [7001, 9001, 9004, 9006, 9007, 9008]
                        .iter()
                        .any(|id| path.ends_with(&format!("/channels/42/messages/{id}")))
            {
                let id: u64 = path.rsplit('/').next().unwrap().parse().unwrap();
                let mut message = wire_message(
                    id,
                    "explicit read",
                    "2026-08-15T09:00:00.000000+00:00",
                    json!([]),
                );
                message["channel_id"] = json!("42");
                if id != 9008 {
                    message["guild_id"] = json!("500");
                }
                if id == 9004 {
                    message["author"]["bot"] = json!(true);
                }
                if id == 9007 {
                    message["message_reference"] =
                        json!({"message_id":"9006", "channel_id":"42", "guild_id":"500"});
                }
                ("200 OK", message.to_string())
            } else if request_line.starts_with("GET ") && path.ends_with("/channels/42") {
                (
                    "200 OK",
                    json!({"id":"42", "type":0, "guild_id":"500",
                        "position":0, "permission_overwrites":[], "name":"fixture",
                        "nsfw":false, "parent_id":null, "topic":null,
                        "last_message_id":null})
                    .to_string(),
                )
            } else if request_line.starts_with("GET ") && path.ends_with("/channels/42/messages/8")
            {
                (
                    "404 Not Found",
                    json!({ "message": "Unknown Message", "code": 10008 }).to_string(),
                )
            } else if request_line.starts_with("GET ")
                && path.ends_with("/channels/42/messages/9003")
            {
                (
                    "403 Forbidden",
                    json!({ "message": "Missing Access", "code": 50001 }).to_string(),
                )
            } else if request_line.starts_with("GET ") && path.contains("/messages/") {
                ("200 OK", {
                    let mut message = wire_message(
                        9001,
                        "explicit read",
                        "2026-08-15T09:00:00.000000+00:00",
                        json!([]),
                    );
                    if path.ends_with("/messages/9015") {
                        message["flags"] = json!(
                            (MessageFlags::SUPPRESS_NOTIFICATIONS | MessageFlags::SUPPRESS_EMBEDS)
                                .bits()
                                | MESSAGE_FLAG_NOT_IN_SDK
                        );
                    }
                    if path.ends_with("/messages/9016") {
                        message["flags"] = json!("invalid");
                    }
                    message.to_string()
                })
            } else if request_line.starts_with("GET ") && path.contains("/messages?") {
                let mut message = wire_message(
                    9002,
                    "incidental fetch",
                    "2026-08-15T09:00:01.000000+00:00",
                    json!([]),
                );
                message["channel_id"] = json!("42");
                ("200 OK", json!([message]).to_string())
            } else if request_line.starts_with("PUT ") && path.contains("/reactions/") {
                ("204 No Content", String::new())
            } else if request_line.starts_with("POST ")
                && path.ends_with("/messages")
                && body.contains("known-no-effect")
                && !rejected_known_once
            {
                rejected_known_once = true;
                (
                    "403 Forbidden",
                    json!({"message":"Missing Permissions","code":50013}).to_string(),
                )
            } else if request_line.starts_with("POST ")
                && path.ends_with("/messages")
                && body.contains("force-pending")
            {
                std::future::pending::<(&str, String)>().await
            } else if request_line.starts_with("POST ")
                && path.ends_with("/messages")
                && body.contains("force-uncertain")
            {
                ("200 OK", "{\"id\":\"9999\"}".to_owned())
            } else if request_line.starts_with("POST ")
                && path.ends_with("/messages")
                && body.contains("force-delivery-failure")
            {
                (
                    "403 Forbidden",
                    json!({ "message": "Missing Permissions", "code": 50013 }).to_string(),
                )
            } else if request_line.starts_with("PATCH ") && path.contains("/messages/") {
                let body: Value = serde_json::from_str(body).expect("edit body");
                (
                    "200 OK",
                    wire_message(
                        9001,
                        body["content"].as_str().unwrap_or_default(),
                        "2026-08-15T09:00:00.000000+00:00",
                        json!([]),
                    )
                    .to_string(),
                )
            } else if request_line.starts_with("POST ") && path.ends_with("/messages") {
                let content = serde_json::from_str::<Value>(body)
                    .ok()
                    .and_then(|body| body["content"].as_str().map(str::to_owned))
                    .unwrap_or_default();
                let id = next_post_id;
                next_post_id += 1;
                (
                    "200 OK",
                    wire_message(id, &content, "2026-08-15T09:00:00.000000+00:00", json!([]))
                        .to_string(),
                )
            } else {
                ("404 Not Found", "{}".to_owned())
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response_body}",
                response_body.len()
            );
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write fake Discord response");
        }
    });
    let http = serenity::http::HttpBuilder::new("fake")
        .proxy(format!("http://{address}"))
        .ratelimiter_disabled(true)
        .build();
    (Arc::new(http), requests, server)
}

pub(crate) fn wire_message(id: u64, content: &str, timestamp: &str, attachments: Value) -> Value {
    json!({
        "id": id.to_string(),
        "type": 0,
        "channel_id": "1080000000000000001",
        "author": {
            "id": "210987654321098765",
            "username": "example-user",
            "global_name": "Example User",
            "avatar": null,
            "discriminator": "0",
            "public_flags": 0,
            "bot": false
        },
        "content": content,
        "timestamp": timestamp,
        "edited_timestamp": null,
        "tts": false,
        "mention_everyone": false,
        "mentions": [],
        "mention_roles": [],
        "attachments": attachments,
        "embeds": [],
        "pinned": false,
        "flags": 0,
        "components": []
    })
}
