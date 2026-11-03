//! The CDP endpoint is a credential: it must not appear in connect errors or in `{:?}` output.
#![cfg(feature = "connect")]

use solari_browser::{connect_endpoint, Session};

#[tokio::test]
async fn a_failed_connect_does_not_print_the_cdp_endpoint() {
    // Nothing listens on port 1.
    let err = match connect_endpoint("ws://127.0.0.1:1/cdp/CDPSECRET?token=TOKSECRET").await {
        Ok(_) => panic!("nothing is listening"),
        Err(e) => e.to_string(),
    };
    assert!(!err.contains("CDPSECRET"), "{err}");
    assert!(!err.contains("TOKSECRET"), "{err}");
    assert!(err.contains("127.0.0.1"), "{err}");
}

#[test]
fn session_debug_does_not_print_the_endpoints() {
    let s = Session {
        id: "SIDSECRET".into(),
        ws_endpoint: "wss://api.getsolari.com/ws/WSSECRET".into(),
        cdp_endpoint: "wss://api.getsolari.com/cdp/CDPSECRET".into(),
        expires_at: "2099-01-01T00:00:00Z".into(),
        storage_state: None,
        proxy: None,
        user_metadata: None,
    };
    let dbg = format!("{s:?}");
    for secret in ["SIDSECRET", "WSSECRET", "CDPSECRET"] {
        assert!(!dbg.contains(secret), "{dbg}");
    }
    assert!(dbg.contains("api.getsolari.com"), "{dbg}");
}
