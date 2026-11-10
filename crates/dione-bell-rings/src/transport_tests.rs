//! Real HTTP/SDK lifecycle checks for the private diagnostic transport.
use crate::diagnostic_http::DiagnosticHttpClient;
use rmcp::{
    ServiceExt, model::ClientConfig,
    transport::streamable_http_client::StreamableHttpClientTransportConfig,
};
use serde_json::{Value, json};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::{JoinHandle, JoinSet},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy)]
enum Scenario {
    OversizedGet,
    UnderLimit,
    InterruptedGet,
    OversizedPost,
    InterruptedPost,
}

struct Fixture {
    uri: String,
    gets: Arc<AtomicUsize>,
    resumptions: Arc<Mutex<Vec<String>>>,
    start_events: Arc<tokio::sync::Notify>,
    stop: CancellationToken,
    server: JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.stop.cancel();
        self.server.abort();
    }
}

async fn read_request(socket: &mut TcpStream) -> (String, Value) {
    let mut bytes = Vec::new();
    while !bytes.ends_with(b"\r\n\r\n") {
        let mut byte = [0];
        socket.read_exact(&mut byte).await.unwrap();
        bytes.push(byte[0]);
        assert!(bytes.len() < 16_384);
    }
    let headers = String::from_utf8(bytes).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")
                .map(|v| v.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    assert!(length < 16_384);
    let mut body = vec![0; length];
    socket.read_exact(&mut body).await.unwrap();
    let body = if body.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (headers, body)
}
async fn json_response(socket: &mut TcpStream, body: Value) {
    let body = body.to_string();
    socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: fixture-session\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
}
async fn stream_response(socket: &mut TcpStream, first: &str) {
    socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: 65536\r\nConnection: close\r\n\r\n").await.unwrap();
    socket.write_all(first.as_bytes()).await.unwrap();
}
async fn fixture(scenario: Scenario) -> Fixture {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let uri = format!(
        "http://{}/SYNTHETIC_TRANSPORT_CANARY",
        listener.local_addr().unwrap()
    );
    let gets = Arc::new(AtomicUsize::new(0));
    let resumptions = Arc::new(Mutex::new(Vec::new()));
    let stop = CancellationToken::new();
    let start_events = Arc::new(tokio::sync::Notify::new());
    let event_gate = start_events.clone();
    let counts = gets.clone();
    let resumed = resumptions.clone();
    let shutdown = stop.clone();
    let server = tokio::spawn(async move {
        let mut children = JoinSet::new();
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => break,
                accepted = listener.accept() => {
                    let (mut socket, _) = accepted.unwrap(); let counts = counts.clone(); let resumed = resumed.clone(); let event_gate = event_gate.clone();
                    children.spawn(async move {
                        let (headers, body) = read_request(&mut socket).await;
                        if headers.starts_with("GET ") {
                            let count = counts.fetch_add(1, Ordering::SeqCst)+1;
                            let last = headers.lines().find_map(|line| line.to_ascii_lowercase().strip_prefix("last-event-id:").map(|v|v.trim().to_owned()));
                            if let Some(last) = last { resumed.lock().unwrap().push(last); }
                            let oversize = matches!(scenario,Scenario::OversizedGet) || (matches!(scenario,Scenario::OversizedPost) && count>1);
                            if oversize && count <=8 {
                                if count == 1 { event_gate.notified().await; }
                                stream_response(&mut socket,&format!("data: {}", "x".repeat(256))).await;
                            } else if matches!(scenario,Scenario::InterruptedGet) && count==1 {
                                stream_response(&mut socket,"id: fixture-get\nevent: ping\ndata: {}\n\n").await;
                                return; // Truncated HTTP body is a recoverable source error.
                            } else {
                                stream_response(&mut socket,": fixture-keepalive\n\n").await;
                            }
                            // Hold successful streams open; the ninth mutant connection is
                            // a fixture safety bound, not a natural transport retry limit.
                            std::future::pending::<()>().await;
                        } else if body.get("method")==Some(&json!("initialize")) {
                            json_response(&mut socket,json!({"jsonrpc":"2.0","id":body["id"],"result":{"protocolVersion":body["params"]["protocolVersion"],"capabilities":{},"serverInfo":{"name":"fixture-orbit","version":"0"}}})).await;
                        } else if body.get("method")==Some(&json!("tools/list")) && matches!(scenario,Scenario::OversizedPost|Scenario::InterruptedPost) {
                            stream_response(&mut socket,"id: fixture-post\nevent: ping\ndata: {}\n\n").await;
                            // Distinct writes let the real SSE parser consume the event ID
                            // before the following overflow or ordinary interruption.
                            tokio::time::sleep(Duration::from_millis(30)).await;
                            if matches!(scenario,Scenario::OversizedPost) {
                                let _ = socket.write_all(format!("data: {}","x".repeat(256)).as_bytes()).await;
                                std::future::pending::<()>().await;
                            }
                        } else {
                            socket.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
                        }
                    });
                }
            }
        }
        children.abort_all();
        while children.join_next().await.is_some() {}
    });
    Fixture {
        uri,
        gets,
        resumptions,
        start_events,
        stop,
        server,
    }
}
async fn wait_for(mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("real transport did not reach the required fixture boundary");
}
async fn connect(
    fixture: &Fixture,
    cap: usize,
) -> rmcp::service::RunningService<rmcp::service::RoleClient, ClientConfig> {
    let http = reqwest_mcp::Client::builder()
        .pool_max_idle_per_host(0)
        .build()
        .unwrap();
    let transport = DiagnosticHttpClient::new(http).into_transport(
        StreamableHttpClientTransportConfig::with_uri(fixture.uri.clone()).max_sse_event_size(cap),
    );
    tokio::time::timeout(
        Duration::from_secs(5),
        ClientConfig::default().serve(transport),
    )
    .await
    .unwrap()
    .unwrap()
}
#[tokio::test]
async fn oversized_get_terminates_real_service_without_second_get() {
    let fixture = fixture(Scenario::OversizedGet).await;
    let service = connect(&fixture, 64).await;
    fixture.start_events.notify_one();
    wait_for(|| fixture.gets.load(Ordering::SeqCst) > 0).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        fixture.gets.load(Ordering::SeqCst),
        1,
        "overflow must not reconnect GET through the real SDK"
    );
    wait_for(|| service.is_closed()).await;
}
#[tokio::test]
async fn under_limit_real_service_stays_live_without_reconnect() {
    let fixture = fixture(Scenario::UnderLimit).await;
    let service = connect(&fixture, 64).await;
    fixture.start_events.notify_one();
    wait_for(|| fixture.gets.load(Ordering::SeqCst) > 0).await;
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(fixture.gets.load(Ordering::SeqCst), 1);
    assert!(!service.is_closed());
    service.cancel().await.unwrap();
}
#[tokio::test]
async fn ordinary_get_interruption_reconnects_with_event_id() {
    let fixture = fixture(Scenario::InterruptedGet).await;
    let service = connect(&fixture, 128).await;
    wait_for(|| fixture.gets.load(Ordering::SeqCst) >= 2).await;
    assert_eq!(fixture.gets.load(Ordering::SeqCst), 2);
    assert_eq!(*fixture.resumptions.lock().unwrap(), vec!["fixture-get"]);
    assert!(!service.is_closed());
    service.cancel().await.unwrap();
}
#[tokio::test]
async fn oversized_post_after_event_id_terminates_without_resumption_get() {
    let fixture = fixture(Scenario::OversizedPost).await;
    let service = connect(&fixture, 64).await;
    fixture.start_events.notify_one();
    wait_for(|| fixture.gets.load(Ordering::SeqCst) > 0).await;
    let _ = tokio::time::timeout(Duration::from_secs(5), service.peer().list_tools(None))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    assert_eq!(
        fixture.gets.load(Ordering::SeqCst),
        1,
        "POST overflow must not create a resumption GET"
    );
    assert!(fixture.resumptions.lock().unwrap().is_empty());
    wait_for(|| service.is_closed()).await;
}
#[tokio::test]
async fn ordinary_post_interruption_resumes_with_event_id() {
    let fixture = fixture(Scenario::InterruptedPost).await;
    let service = connect(&fixture, 64).await;
    fixture.start_events.notify_one();
    wait_for(|| fixture.gets.load(Ordering::SeqCst) > 0).await;
    {
        let request = service.peer().list_tools(None);
        tokio::pin!(request);
        tokio::select! { _ = &mut request => panic!("fixture should keep the resumed request pending"), _ = wait_for(||fixture.gets.load(Ordering::SeqCst)>=2) => {} }
        assert_eq!(fixture.gets.load(Ordering::SeqCst), 2);
        assert_eq!(*fixture.resumptions.lock().unwrap(), vec!["fixture-post"]);
        assert!(!service.is_closed());
    }
    service.cancel().await.unwrap();
}
