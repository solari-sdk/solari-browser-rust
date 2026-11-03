//! Profile editor handle against a fake gateway over loopback: paths, the X-Handoff-Id header, error statuses and
//! `wait_for_save`.

use solari_browser::{Client, ClientOptions, EditorStatus};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

type Seen = Arc<Mutex<Vec<String>>>;

/// `route(request_head_lowercased) -> (status, body)`; every request head is also recorded.
async fn fake_gateway(
    route: impl Fn(&str) -> (u16, String) + Send + Sync + 'static,
) -> (Client, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let (s2, route) = (seen.clone(), Arc::new(route));
    tokio::spawn(async move {
        loop {
            let Ok((mut sock, _)) = listener.accept().await else {
                return;
            };
            let (s2, route) = (s2.clone(), route.clone());
            tokio::spawn(async move {
                let mut buf = vec![0u8; 8192];
                let n = sock.read(&mut buf).await.unwrap_or(0);
                let head = String::from_utf8_lossy(&buf[..n]).to_lowercase();
                s2.lock().unwrap().push(head.clone());
                let (status, body) = route(&head);
                let resp = format!(
                    "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = sock.write_all(resp.as_bytes()).await;
            });
        }
    });
    let mut opts = ClientOptions::new("slr_test_x", format!("http://127.0.0.1:{port}"));
    opts.backoff_ms = 0;
    (Client::new(opts).unwrap(), seen)
}

const HANDOFF: &str = r#"{"handoffId":"h_1","url":"https://edit.example/h_1","expiresAt":"2030-01-01T00:30:00.000Z","version":3,"profileId":"prof_1","mode":"cold"}"#;

fn editor_route(head: &str) -> (u16, String) {
    if head.starts_with("post /profiles/prof_1/login-handoff ") {
        return (200, HANDOFF.into());
    }
    if !head.lines().any(|l| l.trim() == "x-handoff-id: h_1") {
        return (400, r#"{"error":"missing handoff"}"#.into());
    }
    if head.starts_with("get /profiles/prof_1/editor ") {
        (200, r#"{"editorStatus":"editing","version":3}"#.into())
    } else if head.starts_with("post /profiles/prof_1/editor/save ") {
        (200, r#"{"ok":true,"version":4}"#.into())
    } else if head.starts_with("delete /profiles/prof_1/editor ") {
        (200, r#"{"ok":true}"#.into())
    } else {
        (500, "{}".into())
    }
}

#[tokio::test]
async fn edit_status_save_and_exit_send_the_handoff_header() {
    let (client, seen) = fake_gateway(editor_route).await;
    let ed = client
        .profiles()
        .edit("prof_1", Some("refresh login"))
        .await
        .unwrap();
    assert_eq!(
        (ed.url.as_str(), ed.handoff_id.as_str(), ed.start_version),
        ("https://edit.example/h_1", "h_1", 3)
    );
    assert_eq!(
        ed.expires_at
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        1893457800
    );

    let st = ed.status().await.unwrap();
    assert_eq!((st.editor_status, st.version), (EditorStatus::Editing, 3));
    assert_eq!(ed.save_and_close().await.unwrap(), 4);
    ed.exit().await.unwrap();
    let heads: Vec<String> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|h| h.lines().next().unwrap().to_string())
        .collect();
    assert_eq!(
        heads[1..],
        [
            "get /profiles/prof_1/editor http/1.1",
            "post /profiles/prof_1/editor/save http/1.1",
            "delete /profiles/prof_1/editor http/1.1"
        ]
    );
}

#[tokio::test]
async fn expired_not_running_and_failed_save_surface_their_status() {
    for status in [404u16, 409, 502] {
        let (client, seen) = fake_gateway(move |head| {
            if head.starts_with("post /profiles/prof_1/login-handoff ") {
                (200, HANDOFF.into())
            } else {
                (status, r#"{"error":"nope"}"#.into())
            }
        })
        .await;
        let ed = client.profiles().edit("prof_1", None).await.unwrap();
        let err = ed.save_and_close().await.unwrap_err();
        assert_eq!(err.status(), Some(status), "{err}");
        if status != 502 {
            assert_eq!(seen.lock().unwrap().len(), 2, "{status} is not retried");
        }
    }
}

#[tokio::test]
async fn wait_for_save_returns_the_new_version_then_times_out_with_408() {
    let polls = Arc::new(Mutex::new(0u32));
    let p2 = polls.clone();
    let (client, _) = fake_gateway(move |head| {
        if head.starts_with("post /profiles/prof_1/login-handoff ") {
            return (200, HANDOFF.into());
        }
        let mut n = p2.lock().unwrap();
        *n += 1;
        if *n < 3 {
            (200, r#"[{"id":"prof_1","name":"a","version":3}]"#.into())
        } else {
            (200, r#"[{"id":"prof_2","name":"b","version":9},{"id":"prof_1","name":"a","version":4}]"#.into())
        }
    })
    .await;
    let mut ed = client.profiles().edit("prof_1", None).await.unwrap();
    ed.poll_interval = Duration::from_millis(1);
    assert_eq!(
        ed.wait_for_save(Some(Duration::from_secs(5)))
            .await
            .unwrap(),
        4
    );
    assert_eq!(*polls.lock().unwrap(), 3);

    let (client, _) = fake_gateway(|head| {
        if head.starts_with("post /profiles/prof_1/login-handoff ") {
            (200, HANDOFF.into())
        } else {
            (200, r#"[{"id":"prof_1","name":"a","version":3}]"#.into())
        }
    })
    .await;
    let mut ed = client.profiles().edit("prof_1", None).await.unwrap();
    ed.poll_interval = Duration::from_millis(1);
    let err = ed
        .wait_for_save(Some(Duration::from_millis(20)))
        .await
        .unwrap_err();
    assert_eq!(err.status(), Some(408));
    ed.expires_at = std::time::SystemTime::now() - Duration::from_secs(60);
    assert_eq!(
        ed.wait_for_save(None).await.unwrap_err().status(),
        Some(408)
    );
}
