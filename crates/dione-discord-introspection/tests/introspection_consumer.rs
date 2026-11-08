use dione_config::{Config, LoadedConfig};
use dione_discord_introspection::{
    introspection::{IntrospectionCtx, get_user},
    visibility::{VisibilityRequest, VisibilityRequestError},
    wire,
};
use serde_json::json;
use serenity::{
    http::HttpBuilder,
    model::id::{ChannelId, UserId},
};
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::TcpListener,
    time::timeout,
};

#[tokio::test]
async fn ordinary_consumer_reads_typed_user_and_literal_wire_result() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        timeout(Duration::from_secs(5), async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = tokio::io::BufReader::new(stream);
            let mut request = String::new();
            assert_ne!(reader.read_line(&mut request).await.unwrap(), 0);
            loop {
                let mut line = String::new();
                assert_ne!(reader.read_line(&mut line).await.unwrap(), 0);
                if line == "\r\n" { break; }
            }
            let body = r#"{"id":"91","username":"fixture-orbit-29","discriminator":"0","bot":false}"#;
            reader.get_mut().write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            request
        }).await.unwrap()
    });
    let http = HttpBuilder::new("fixture-unused-token")
        .proxy(format!("http://{address}"))
        .ratelimiter_disabled(true)
        .client(reqwest::Client::builder().no_proxy().build().unwrap())
        .build();
    let ctx = IntrospectionCtx::new(
        Arc::new(http),
        Arc::new(LoadedConfig::try_from_raw(Config::default()).unwrap()),
    );
    let observed = get_user(&ctx, UserId::new(80)).await.unwrap();
    assert_eq!(observed.id(), UserId::new(91));
    assert_eq!(observed.name(), "fixture-orbit-29");
    assert_eq!(
        wire::user(Ok(observed)),
        json!({"id":"91","name":"fixture-orbit-29","discriminator":null,"bot":false})
    );
    assert_eq!(server.await.unwrap(), "GET /api/v10/users/80 HTTP/1.1\r\n");
}

#[test]
fn ordinary_consumer_cannot_create_out_of_bounds_visibility_request() {
    assert!(matches!(
        VisibilityRequest::new(ChannelId::new(42), vec![]),
        Err(VisibilityRequestError::Empty)
    ));
    assert!(matches!(
        VisibilityRequest::new(ChannelId::new(42), vec![UserId::new(1); 26]),
        Err(VisibilityRequestError::TooMany)
    ));
    let request =
        VisibilityRequest::new(ChannelId::new(42), vec![UserId::new(3), UserId::new(2)]).unwrap();
    assert_eq!(request.users(), [UserId::new(3), UserId::new(2)]);
}
