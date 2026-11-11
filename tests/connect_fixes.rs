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

#[tokio::test]
async fn a_handshake_with_no_answer_fails_at_the_bound() {
    // A gateway that accepts the TCP connection and the upgrade request and never answers it. chromiumoxide has no dial timeout, so
    // connect_endpoint used to wait for ever (the 8-minute hang of the 2026-11-11 release-gate run, seen in browser-go).
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let mut held = Vec::new();
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                return;
            };
            held.push(sock); // open, never answered
        }
    });
    let start = std::time::Instant::now();
    // The guard: without the bound the call below runs into it instead of hanging the suite.
    let res = tokio::time::timeout(
        Duration::from_secs(6),
        solari_browser::connect_endpoint_timeout(
            &format!("ws://127.0.0.1:{port}/cdp/SECRET-SESSION-ID"),
            Duration::from_millis(300),
        ),
    )
    .await
    .expect("connect_endpoint_timeout must fail at its own bound, not wait for the caller");
    let err = match res {
        Ok(_) => panic!("a gateway that never answers cannot complete a handshake"),
        Err(e) => e.to_string(),
    };
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "took {:?}",
        start.elapsed()
    );
    assert!(
        err.contains("no WebSocket handshake answer within 300ms"),
        "{err}"
    );
    assert!(
        !err.contains("SECRET-SESSION-ID"),
        "the session id leaked: {err}"
    );
}

#[tokio::test]
async fn an_ordinary_dial_failure_does_not_carry_the_session_id() {
    // A port nothing listens on, and a gateway that answers the upgrade with 403: neither error may quote the endpoint, whose path
    // is the session credential.
    let closed = {
        let l = TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let forbidden = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let fport = forbidden.local_addr().unwrap().port();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = forbidden.accept().await else { return };
            tokio::spawn(async move {
                let mut buf = [0u8; 2048];
                let _ = sock.read(&mut buf).await;
                let _ = sock
                    .write_all(b"HTTP/1.1 403 Forbidden\r\ncontent-length: 0\r\nconnection: close\r\n\r\n")
                    .await;
            });
        }
    });
    for (what, port) in [("closed port", closed), ("HTTP 403", fport)] {
        let err = match connect_endpoint(&format!("ws://127.0.0.1:{port}/cdp/SECRET-SESSION-ID")).await {
            Ok(_) => panic!("{what}: cannot connect"),
            Err(e) => e.to_string(),
        };
        assert!(!err.contains("SECRET-SESSION-ID"), "{what}: the session id leaked: {err}");
        assert!(err.contains("failed to connect"), "{what}: {err}");
    }
}
