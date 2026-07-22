//! PronounDB v2 HTTP adapter.

use crate::pronouns::{PronounProvider, PronounProviderError, PronounSelection};
use futures_util::StreamExt as _;
use serde::Deserialize;
use serenity::model::id::UserId;
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::sync::{Mutex, Semaphore};

const LOOKUP_URL: &str = "https://pronoundb.org/api/v2/lookup";
const CACHE_TTL: Duration = Duration::from_secs(60 * 60);
const CACHE_CAPACITY: usize = 256;
const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_CONCURRENT_LOOKUPS: usize = 8;

pub struct PronounDbProvider {
    client: reqwest::Client,
    endpoint: String,
    cache: Mutex<PronounCache>,
    request_limit: Semaphore,
    single_flight: Mutex<HashMap<UserId, Weak<Mutex<()>>>>,
}

impl PronounDbProvider {
    pub fn new(version: &str) -> Result<Self, reqwest::Error> {
        Self::with_endpoint(version, LOOKUP_URL)
    }

    fn with_endpoint(version: &str, endpoint: &str) -> Result<Self, reqwest::Error> {
        let user_agent = format!("Dione/{version} (https://github.com/butterflyskies/dione)");
        let client = reqwest::Client::builder().user_agent(user_agent).build()?;
        Ok(Self {
            client,
            endpoint: endpoint.to_owned(),
            cache: Mutex::new(PronounCache::default()),
            request_limit: Semaphore::new(MAX_CONCURRENT_LOOKUPS),
            single_flight: Mutex::new(HashMap::new()),
        })
    }

    async fn lookup_inner(
        &self,
        user_id: UserId,
    ) -> Result<Option<PronounSelection>, PronounProviderError> {
        let now = Instant::now();
        if let Some(selection) = self.cache.lock().await.get(user_id, now) {
            return Ok(selection);
        }

        let flight = {
            let mut flights = self.single_flight.lock().await;
            flights.retain(|_, flight| flight.strong_count() > 0);
            match flights.get(&user_id).and_then(Weak::upgrade) {
                Some(flight) => flight,
                None => {
                    let flight = Arc::new(Mutex::new(()));
                    flights.insert(user_id, Arc::downgrade(&flight));
                    flight
                }
            }
        };
        // This keyed guard intentionally spans the lookup so requests for the
        // same user coalesce. Unrelated users never share this mutex.
        let _single_flight = flight.lock().await;
        let now = Instant::now();
        if let Some(selection) = self.cache.lock().await.get(user_id, now) {
            return Ok(selection);
        }
        let _request_permit = self
            .request_limit
            .acquire()
            .await
            .map_err(|_| PronounProviderError::Transport)?;

        let response = self
            .client
            .get(&self.endpoint)
            .query(&[
                ("platform", "discord"),
                ("ids", &user_id.get().to_string()),
            ])
            .send()
            .await
            .map_err(|_| PronounProviderError::Transport)?
            .error_for_status()
            .map_err(|_| PronounProviderError::Transport)?;
        let body = read_bounded_body(response).await?;
        let records = serde_json::from_slice::<HashMap<String, PronounDbRecord>>(&body)
            .map_err(|_| PronounProviderError::MalformedResponse)?;
        let selection = match records.get(&user_id.get().to_string()) {
            None => None,
            Some(record) => match record.sets.get("en") {
                None => None,
                Some(codes) => Some(
                    PronounSelection::from_codes(codes)
                        .ok_or(PronounProviderError::MalformedResponse)?,
                ),
            },
        };
        self.cache
            .lock()
            .await
            .insert(user_id, selection.clone(), now);
        Ok(selection)
    }
}

async fn read_bounded_body(
    response: reqwest::Response,
) -> Result<Vec<u8>, PronounProviderError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(PronounProviderError::MalformedResponse);
    }

    let mut body = Vec::with_capacity(
        response
            .content_length()
            .unwrap_or(0)
            .min(MAX_RESPONSE_BYTES as u64) as usize,
    );
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| PronounProviderError::Transport)?;
        let next_len = body
            .len()
            .checked_add(chunk.len())
            .ok_or(PronounProviderError::MalformedResponse)?;
        if next_len > MAX_RESPONSE_BYTES {
            return Err(PronounProviderError::MalformedResponse);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

impl PronounProvider for PronounDbProvider {
    fn lookup(
        &self,
        user_id: UserId,
    ) -> Pin<
        Box<
            dyn Future<Output = Result<Option<PronounSelection>, PronounProviderError>> + Send + '_,
        >,
    > {
        Box::pin(self.lookup_inner(user_id))
    }
}

#[derive(Deserialize)]
struct PronounDbRecord {
    sets: HashMap<String, Vec<String>>,
}

#[derive(Default)]
struct PronounCache {
    entries: HashMap<UserId, CacheEntry>,
}

impl PronounCache {
    fn get(&mut self, user_id: UserId, now: Instant) -> Option<Option<PronounSelection>> {
        match self.entries.get(&user_id) {
            Some(entry) if now.duration_since(entry.stored_at) < CACHE_TTL => {
                Some(entry.selection.clone())
            }
            Some(_) => {
                self.entries.remove(&user_id);
                None
            }
            None => None,
        }
    }

    fn insert(
        &mut self,
        user_id: UserId,
        selection: Option<PronounSelection>,
        now: Instant,
    ) {
        if self.entries.len() >= CACHE_CAPACITY
            && !self.entries.contains_key(&user_id)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.stored_at)
                .map(|(id, _)| *id)
        {
            self.entries.remove(&oldest);
        }
        self.entries.insert(
            user_id,
            CacheEntry {
                selection,
                stored_at: now,
            },
        );
    }
}

struct CacheEntry {
    selection: Option<PronounSelection>,
    stored_at: Instant,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures_util::future::join_all;
    use std::sync::{Arc, atomic::{AtomicUsize, Ordering}};
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::task::JoinHandle;

    struct TestServer {
        endpoint: String,
        requests: Arc<AtomicUsize>,
        max_active: Arc<AtomicUsize>,
        captured: Arc<Mutex<Vec<String>>>,
        task: JoinHandle<()>,
    }

    async fn spawn_server(expected: usize, raw_response: Vec<u8>, delay_ms: u64) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let endpoint = format!("http://{}/api/v2/lookup", listener.local_addr().expect("addr"));
        let requests = Arc::new(AtomicUsize::new(0));
        let max_active = Arc::new(AtomicUsize::new(0));
        let captured = Arc::new(Mutex::new(Vec::new()));
        let task_requests = Arc::clone(&requests);
        let task_max_active = Arc::clone(&max_active);
        let task_captured = Arc::clone(&captured);
        let response = Arc::new(raw_response);
        let task = tokio::spawn(async move {
            let active = Arc::new(AtomicUsize::new(0));
            let mut connections = tokio::task::JoinSet::new();
            for _ in 0..expected {
                let (mut socket, _) = listener.accept().await.expect("accept");
                let response = Arc::clone(&response);
                let active = Arc::clone(&active);
                let requests = Arc::clone(&task_requests);
                let max_active = Arc::clone(&task_max_active);
                let captured = Arc::clone(&task_captured);
                connections.spawn(async move {
                    let current = active.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active.fetch_max(current, Ordering::SeqCst);
                    requests.fetch_add(1, Ordering::SeqCst);
                    let mut request = vec![0; 4096];
                    let read = socket.read(&mut request).await.expect("read request");
                    captured.lock().await.push(
                        String::from_utf8_lossy(&request[..read]).into_owned(),
                    );
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                    socket.write_all(&response).await.expect("write response");
                    active.fetch_sub(1, Ordering::SeqCst);
                });
            }
            while connections.join_next().await.is_some() {}
        });
        TestServer {
            endpoint,
            requests,
            max_active,
            captured,
            task,
        }
    }

    fn response(status: &str, body: &[u8]) -> Vec<u8> {
        let mut response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[test]
    fn parses_documented_v2_map_shape() {
        let records: HashMap<String, PronounDbRecord> = serde_json::from_value(serde_json::json!({
            "42": { "sets": { "en": ["she", "they"] } }
        }))
        .expect("documented response");
        let selection = PronounSelection::from_codes(&records["42"].sets["en"])
            .expect("supported pronouns");
        assert_eq!(selection.display(), "she/they");
    }

    #[tokio::test]
    async fn sends_documented_request_and_descriptive_user_agent() {
        let body = br#"{"42":{"sets":{"en":["she"]}}}"#;
        let server = spawn_server(1, response("200 OK", body), 0).await;
        let provider = PronounDbProvider::with_endpoint("9.8.7", &server.endpoint).expect("client");
        let selection = provider.lookup_inner(UserId::new(42)).await.expect("lookup");
        assert_eq!(selection.expect("record").display(), "she/her");
        server.task.await.expect("server");
        let request = &server.captured.lock().await[0];
        assert!(request.starts_with("GET /api/v2/lookup?platform=discord&ids=42 HTTP/1.1"));
        assert!(request.to_ascii_lowercase().contains(
            "user-agent: dione/9.8.7 (https://github.com/butterflyskies/dione)"
        ));
    }

    #[tokio::test]
    async fn rejects_oversized_declared_and_streamed_bodies() {
        let declared = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        )
        .into_bytes();
        let server = spawn_server(1, declared, 0).await;
        let provider = PronounDbProvider::with_endpoint("1.0.0", &server.endpoint).expect("client");
        assert_eq!(
            provider.lookup_inner(UserId::new(1)).await,
            Err(PronounProviderError::MalformedResponse)
        );
        server.task.await.expect("server");

        let first = vec![b'a'; MAX_RESPONSE_BYTES];
        let second = [b'b'];
        let mut chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
        chunked.extend_from_slice(format!("{:X}\r\n", first.len()).as_bytes());
        chunked.extend_from_slice(&first);
        chunked.extend_from_slice(b"\r\n1\r\n");
        chunked.extend_from_slice(&second);
        chunked.extend_from_slice(b"\r\n0\r\n\r\n");
        let server = spawn_server(1, chunked, 0).await;
        let provider = PronounDbProvider::with_endpoint("1.0.0", &server.endpoint).expect("client");
        assert_eq!(
            provider.lookup_inner(UserId::new(1)).await,
            Err(PronounProviderError::MalformedResponse)
        );
        server.task.await.expect("server");
    }

    #[tokio::test]
    async fn coalesces_same_id_and_bounds_distinct_upstream_calls() {
        let server = spawn_server(1, response("200 OK", b"{}"), 25).await;
        let provider = Arc::new(
            PronounDbProvider::with_endpoint("1.0.0", &server.endpoint).expect("client"),
        );
        let lookups = (0..32).map(|_| {
            let provider = Arc::clone(&provider);
            async move { provider.lookup_inner(UserId::new(42)).await }
        });
        assert!(join_all(lookups).await.into_iter().all(|result| result == Ok(None)));
        server.task.await.expect("server");
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);

        let count = MAX_CONCURRENT_LOOKUPS * 2;
        let server = spawn_server(count, response("200 OK", b"{}"), 25).await;
        let provider = Arc::new(
            PronounDbProvider::with_endpoint("1.0.0", &server.endpoint).expect("client"),
        );
        let lookups = (1..=count).map(|id| {
            let provider = Arc::clone(&provider);
            async move { provider.lookup_inner(UserId::new(id as u64)).await }
        });
        assert!(join_all(lookups).await.into_iter().all(|result| result == Ok(None)));
        server.task.await.expect("server");
        assert!(server.max_active.load(Ordering::SeqCst) <= MAX_CONCURRENT_LOOKUPS);
    }

    #[tokio::test]
    async fn caches_absence_but_not_provider_errors() {
        let server = spawn_server(1, response("200 OK", b"{}"), 0).await;
        let provider = PronounDbProvider::with_endpoint("1.0.0", &server.endpoint).expect("client");
        assert_eq!(provider.lookup_inner(UserId::new(1)).await, Ok(None));
        assert_eq!(provider.lookup_inner(UserId::new(1)).await, Ok(None));
        server.task.await.expect("server");
        assert_eq!(server.requests.load(Ordering::SeqCst), 1);

        let server = spawn_server(2, response("500 Internal Server Error", b"{}"), 0).await;
        let provider = PronounDbProvider::with_endpoint("1.0.0", &server.endpoint).expect("client");
        for _ in 0..2 {
            assert_eq!(
                provider.lookup_inner(UserId::new(1)).await,
                Err(PronounProviderError::Transport)
            );
        }
        server.task.await.expect("server");
        assert_eq!(server.requests.load(Ordering::SeqCst), 2);
    }
}
