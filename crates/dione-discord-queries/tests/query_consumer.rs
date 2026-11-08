use dione_config::{ChannelConfig, Config, LoadedConfig};
use dione_discord_queries::{
    QueryCtx, QueryErrorKind, queries,
    search::{SearchLimit, SearchOffset, SearchTransport},
    wire,
};
use dione_pre_send::ConstructId;
use serenity::{http::HttpBuilder, model::id::ChannelId};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt},
    net::TcpListener,
};
#[tokio::test]
async fn ordinary_consumer_preserves_policy_before_sdk_and_literal_pin_response() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(5),async move{
 let (stream,_)=listener.accept().await.unwrap();let mut reader=tokio::io::BufReader::new(stream);let mut request=String::new();reader.read_line(&mut request).await.unwrap();
 loop{let mut line=String::new();reader.read_line(&mut line).await.unwrap();if line=="\r\n"{break;}}
 reader.get_mut().write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n[]").await.unwrap();request
 }).await.unwrap()
    });
    let http = HttpBuilder::new("fixture-unused-token")
        .proxy(format!("http://{address}"))
        .ratelimiter_disabled(true)
        .client(reqwest::Client::builder().no_proxy().build().unwrap())
        .build();
    let mut config = Config::default();
    config.channels.push(ChannelConfig {
        id: "42".into(),
        ..Default::default()
    });
    let ctx = QueryCtx::new(
        Arc::new(http),
        dione_discord_state::new_state(),
        Arc::new(LoadedConfig::try_from_raw(config).unwrap()),
        "/tmp".into(),
        ConstructId::parse("fixture-orbit-30").unwrap(),
    );
    let denied = queries::fetch_pins(&ctx, ChannelId::new(99))
        .await
        .unwrap_err();
    assert!(matches!(denied.kind,QueryErrorKind::OutboundDenied(id) if id.get()==99));
    assert_eq!(
        wire::pins(queries::fetch_pins(&ctx, ChannelId::new(42)).await),
        serde_json::json!({"messages":[],"count":0})
    );
    assert_eq!(
        server.await.unwrap(),
        "GET /api/v10/channels/42/pins HTTP/1.1\r\n"
    );
}
#[test]
fn ordinary_consumer_validates_search_ranges_and_credential_destination() {
    assert!(SearchLimit::try_from(0).is_err());
    assert!(SearchLimit::try_from(26).is_err());
    assert_eq!(SearchLimit::try_from(25).unwrap().get(), 25);
    assert!(SearchOffset::try_from(9976).is_err());
    assert_eq!(SearchOffset::try_from(9975).unwrap().get(), 9975);
    assert!(SearchTransport::with_endpoint("file:///tmp/fixture".parse().unwrap()).is_err());
    assert!(
        SearchTransport::with_endpoint(
            "https://example.invalid/api/v10?secret=fixture"
                .parse()
                .unwrap()
        )
        .is_err()
    );
    assert!(SearchTransport::with_endpoint("http://127.0.0.1:9/api/v10".parse().unwrap()).is_ok());
}
