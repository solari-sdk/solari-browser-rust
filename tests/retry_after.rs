//! A 503 that carries `Retry-After` (a jail kind with no capacity answers `503 NoCapacity`, `Retry-After: 10`; PARITY B17) makes the
//! client wait that long before the next attempt instead of its own fixed backoff. Every attempt of one create keeps the same
//! Idempotency-Key.

use solari_browser::{Client, ClientOptions};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Seen = Arc<Mutex<Vec<(Instant, String)>>>;

/// First request: 503 NoCapacity (with `retry_after` when given); later requests: 200.
async fn gateway(retry_after: Option<&'static str>) -> (Client, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let s2 = s2.clone();
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                let key = head.lines().find(|l| l.starts_with("idempotency-key:")).unwrap_or("").to_string();
                let first = {
                    let mut g = s2.lock().unwrap();
                    g.push((Instant::now(), key));
                    g.len() == 1
                };
                let (status, extra, body) = if first {
                    (503, retry_after.map(|v| format!("Retry-After: {v}\r\n")).unwrap_or_default(), r#"{"error":"no capacity","code":"NoCapacity"}"#)
                } else {
                    (200, String::new(), r#"{"sessionId":"s-1","wsEndpoint":"ws://127.0.0.1:1/ws","cdpEndpoint":"ws://127.0.0.1:1/cdp","expiresAt":"2030-01-01T00:00:00Z"}"#)
                };
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    let mut opts = ClientOptions::new("slr_test_x", format!("http://127.0.0.1:{port}"));
    opts.backoff_ms = 10;
    (Client::new(opts).unwrap(), seen)
}

fn gap(seen: &Seen) -> (usize, Duration) {
    let g = seen.lock().unwrap();
    (g.len(), g[1].0 - g[0].0)
}

#[tokio::test]
async fn a_503_with_retry_after_waits_that_long_and_keeps_the_idempotency_key() {
    let (client, seen) = gateway(Some("1")).await;
    client.sessions().create(Default::default()).await.unwrap();
    let (n, d) = gap(&seen);
    assert_eq!(n, 2);
    assert!(d >= Duration::from_millis(900), "the retry came after {d:?}; Retry-After: 1 asks for 1s");
    let g = seen.lock().unwrap();
    assert!(!g[0].1.is_empty() && g[0].1 == g[1].1, "same Idempotency-Key on every attempt: {:?} vs {:?}", g[0].1, g[1].1);
}

#[tokio::test]
async fn a_503_without_retry_after_keeps_the_backoff() {
    let (client, seen) = gateway(None).await;
    client.sessions().create(Default::default()).await.unwrap();
    let (_, d) = gap(&seen);
    assert!(d < Duration::from_millis(800), "no Retry-After: the retry came after {d:?}");
}
