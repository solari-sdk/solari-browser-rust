//! Three defects that only show against the hosted gateway, each reproduced offline over loopback:
//!  1. a `wss://` CDP endpoint could not be dialled at all ("URL error: TLS support not compiled in") because chromiumoxide builds
//!     async-tungstenite without TLS;
//!  2. `sessions().create()` with default options sends a body-less POST, and the gateway's front end answers 411 without a
//!     `Content-Length`;
//!  3. `ConnectedBrowser::disconnect()` waited forever for a handler that does not end on its own.
#![cfg(feature = "connect")]

use async_tungstenite::tokio::accept_async;
use solari_browser::{connect_endpoint, Client, ClientOptions, CreateSessionOptions};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::test]
async fn a_wss_endpoint_fails_on_the_handshake_not_on_missing_tls() {
    // A peer that accepts the TCP connection and hangs up: the dial gets past TCP, so a build without TLS fails with
    // "TLS support not compiled in" and a build with TLS fails on the TLS handshake instead.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else { return };
            drop(sock);
        }
    });
    let err = match connect_endpoint(&format!("wss://127.0.0.1:{port}/cdp/x")).await {
        Ok(_) => panic!("a peer that hangs up cannot complete a handshake"),
        Err(e) => e.to_string(),
    };
    assert!(!err.contains("TLS support not compiled in"), "wss:// cannot be dialled: {err}");
}

#[tokio::test]
async fn disconnect_returns_even_though_the_browser_stays_up() {
    // A CDP peer that accepts the WebSocket and then says nothing, like a browser that stays up.
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let (sock, _) = listener.accept().await.unwrap();
        let _ws = accept_async(sock).await.unwrap();
        tokio::time::sleep(Duration::from_secs(60)).await;
    });
    let connected = connect_endpoint(&format!("ws://127.0.0.1:{port}/")).await.expect("connect");
    tokio::time::timeout(Duration::from_secs(5), connected.disconnect())
        .await
        .expect("disconnect() must return, not wait for a handler that never ends");
}

#[tokio::test]
async fn a_default_create_sends_content_length_zero() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else { return };
            let s2 = s2.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 4096];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                s2.lock().unwrap().push(String::from_utf8_lossy(&buf[..n]).to_lowercase());
                let body = r#"{"sessionId":"s1","wsEndpoint":"ws://x/ws","cdpEndpoint":"ws://x/cdp","expiresAt":"2030-01-01T00:00:00Z"}"#;
                let resp = format!(
                    "HTTP/1.1 201 Created\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    let client = Client::new(ClientOptions::new("slr_test_x", format!("http://127.0.0.1:{port}"))).expect("client");
    client.sessions().create(CreateSessionOptions::new()).await.expect("create");
    let head = seen.lock().unwrap()[0].clone();
    assert!(head.starts_with("post /sessions "), "{head}");
    assert!(head.lines().any(|l| l.trim() == "content-length: 0"), "a body-less POST must carry content-length: 0, got:\n{head}");
}
