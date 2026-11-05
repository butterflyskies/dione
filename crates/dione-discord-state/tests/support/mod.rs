use serde_json::Value;
use serenity::http::{Http, HttpBuilder};
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};

pub struct Fixture {
    pub http: Http,
    pub requests: Arc<Mutex<Vec<String>>>,
    task: Option<JoinHandle<()>>,
}
impl Fixture {
    pub async fn new(responses: Vec<(&'static str, Value)>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let proxy = format!("http://{}", listener.local_addr().unwrap());
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        let task = tokio::spawn(async move {
            timeout(Duration::from_secs(5), async move {
                for (status, body) in responses {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut reader = tokio::io::BufReader::new(stream);
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        assert_ne!(reader.read_line(&mut header).await.unwrap(), 0);
                        if header == "\r\n" { break; }
                        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).await.unwrap();
                    assert!(bytes.is_empty());
                    recorded.lock().unwrap().push(line.trim_end().to_owned());
                    let body = body.to_string();
                    let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    reader.get_mut().write_all(response.as_bytes()).await.unwrap();
                }
            }).await.expect("loopback exchange deadline");
        });
        let http = HttpBuilder::new("fake")
            .proxy(&proxy)
            .ratelimiter_disabled(true)
            .client(
                reqwest::Client::builder()
                    .no_proxy()
                    .redirect(reqwest::redirect::Policy::none())
                    .build()
                    .unwrap(),
            )
            .build();
        assert!(http.ratelimiter.is_none());
        assert_eq!(http.proxy.as_deref(), Some(proxy.as_str()));
        Self {
            http,
            requests,
            task: Some(task),
        }
    }
    pub fn count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }
    pub async fn finish(&mut self) {
        timeout(Duration::from_secs(5), self.task.as_mut().unwrap())
            .await
            .unwrap()
            .unwrap();
        self.task = None;
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}
