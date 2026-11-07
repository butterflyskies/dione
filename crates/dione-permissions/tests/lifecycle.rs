use camino::Utf8PathBuf;
use dione_config::{Config, ConfigRuntime, LoadedConfig};
use dione_discord_events::NotificationEvent;
use dione_discord_state::{PendingPermission, new_state};
use dione_permissions::{PermissionError, handle_interaction, send_permission_request};
use serde_json::{Value, json};
use serenity::{
    http::{Http, HttpBuilder},
    model::application::Interaction,
};
use std::sync::Arc;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::{Mutex, mpsc},
    task::JoinHandle,
};

#[derive(Clone)]
struct Request {
    method: String,
    path: String,
    body: Value,
    pending_ids: Vec<u64>,
    prompt_ids_sent: bool,
    queued_decisions: usize,
}
struct Fixture {
    http: Http,
    requests: Arc<Mutex<Vec<Request>>>,
    server: JoinHandle<()>,
    event_sender: Arc<Mutex<Option<mpsc::Sender<NotificationEvent>>>>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}
fn user(id: u64) -> Value {
    json!({"id":id.to_string(),"username":"admin","discriminator":"0","avatar":null,"bot":false})
}
fn message(id: u64, channel: u64) -> Value {
    json!({"id":id.to_string(),"channel_id":channel.to_string(),"author":user(999),
    "content":"permission prompt","timestamp":"2026-11-06T00:00:00Z","edited_timestamp":null,
    "tts":false,"mention_everyone":false,"mentions":[],"mention_roles":[],"attachments":[],
    "embeds":[],"pinned":false,"type":0,"components":[]})
}
async fn fixture(state: dione_discord_state::State) -> Fixture {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(Mutex::new(Vec::new()));
    let captured = requests.clone();
    let event_sender: Arc<Mutex<Option<mpsc::Sender<NotificationEvent>>>> =
        Arc::new(Mutex::new(None));
    let event_observer = event_sender.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut bytes = Vec::new();
            let mut buffer = [0; 4096];
            let end = loop {
                let read = stream.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                bytes.extend_from_slice(&buffer[..read]);
                if let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
            let length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while bytes.len() < end + length {
                let read = stream.read(&mut buffer).await.unwrap();
                assert_ne!(read, 0);
                bytes.extend_from_slice(&buffer[..read]);
            }
            let mut line = headers.lines().next().unwrap().split_whitespace();
            let method = line.next().unwrap().to_owned();
            let path = line.next().unwrap().to_owned();
            let body = if length == 0 {
                Value::Null
            } else {
                serde_json::from_slice(&bytes[end..end + length]).unwrap()
            };
            let locked = state.read().await;
            let pending_ids = locked.pending_permissions.keys().copied().collect();
            let prompt_ids_sent = locked.is_own_send(5001) && locked.is_own_send(5004);
            drop(locked);
            let queued_decisions = event_observer
                .lock()
                .await
                .as_ref()
                .map_or(0, |sender| 8 - sender.capacity());
            captured.lock().await.push(Request {
                method: method.clone(),
                path: path.clone(),
                body: body.clone(),
                pending_ids,
                prompt_ids_sent,
                queued_decisions,
            });
            let (status, body) = if path.ends_with("/users/@me/channels") {
                let recipient = body["recipient_id"].as_str().unwrap();
                if recipient == "222" {
                    ("403 Forbidden", json!({"message":"denied","code":50001}))
                } else {
                    let id = 4000 + recipient.parse::<u64>().unwrap();
                    (
                        "200 OK",
                        json!({"id":id.to_string(),"type":1,"recipients":[user(recipient.parse().unwrap())]}),
                    )
                }
            } else if method == "POST" && path.ends_with("/messages") {
                if path.contains("/4333/") {
                    ("403 Forbidden", json!({"message":"denied","code":50001}))
                } else {
                    let (id, ch) = if path.contains("/4111/") {
                        (5001, 4111)
                    } else {
                        (5004, 4444)
                    };
                    ("200 OK", message(id, ch))
                }
            } else if path.contains("bad-ack") || method == "DELETE" && path.ends_with("/5001") {
                ("403 Forbidden", json!({"message":"denied","code":50001}))
            } else {
                ("204 No Content", Value::Null)
            };
            let body = if status.starts_with("204") {
                String::new()
            } else {
                body.to_string()
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
            stream.shutdown().await.unwrap();
        }
    });
    Fixture {
        http: HttpBuilder::new("test-token")
            .proxy(format!("http://{address}"))
            .ratelimiter_disabled(true)
            .build(),
        requests,
        server,
        event_sender,
    }
}
fn component(custom: &str, actor: u64, token: &str) -> Interaction {
    serde_json::from_value(json!({"id":"10000","application_id":"999","type":3,
    "data":{"custom_id":custom,"component_type":2},"channel_id":"4111","user":user(actor),
    "token":token,"version":1,"message":message(5001,4111),"locale":"en-US",
    "entitlements":[],"attachment_size_limit":10485760}))
    .unwrap()
}
fn config(admins: &[&str]) -> LoadedConfig {
    let mut raw = Config::default();
    raw.access.admins = admins.iter().map(|s| (*s).to_owned()).collect();
    LoadedConfig::try_from_raw(raw).unwrap()
}
async fn policy(path: &camino::Utf8Path, admin: u64) {
    tokio::fs::write(
        path.join("config.toml"),
        format!("[access]\nadmins = [\"{admin}\"]\n"),
    )
    .await
    .unwrap();
    let (loaded, error) = ConfigRuntime::new(path.to_owned()).reload().await;
    assert!(error.is_none());
    assert!(loaded.is_admin(admin));
}
async fn pending(state: &dione_discord_state::State, id: u64, request: &str) {
    state.write().await.pending_permissions.insert(
        id,
        PendingPermission {
            request_id: request.to_owned(),
            channel_id: serenity::model::id::ChannelId::new(4111),
            created_at: chrono::Utc::now(),
        },
    );
}
async fn decision(events: &mut mpsc::Receiver<NotificationEvent>, request: &str, granted: bool) {
    let NotificationEvent::PermissionResponse {
        request_id,
        granted: actual,
    } = events.try_recv().unwrap()
    else {
        panic!("permission event")
    };
    assert_eq!(request_id, request);
    assert_eq!(actual, granted);
    assert!(events.try_recv().is_err());
}

#[tokio::test]
async fn real_sender_current_admin_ack_cleanup_and_duplicate_lifecycle() {
    let state = new_state();
    let fixture = fixture(state.clone()).await;
    assert!(matches!(
        send_permission_request(
            &fixture.http,
            &config(&[]),
            &state,
            "r",
            "Bash",
            "run",
            "{}"
        )
        .await,
        Err(PermissionError::NoAdmins)
    ));
    assert!(fixture.requests.lock().await.is_empty());
    let before = chrono::Utc::now();
    send_permission_request(
        &fixture.http,
        &config(&["111", "222", "333", "444"]),
        &state,
        "request-one",
        "Bash",
        "run ls",
        "{\"command\":\"ls\"}",
    )
    .await
    .unwrap();
    {
        let seen = fixture.requests.lock().await;
        assert_eq!(seen.len(), 7);
        let recipients: std::collections::BTreeSet<_> = seen
            .iter()
            .filter_map(|r| r.body["recipient_id"].as_str())
            .collect();
        assert_eq!(
            recipients,
            std::collections::BTreeSet::from(["111", "222", "333", "444"])
        );
        let body = &seen
            .iter()
            .find(|request| request.path.ends_with("/channels/4111/messages"))
            .unwrap()
            .body;
        assert_eq!(
            body["content"],
            "**Permission request** — tool `Bash`\nDescription: run ls\nInput preview:\n```\n{\"command\":\"ls\"}\n```\nRequest ID: `request-one`"
        );
        for (index, label, style, custom) in [
            (0, "Allow", 3, "perm:allow:request-one"),
            (1, "Deny", 4, "perm:deny:request-one"),
        ] {
            let button = &body["components"][0]["components"][index];
            assert_eq!(button["label"], label);
            assert_eq!(button["style"], style);
            assert_eq!(button["custom_id"], custom);
        }
    }
    {
        let locked = state.read().await;
        assert_eq!(
            locked
                .pending_permissions
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [5001, 5004]
        );
        for id in [5001, 5004] {
            let p = &locked.pending_permissions[&id];
            assert_eq!(p.request_id, "request-one");
            assert!(p.created_at >= before);
            assert_eq!(p.channel_id.get(), if id == 5001 { 4111 } else { 4444 });
            assert!(locked.is_own_send(id));
        }
    }
    let dir = tempfile::TempDir::new().unwrap();
    let path = Utf8PathBuf::from_path_buf(dir.path().to_owned()).unwrap();
    policy(&path, 111).await;
    let (tx, mut events) = mpsc::channel(8);
    *fixture.event_sender.lock().await = Some(tx.clone());
    let ping: Interaction = serde_json::from_value(
        json!({"type":1,"id":"1","application_id":"999","token":"t","version":1}),
    )
    .unwrap();
    handle_interaction(&fixture.http, ping, &state, &path, &tx).await;
    handle_interaction(
        &fixture.http,
        component("other:allow:request-one", 111, "ignored"),
        &state,
        &path,
        &tx,
    )
    .await;
    assert_eq!(fixture.requests.lock().await.len(), 7);
    policy(&path, 222).await;
    handle_interaction(
        &fixture.http,
        component("perm:allow:request-one", 111, "unauthorized"),
        &state,
        &path,
        &tx,
    )
    .await;
    {
        let seen = fixture.requests.lock().await;
        assert_eq!(seen.len(), 8);
        assert_eq!(seen[7].body["type"], 4);
        assert_eq!(seen[7].body["data"]["content"], "Not authorized.");
        assert_eq!(seen[7].body["data"]["flags"], 64);
    }
    assert_eq!(state.read().await.pending_permissions.len(), 2);
    assert!(events.try_recv().is_err());
    handle_interaction(
        &fixture.http,
        component("perm:allow:request-one", 222, "bad-ack"),
        &state,
        &path,
        &tx,
    )
    .await;
    assert_eq!(fixture.requests.lock().await.len(), 9);
    assert_eq!(state.read().await.pending_permissions.len(), 2);
    assert!(events.try_recv().is_err());
    pending(&state, 6000, "unrelated").await;
    state.write().await.recent_sent_ids.clear();
    handle_interaction(
        &fixture.http,
        component("perm:allow:request-one", 222, "allow"),
        &state,
        &path,
        &tx,
    )
    .await;
    {
        let seen = fixture.requests.lock().await;
        assert_eq!(seen.len(), 12);
        assert!(seen[9].path.contains("/interactions/"));
        assert_eq!(
            seen[9].pending_ids,
            [5001, 5004, 6000],
            "ack must precede removal"
        );
        assert_eq!(seen[9].queued_decisions, 0);
        assert_eq!(seen[9].body["type"], 7);
        assert_eq!(
            seen[9].body["data"]["content"],
            "Permission request `request-one` — **Allowed** by <@222>"
        );
        assert_eq!(seen[9].body["data"]["components"], json!([]));
        let deletes: std::collections::BTreeSet<_> = seen[10..12]
            .iter()
            .map(|r| {
                assert_eq!(r.method, "DELETE");
                assert_eq!(r.pending_ids, [6000]);
                assert!(
                    r.prompt_ids_sent,
                    "deletion suppression must be ready before HTTP cleanup"
                );
                assert_eq!(
                    r.queued_decisions, 0,
                    "decision must follow all deletion attempts"
                );
                r.path.rsplit('/').next().unwrap()
            })
            .collect();
        assert_eq!(deletes, std::collections::BTreeSet::from(["5001", "5004"]));
    }
    {
        let locked = state.read().await;
        assert_eq!(
            locked
                .pending_permissions
                .keys()
                .copied()
                .collect::<Vec<_>>(),
            [6000]
        );
        assert!(locked.is_own_send(5001));
        assert!(locked.is_own_send(5004));
    }
    decision(&mut events, "request-one", true).await;
    handle_interaction(
        &fixture.http,
        component("perm:allow:request-one", 222, "duplicate"),
        &state,
        &path,
        &tx,
    )
    .await;
    assert_eq!(fixture.requests.lock().await.len(), 13);
    assert!(events.try_recv().is_err());
    pending(&state, 7000, "deny-one").await;
    handle_interaction(
        &fixture.http,
        component("perm:deny:deny-one", 222, "deny"),
        &state,
        &path,
        &tx,
    )
    .await;
    decision(&mut events, "deny-one", false).await;
    {
        let seen = fixture.requests.lock().await;
        assert_eq!(seen.len(), 15);
        assert_eq!(
            seen[13].body["data"]["content"],
            "Permission request `deny-one` — **Denied** by <@222>"
        );
    }
    pending(&state, 8000, "changed-policy").await;
    policy(&path, 111).await;
    handle_interaction(
        &fixture.http,
        component("perm:allow:changed-policy", 222, "removed-admin"),
        &state,
        &path,
        &tx,
    )
    .await;
    assert!(state.read().await.pending_permissions.contains_key(&8000));
    assert!(events.try_recv().is_err());
    handle_interaction(
        &fixture.http,
        component("perm:allow:changed-policy", 111, "current-admin"),
        &state,
        &path,
        &tx,
    )
    .await;
    decision(&mut events, "changed-policy", true).await;
    assert_eq!(
        state
            .read()
            .await
            .pending_permissions
            .keys()
            .copied()
            .collect::<Vec<_>>(),
        [6000]
    );
}
