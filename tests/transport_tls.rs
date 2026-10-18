//! Verify the native Discord transports authenticate certificates and hostnames.

use futures_util::{SinkExt, StreamExt};
use rcgen::generate_simple_self_signed;
use rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::timeout,
};
use tokio_rustls::TlsAcceptor;
use tokio_tungstenite::{
    Connector, accept_async, client_async_tls_with_config, tungstenite::Message,
};

struct TlsServer {
    address: SocketAddr,
    certificate: Vec<u8>,
    task: JoinHandle<()>,
}

impl Drop for TlsServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn server(websocket: bool) -> TlsServer {
    let key = generate_simple_self_signed(vec!["localhost".to_owned()]).unwrap();
    let certificate = key.cert.der().to_vec();
    let config =
        ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(
                vec![key.cert.der().clone()],
                PrivatePkcs8KeyDer::from(key.key_pair.serialize_der()).into(),
            )
            .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        let Ok(mut stream) = TlsAcceptor::from(Arc::new(config)).accept(stream).await else {
            return;
        };
        if websocket {
            let mut socket = accept_async(stream).await.unwrap();
            socket
                .send(Message::Text("authenticated".to_owned()))
                .await
                .unwrap();
        } else {
            let mut request = [0; 4096];
            let received = stream.read(&mut request).await.unwrap();
            assert_ne!(received, 0, "empty HTTP request");
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 13\r\nConnection: close\r\n\r\nauthenticated")
                .await
                .unwrap();
        }
    });
    TlsServer {
        address,
        certificate,
        task,
    }
}

#[tokio::test]
async fn native_http_requires_trusted_certificate_and_matching_hostname() {
    for (trusted, host, allowed) in [
        (true, "localhost", true),
        (false, "localhost", false),
        (true, "127.0.0.1", false),
    ] {
        let server = server(false).await;
        let mut builder = reqwest::Client::builder()
            .use_native_tls()
            .no_proxy()
            .tls_built_in_root_certs(false)
            .resolve("localhost", server.address)
            .timeout(Duration::from_secs(5));
        if trusted {
            builder = builder
                .add_root_certificate(reqwest::Certificate::from_der(&server.certificate).unwrap());
        }
        let result = builder
            .build()
            .unwrap()
            .get(format!("https://{host}:{}/", server.address.port()))
            .send()
            .await;
        if allowed {
            let response = result.unwrap();
            assert_eq!(response.status(), reqwest::StatusCode::OK);
            assert_eq!(response.text().await.unwrap(), "authenticated");
        } else {
            assert!(
                result.unwrap_err().is_connect(),
                "TLS authentication must fail before HTTP"
            );
        }
    }
}

#[tokio::test]
async fn native_websocket_requires_trusted_certificate_and_matching_hostname() {
    for (trusted, host, allowed) in [
        (true, "localhost", true),
        (false, "localhost", false),
        (true, "127.0.0.1", false),
    ] {
        let server = server(true).await;
        let mut builder = native_tls::TlsConnector::builder();
        builder.disable_built_in_roots(true);
        if trusted {
            builder.add_root_certificate(
                native_tls::Certificate::from_der(&server.certificate).unwrap(),
            );
        }
        let stream = TcpStream::connect(server.address).await.unwrap();
        let result = timeout(
            Duration::from_secs(5),
            client_async_tls_with_config(
                format!("wss://{host}:{}/", server.address.port()),
                stream,
                None,
                Some(Connector::NativeTls(builder.build().unwrap())),
            ),
        )
        .await
        .unwrap();
        if allowed {
            let (mut socket, _) = result.unwrap();
            let message = timeout(Duration::from_secs(5), socket.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            assert_eq!(message, Message::Text("authenticated".to_owned()));
        } else {
            assert!(matches!(
                result,
                Err(tokio_tungstenite::tungstenite::Error::Tls(_))
            ));
        }
    }
}
