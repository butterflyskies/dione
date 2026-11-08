//! Private loopback support for installed metadata tool tests.
use dione_config::{Config, LoadedConfig};
use dione_discord_introspection::introspection::IntrospectionCtx;
use serde_json::Value;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::timeout,
};

/// Keeps fixture traffic on the local SDK proxy rather than its rate-limiter route.
pub(crate) fn fixture_http(address: SocketAddr) -> serenity::http::Http {
    let proxy = format!("http://{address}");
    let http = serenity::http::HttpBuilder::new("fake")
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
    assert!(
        http.ratelimiter.is_none(),
        "the SDK rate limiter bypasses the local proxy"
    );
    assert_eq!(http.proxy.as_deref(), Some(proxy.as_str()));
    http
}

pub(crate) async fn fixture(
    responses: Vec<(&'static str, Value)>,
) -> (IntrospectionCtx, JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        timeout(Duration::from_secs(5), async move {
                let mut requests = Vec::new();
                for (status, body) in responses {
                    let (stream, _) = listener.accept().await.unwrap();
                    let mut reader = tokio::io::BufReader::new(stream);
                    let mut line = String::new();
                    assert_ne!(reader.read_line(&mut line).await.unwrap(), 0, "request line ended at EOF");
                    let mut length = 0;
                    loop {
                        let mut header = String::new();
                        assert_ne!(reader.read_line(&mut header).await.unwrap(), 0, "request headers ended at EOF");
                        if header == "\r\n" { break; }
                        if let Some(value) = header.to_ascii_lowercase().strip_prefix("content-length:") {
                            length = value.trim().parse::<usize>().unwrap();
                        }
                    }
                    let mut bytes = vec![0; length];
                    reader.read_exact(&mut bytes).await.unwrap();
                    assert!(bytes.is_empty(), "metadata reads must have no request body");
                    requests.push(line.trim_end().to_owned());
                    let body = body.to_string();
                    let response = format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
                    reader.get_mut().write_all(response.as_bytes()).await.unwrap();
                }
                requests
            }).await.expect("fake Discord exchange deadline")
    });
    let ctx = IntrospectionCtx::new(
        Arc::new(fixture_http(address)),
        Arc::new(
            LoadedConfig::try_from_raw(Config {
                timezone: Some("America/Chicago".into()),
                ..Config::default()
            })
            .expect("valid test configuration"),
        ),
    );
    (ctx, server)
}
