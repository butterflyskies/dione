use dione_coordination::{ClaimOutcome, CoordinationConfig, CoordinationEvent, Coordinator};
use serde_json::{Value, json};
use serenity::model::id::MessageId;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{TcpListener, TcpStream},
    sync::{broadcast, mpsc},
    task::AbortHandle,
};
const CHANNEL: &str = "1550955234304466985";
const BOT_A: &str = "111";
const BOT_B: &str = "222";
const MSG: &str = "1001";
const REPLY: &str = "2001";
type Push = mpsc::UnboundedSender<Value>;

struct Waiter {
    bot_id: String,
    conn_id: u64,
    push: Push,
}

struct Key {
    owner: String,
    /// In line order; `waiters[0]` is promoted next.
    waiters: Vec<Waiter>,
    lease: Option<AbortHandle>,
    /// Tombstone: the owner reported `done`; later claims are told who
    /// answered instead of proceeding.
    done: Option<String>,
}

struct FakeServer {
    lease_ms: u64,
    keys: Mutex<HashMap<String, Key>>,
}

impl FakeServer {
    fn arm_lease(self: &Arc<Self>, message_id: &str, key: &mut Key) {
        if let Some(lease) = key.lease.take() {
            lease.abort();
        }
        let server = Arc::clone(self);
        let message_id = message_id.to_owned();
        let lease_ms = self.lease_ms;
        let handle = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(lease_ms)).await;
            server.lease_expired(&message_id);
        });
        key.lease = Some(handle.abort_handle());
    }

    fn lease_expired(self: &Arc<Self>, message_id: &str) {
        let mut keys = self.keys.lock().unwrap();
        self.promote(&mut keys, message_id);
    }

    /// The owner left (release or lease expiry): promote the next waiter,
    /// or drop the key. A tombstone is never promoted.
    fn promote(self: &Arc<Self>, keys: &mut HashMap<String, Key>, message_id: &str) {
        let Some(key) = keys.get_mut(message_id) else {
            return;
        };
        if key.done.is_some() {
            return;
        }
        if key.waiters.is_empty() {
            if let Some(lease) = key.lease.take() {
                lease.abort();
            }
            keys.remove(message_id);
            return;
        }
        let next = key.waiters.remove(0);
        key.owner = next.bot_id;
        let _ = next
            .push
            .send(json!({ "event": "promoted", "message_id": message_id }));
        self.arm_lease(message_id, key);
    }

    fn claim(self: &Arc<Self>, waiter: Waiter, message_id: &str) -> Value {
        let mut keys = self.keys.lock().unwrap();
        let Some(key) = keys.get_mut(message_id) else {
            let mut key = Key {
                owner: waiter.bot_id,
                waiters: Vec::new(),
                lease: None,
                done: None,
            };
            self.arm_lease(message_id, &mut key);
            keys.insert(message_id.to_owned(), key);
            return json!({ "status": "proceed", "lease_ms": self.lease_ms });
        };
        if key.owner == waiter.bot_id {
            return json!({ "status": "proceed", "lease_ms": self.lease_ms });
        }
        if let Some(reply_id) = &key.done {
            let _ = waiter.push.send(json!({
                "event": "done",
                "message_id": message_id,
                "bot_id": key.owner,
                "reply_message_id": reply_id,
            }));
            return json!({
                "status": "wait",
                "ahead": [key.owner],
                "lease_ms": self.lease_ms,
                "done": true,
                "reply_message_id": reply_id,
            });
        }
        let mut ahead = vec![key.owner.clone()];
        ahead.extend(key.waiters.iter().map(|w| w.bot_id.clone()));
        key.waiters.push(waiter);
        json!({ "status": "wait", "ahead": ahead, "lease_ms": self.lease_ms })
    }

    fn done(&self, bot_id: &str, message_id: &str, reply_id: &str) -> Value {
        let mut keys = self.keys.lock().unwrap();
        let Some(key) = keys.get_mut(message_id).filter(|k| k.owner == bot_id) else {
            return json!({ "error": "not owner" });
        };
        if key.done.is_some() {
            return json!({ "ok": true });
        }
        key.done = Some(reply_id.to_owned());
        if let Some(lease) = key.lease.take() {
            lease.abort();
        }
        for waiter in key.waiters.drain(..) {
            let _ = waiter.push.send(json!({
                "event": "done",
                "message_id": message_id,
                "bot_id": bot_id,
                "reply_message_id": reply_id,
            }));
        }
        json!({ "ok": true })
    }

    fn release(self: &Arc<Self>, bot_id: &str, message_id: &str) -> Value {
        let mut keys = self.keys.lock().unwrap();
        match keys.get(message_id) {
            Some(key) if key.done.is_some() => json!({ "error": "already done" }),
            Some(key) if key.owner == bot_id => {
                self.promote(&mut keys, message_id);
                json!({ "ok": true })
            }
            _ => json!({ "error": "not owner" }),
        }
    }

    /// Connection closed: its waiters leave the line. An owner's
    /// connection is transient, so its claim stays.
    fn drop_conn(&self, conn_id: u64) {
        let mut keys = self.keys.lock().unwrap();
        for key in keys.values_mut() {
            key.waiters.retain(|w| w.conn_id != conn_id);
        }
    }

    async fn handle(self: Arc<Self>, stream: TcpStream, conn_id: u64) {
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        let (push, mut pushes) = mpsc::unbounded_channel::<Value>();
        let mut bot: Option<String> = None;
        loop {
            let reply = tokio::select! {
                biased;
                line = lines.next_line() => {
                    let Ok(Some(line)) = line else { break };
                    let Ok(m) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let s = |k: &str| m.get(k).and_then(Value::as_str).unwrap_or("").to_owned();
                    match (m.get("msg").and_then(Value::as_str), bot.clone()) {
                        (Some("hello"), _) => {
                            bot = Some(s("bot_id"));
                            json!({ "ok": true })
                        }
                        (_, None) => json!({ "error": "hello first" }),
                        (Some("claim"), Some(b)) => self.claim(
                            Waiter { bot_id: b, conn_id, push: push.clone() },
                            &s("message_id"),
                        ),
                        (Some("done"), Some(b)) => {
                            self.done(&b, &s("message_id"), &s("reply_message_id"))
                        }
                        (Some("release"), Some(b)) => self.release(&b, &s("message_id")),
                        (other, _) => json!({ "error": format!("unknown msg {other:?}") }),
                    }
                }
                Some(ev) = pushes.recv() => ev,
            };
            let mut out = reply.to_string();
            out.push('\n');
            if write.write_all(out.as_bytes()).await.is_err() {
                break;
            }
        }
        self.drop_conn(conn_id);
    }
}

/// Bind an ephemeral port, serve in the background, return the address.
async fn spawn_server(lease_ms: u64) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let server = Arc::new(FakeServer {
        lease_ms,
        keys: Mutex::new(HashMap::new()),
    });
    tokio::spawn(async move {
        let mut next_conn = 0u64;
        while let Ok((stream, _)) = listener.accept().await {
            next_conn += 1;
            tokio::spawn(Arc::clone(&server).handle(stream, next_conn));
        }
    });
    addr
}

fn config(addr: &str) -> CoordinationConfig {
    CoordinationConfig {
        addr: addr.to_owned(),
        lease_ms: 240_000,
        connect_timeout_ms: 2_000,
        fail_open: true,
    }
}

fn coordinator(addr: &str, bot_id: &str) -> Coordinator {
    Coordinator::new(config(addr), bot_id)
}

async fn recv_event(
    rx: &mut broadcast::Receiver<CoordinationEvent>,
    within: Duration,
) -> CoordinationEvent {
    tokio::time::timeout(within, rx.recv())
        .await
        .expect("event within deadline")
        .expect("broadcast open")
}

async fn spawn_scripted_server(script: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let script = script.clone();
            tokio::spawn(async move {
                let (read, mut write) = stream.into_split();
                let mut lines = BufReader::new(read).lines();
                if let Ok(Some(_hello)) = lines.next_line().await {
                    let _ = write.write_all(b"{\"ok\":true}\n").await;
                }
                if let Ok(Some(_request)) = lines.next_line().await {
                    for line in &script {
                        let _ = write.write_all(format!("{line}\n").as_bytes()).await;
                    }
                }
                while let Ok(Some(_)) = lines.next_line().await {}
            });
        }
    });
    addr
}

/// `main.rs` subscribes once, at startup; every reload (file watcher,
/// `reload_config`, `add_channel`, ...) builds a fresh `LoadedConfig` and
/// so fresh coordinators. A push that arrives for a claim made through the
/// reloaded coordinator must still reach the startup subscription.
#[tokio::test]
async fn events_reach_startup_subscriber_after_config_reload() {
    let addr = spawn_server(10_000).await;
    let raw = || {
        let mut raw = crate::config::Config::default();
        raw.pre_send.author_id = Some(serenity::model::id::UserId::new(222));
        raw.coordination
            .insert("claim-once".to_owned(), config(&addr));
        raw
    };
    let startup = crate::config::LoadedConfig::from_raw(raw());
    let mut events = startup.coordinators["claim-once"].subscribe();
    let reloaded = crate::config::LoadedConfig::from_raw(raw());

    let owner = coordinator(&addr, "111");
    let mid = "880001";
    assert!(matches!(
        owner.claim(CHANNEL, mid).await,
        ClaimOutcome::Proceed { .. }
    ));
    assert!(matches!(
        reloaded.coordinators["claim-once"]
            .claim(CHANNEL, mid)
            .await,
        ClaimOutcome::Wait { .. }
    ));
    owner.release(mid).await;

    // The subscription may also carry other tests' events.
    let event = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            match events.recv().await {
                Ok(event) if event.message_id() == mid.parse::<MessageId>().unwrap() => {
                    return event;
                }
                Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => panic!("bus closed"),
            }
        }
    })
    .await
    .expect("promoted must reach the startup subscriber after a reload");
    assert!(matches!(event, CoordinationEvent::Promoted { .. }));
}

#[tokio::test]
async fn malformed_coordinator_pushes_do_not_interrupt_subsequent_live_delivery() {
    let mut script = vec![json!({"status":"wait","ahead":[BOT_A],"lease_ms":10_000}).to_string()];
    for field in ["message_id", "bot_id", "reply_message_id"] {
        for invalid in ["0", "18446744073709551616", "99999999999999999999"] {
            let mut push =
                json!({"event":"done","message_id":MSG,"bot_id":BOT_A,"reply_message_id":REPLY});
            push[field] = json!(invalid);
            script.push(push.to_string());
        }
    }
    script.push(
        json!({"event":"done","message_id":MSG,"bot_id":BOT_A,"reply_message_id":REPLY})
            .to_string(),
    );
    let addr = spawn_scripted_server(script).await;
    let coordinator = coordinator(&addr, BOT_B);
    let mut events = coordinator.subscribe();
    assert!(matches!(
        coordinator.claim(CHANNEL, MSG).await,
        ClaimOutcome::Wait { .. }
    ));
    let event = recv_event(&mut events, Duration::from_secs(2)).await;
    let CoordinationEvent::Done {
        bot_id,
        reply_message_id,
        ..
    } = &event
    else {
        panic!("valid done push must survive malformed predecessors");
    };
    assert_eq!(
        bot_id.to_string(),
        BOT_A,
        "invalid responder must never enter the event bus"
    );
    assert_eq!(
        reply_message_id.to_string(),
        REPLY,
        "invalid reply must never enter the event bus"
    );
    assert!(matches!(
        events.try_recv(),
        Err(broadcast::error::TryRecvError::Empty)
    ));
    let delivery = crate::discord::delivery::single(
        &crate::harness::test_support::module(),
        crate::discord::events::NotificationEvent::Coordination(event),
        crate::harness::test_support::context(),
    )
    .unwrap();
    let (writer, reader) = tokio::io::duplex(4096);
    let connection = crate::harness::claude_with_output(dione_harness::ProtocolOutput::new(writer));
    connection.deliver(&delivery).await.unwrap();
    let output: Value = serde_json::from_str(
        &BufReader::new(reader)
            .lines()
            .next_line()
            .await
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        output,
        json!({"jsonrpc":"2.0","method":"notifications/claude/channel","params":{"content":"claim-once: 111 replied to message 1001 in channel 1550955234304466985 (reply 2001); no reply needed","meta":{"type":"coordination","event":"done","chat_id":CHANNEL,"message_id":MSG,"by":BOT_A,"reply_message_id":REPLY}}})
    );
}
