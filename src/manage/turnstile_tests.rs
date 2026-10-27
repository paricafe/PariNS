use super::*;
use crate::manage::{Active, ApiRequest, Manager, api, auth_budget, store::Stored};
use axum::http::{HeaderValue, header};
use std::sync::Mutex as StdMutex;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::Semaphore,
    task::JoinSet,
};

const PASSWORD: &str = "local-original-password";
const LOCAL: &str = "127.0.0.1:40000";
/// A distinct non-loopback peer per call, for calls not about the attempt budget.
const ANY: &str = "any";

fn peer(text: &str) -> SocketAddr {
    use std::sync::atomic::{AtomicU32, Ordering};
    static NEXT: AtomicU32 = AtomicU32::new(1);
    if text == ANY {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        SocketAddr::from(([198, 18, (n >> 8) as u8, n as u8], 1))
    } else {
        text.parse().unwrap()
    }
}

async fn fixture() -> (tempfile::TempDir, Arc<Shared>) {
    let temp = tempfile::tempdir().unwrap();
    let address = "127.0.0.1:3000".parse().unwrap();
    let store = Store::open(&temp.path().join("state")).unwrap();
    store
        .save(&Stored {
            username: "admin".into(),
            password_hash: store::hash_password(PASSWORD).unwrap(),
            toml: "listen='127.0.0.1:0'\nquery_timeout_ms=200\ntcp_io_timeout_ms=500\nshutdown_grace_ms=100\nmax_inflight=16\nmax_tcp_connections=8\n[upstreams]\nservers=['127.0.0.1:9']\n".into(),
            previous: None,
            revision: 1,
        })
        .unwrap();
    let active = Arc::new(StdMutex::new(Active {
        snapshot: Arc::new(Snapshot::initial()),
        sessions: Vec::new(),
    }));
    let manager = Arc::new(tokio::sync::Mutex::new(
        Manager::open(store, address, active.clone()).await.unwrap(),
    ));
    let shared = Arc::new(Shared {
        filter_tasks: tokio::sync::Mutex::new(JoinSet::new()),
        live: Default::default(),
        manager,
        active,
        auth: auth_budget::Budget::new(),
        mutation: Arc::new(Semaphore::new(1)),
        address,
    });
    (temp, shared)
}

fn host() -> HeaderMap {
    let mut headers = HeaderMap::new();
    headers.insert(header::HOST, HeaderValue::from_static("127.0.0.1:3000"));
    headers
}

/// A signed-in administrator, as after an ordinary sign-in.
fn signed_in(shared: &Shared) -> HeaderMap {
    let transport = shared.active.lock().unwrap().snapshot.clone();
    let session = shared.session(&transport).unwrap();
    let mut headers = host();
    headers.insert(
        header::COOKIE,
        HeaderValue::from_str(&format!("parins_session_http={}", session.token)).unwrap(),
    );
    headers.insert(
        "x-parins-session",
        HeaderValue::from_str(&session.binding).unwrap(),
    );
    headers
}

async fn call(
    shared: &Arc<Shared>,
    peer_text: &str,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: Value,
) -> Result<Value, ApiError> {
    let transport = shared.active.lock().unwrap().snapshot.clone();
    let mut cookie = None;
    let bytes = serde_json::to_vec(&body).unwrap();
    api(
        shared.clone(),
        transport,
        ApiRequest {
            peer: peer(peer_text),
            method,
            path,
            headers,
            body: &bytes,
        },
        &mut cookie,
    )
    .await
}

async fn ok(
    shared: &Arc<Shared>,
    peer: &str,
    headers: &HeaderMap,
    method: &str,
    path: &str,
    body: Value,
) -> Value {
    call(shared, peer, headers, method, path, body)
        .await
        .unwrap_or_else(|error| panic!("{path}: {error:?}"))
}

async fn set(shared: &Arc<Shared>, setting: Value) -> Result<Value, ApiError> {
    let headers = signed_in(shared);
    call(
        shared,
        ANY,
        &headers,
        "POST",
        "/api/account/turnstile",
        json!({"current_password": PASSWORD, "turnstile": setting}),
    )
    .await
}

async fn login(shared: &Arc<Shared>, peer: &str, token: Option<&str>) -> Result<Value, ApiError> {
    let mut body = json!({"username": "admin", "password": PASSWORD});
    if let Some(token) = token {
        body["turnstile_token"] = json!(token);
    }
    call(shared, peer, &host(), "POST", "/api/login", body).await
}

#[derive(Clone)]
enum Reply {
    Json(&'static str),
    Status(u16),
    Hang,
}

/// A local stand-in for Cloudflare siteverify, recording request bodies.
async fn siteverify(reply: Arc<StdMutex<Reply>>) -> (SocketAddr, Arc<StdMutex<Vec<String>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let bodies = Arc::new(StdMutex::new(Vec::new()));
    let seen = bodies.clone();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let reply = reply.lock().unwrap().clone();
            let seen = seen.clone();
            tokio::spawn(async move {
                let mut request = Vec::new();
                let mut buffer = [0u8; 4096];
                loop {
                    let read = socket.read(&mut buffer).await.unwrap_or(0);
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request).to_string();
                    if let Some((head, body)) = text.split_once("\r\n\r\n") {
                        let length = head
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length: ")
                                    .and_then(|value| value.trim().parse::<usize>().ok())
                            })
                            .unwrap_or(0);
                        if body.len() >= length {
                            seen.lock().unwrap().push(body.to_owned());
                            break;
                        }
                    }
                    if read == 0 {
                        return;
                    }
                }
                let response = match reply {
                    Reply::Json(body) => format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                        body.len()
                    ),
                    Reply::Status(status) => format!(
                        "HTTP/1.1 {status} Error\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    ),
                    Reply::Hang => {
                        tokio::time::sleep(Duration::from_secs(30)).await;
                        return;
                    }
                };
                let _ = socket.write_all(response.as_bytes()).await;
            });
        }
    });
    (address, bodies)
}

#[tokio::test]
async fn turnstile_is_checked_before_the_password_except_from_loopback() {
    let (temp, shared) = fixture().await;
    let reply = Arc::new(StdMutex::new(Reply::Json(
        r#"{"success":true,"hostname":"127.0.0.1","action":"login"}"#,
    )));
    let (address, bodies) = siteverify(reply.clone()).await;
    shared.manager.lock().await.turnstile.siteverify = Some(address);
    let state = std::fs::read(temp.path().join("state/state.json")).unwrap();
    let other = signed_in(&shared);
    set(
        &shared,
        json!({"site_key": "0x4AAAAAAA-site", "secret_key": "0x4AAAAAAA-secret"}),
    )
    .await
    .unwrap();
    // A credential change: every session ended; the configuration envelope stays.
    assert!(shared.active.lock().unwrap().sessions.is_empty());
    assert_eq!(
        call(&shared, ANY, &other, "GET", "/api/account", json!(null))
            .await
            .unwrap_err()
            .0,
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        std::fs::read(temp.path().join("state/state.json")).unwrap(),
        state
    );
    assert_eq!(
        read(&temp.path().join("state"))
            .unwrap()
            .unwrap()
            .secret_key,
        "0x4AAAAAAA-secret"
    );
    let headers = signed_in(&shared);
    let view = ok(
        &shared,
        ANY,
        &headers,
        "GET",
        "/api/account/turnstile",
        json!(null),
    )
    .await;
    assert_eq!(
        view,
        json!({"turnstile": {"site_key": "0x4AAAAAAA-site", "secret_set": true, "test_keys": null},
            "loopback": false})
    );
    let config = ok(&shared, ANY, &headers, "GET", "/api/config", json!(null)).await;
    assert!(!format!("{view}{config}").contains("0x4AAAAAAA-secret"));

    let remote = "192.0.2.40:1";
    let options = ok(
        &shared,
        remote,
        &host(),
        "GET",
        "/api/login/options",
        json!(null),
    )
    .await;
    assert_eq!(
        options,
        json!({"turnstile": {"site_key": "0x4AAAAAAA-site", "action": "login"}, "turnstile_skipped": false})
    );
    let transport = shared.active.lock().unwrap().snapshot.clone();
    assert!(document_needs_widget(&shared, &transport, peer(remote), &host()).await);
    assert!(!document_needs_widget(&shared, &transport, peer(remote), &headers).await);
    assert!(!document_needs_widget(&shared, &transport, peer(LOCAL), &host()).await);

    assert_eq!(
        login(&shared, remote, None).await.unwrap_err().1,
        "TURNSTILE_REQUIRED"
    );
    assert_eq!(
        login(&shared, remote, Some("token-1")).await.unwrap()["authenticated"],
        true
    );
    let body = bodies.lock().unwrap().last().cloned().unwrap();
    assert!(body.contains("secret=0x4AAAAAAA-secret") && body.contains("response=token-1"));

    // Failures and outages are refused before any password check.
    let other = "192.0.2.41:1";
    for (answer, expected) in [
        (
            Reply::Json(r#"{"success":false,"error-codes":["invalid-input-response"]}"#),
            "TURNSTILE_FAILED",
        ),
        (
            Reply::Json(r#"{"success":true,"hostname":"evil.example","action":"login"}"#),
            "TURNSTILE_FAILED",
        ),
        (
            Reply::Json(r#"{"success":true,"hostname":"127.0.0.1","action":"other"}"#),
            "TURNSTILE_FAILED",
        ),
        (Reply::Status(500), "TURNSTILE_UNAVAILABLE"),
        (Reply::Hang, "TURNSTILE_UNAVAILABLE"),
    ] {
        *reply.lock().unwrap() = answer;
        assert_eq!(
            login(&shared, other, Some("token")).await.unwrap_err().1,
            expected
        );
    }
    // They spent this peer's attempts.
    *reply.lock().unwrap() =
        Reply::Json(r#"{"success":true,"hostname":"127.0.0.1","action":"login"}"#);
    assert_eq!(
        login(&shared, other, Some("token")).await.unwrap_err().1,
        "LOGIN_LIMIT"
    );

    // A loopback socket peer (an SSH tunnel) is not asked, even if Cloudflare is down.
    *reply.lock().unwrap() = Reply::Status(503);
    let count = bodies.lock().unwrap().len();
    for loopback in [LOCAL, "[::1]:4000", "[::ffff:127.0.0.1]:4000"] {
        let options = ok(
            &shared,
            loopback,
            &host(),
            "GET",
            "/api/login/options",
            json!(null),
        )
        .await;
        assert_eq!(
            options,
            json!({"turnstile": null, "turnstile_skipped": true})
        );
        assert!(login(&shared, loopback, None).await.is_ok());
    }
    assert_eq!(bodies.lock().unwrap().len(), count);
    let headers = signed_in(&shared);
    assert_eq!(
        ok(
            &shared,
            LOCAL,
            &headers,
            "GET",
            "/api/account/turnstile",
            json!(null)
        )
        .await["loopback"],
        true
    );

    // Keeping the saved secret while changing the site key.
    set(&shared, json!({"site_key": "0x4AAAAAAA-other"}))
        .await
        .unwrap();
    assert_eq!(
        read(&temp.path().join("state"))
            .unwrap()
            .unwrap()
            .secret_key,
        "0x4AAAAAAA-secret"
    );
    set(&shared, json!(null)).await.unwrap();
    assert!(!temp.path().join("state/turnstile.json").exists());
    assert!(login(&shared, "192.0.2.44:1", None).await.is_ok());
    shared.manager.lock().await.terminal_shutdown().await;
}

#[tokio::test]
async fn changes_need_the_password_and_a_turnstile_change_voids_sign_ins_in_flight() {
    let (_temp, shared) = fixture().await;
    let headers = signed_in(&shared);
    let wrong = call(
        &shared,
        ANY,
        &headers,
        "POST",
        "/api/account/turnstile",
        json!({"current_password": "wrong-password-value", "turnstile": {"site_key": "a", "secret_key": "b"}}),
    )
    .await
    .unwrap_err();
    assert_eq!(wrong.1, "CURRENT_PASSWORD_INCORRECT");
    assert_eq!(shared.active.lock().unwrap().sessions.len(), 1);
    assert!(shared.manager.lock().await.turnstile.keys.is_none());
    // A sign-in admitted before the change cannot finish after it.
    let admitted = admission(&shared).await;
    set(&shared, json!({"site_key": "a", "secret_key": "b"}))
        .await
        .unwrap();
    assert_ne!(
        admitted.generation,
        shared.manager.lock().await.turnstile.generation
    );
    shared.manager.lock().await.terminal_shutdown().await;
}

#[tokio::test]
async fn cloudflare_test_keys_are_accepted_only_as_a_pair() {
    let (temp, shared) = fixture().await;
    const TEST_SITE: &str = "1x00000000000000000000AA";
    const PASS: &str = "1x0000000000000000000000000000000AA";
    const FAIL: &str = "2x0000000000000000000000000000000AA";
    for (site, secret) in [
        ("0x4AAAAAAA-site", PASS),
        ("0x4AAAAAAA-site", FAIL),
        (TEST_SITE, "0x4AAAAAAA-secret"),
    ] {
        assert_eq!(
            set(&shared, json!({"site_key": site, "secret_key": secret}))
                .await
                .unwrap_err()
                .1,
            "TURNSTILE_TEST_KEYS"
        );
    }
    set(&shared, json!({"site_key": TEST_SITE, "secret_key": PASS}))
        .await
        .unwrap();
    let view = ok(
        &shared,
        ANY,
        &signed_in(&shared),
        "GET",
        "/api/account/turnstile",
        json!(null),
    )
    .await;
    assert_eq!(view["turnstile"]["test_keys"], "pass");
    // Keeping the test secret still needs a test site key.
    assert_eq!(
        set(&shared, json!({"site_key": "0x4AAAAAAA-site"}))
            .await
            .unwrap_err()
            .1,
        "TURNSTILE_TEST_KEYS"
    );
    set(
        &shared,
        json!({"site_key": "3x00000000000000000000FF", "secret_key": FAIL}),
    )
    .await
    .unwrap();
    let view = ok(
        &shared,
        ANY,
        &signed_in(&shared),
        "GET",
        "/api/account/turnstile",
        json!(null),
    )
    .await;
    assert_eq!(view["turnstile"]["test_keys"], "fail");
    // An unpaired file is rejected at startup and by the managed check.
    let store = shared.manager.lock().await.store.clone();
    store
        .atomic_write(
            FILE,
            format!(r#"{{"site_key":"real","secret_key":"{PASS}"}}"#).as_bytes(),
        )
        .unwrap();
    assert!(read(&temp.path().join("state")).is_err());
    for text in [
        r#"{"site_key":"","secret_key":"x"}"#,
        r#"{"site_key":"a"}"#,
        r#"{"site_key":"a","secret_key":"b","extra":1}"#,
    ] {
        store.atomic_write(FILE, text.as_bytes()).unwrap();
        assert!(read(&temp.path().join("state")).is_err(), "{text}");
    }
    shared.manager.lock().await.terminal_shutdown().await;
}

#[tokio::test]
async fn only_a_configured_test_secret_relaxes_the_host_and_action_checks() {
    let (_temp, shared) = fixture().await;
    let reply = Arc::new(StdMutex::new(Reply::Json(
        r#"{"success":true,"hostname":"example.com","action":"","metadata":{"result_with_testing_key":true}}"#,
    )));
    let (address, _) = siteverify(reply).await;
    shared.manager.lock().await.turnstile.siteverify = Some(address);
    set(
        &shared,
        json!({"site_key": "0x4AAAAAAA-site", "secret_key": "0x4AAAAAAA-secret"}),
    )
    .await
    .unwrap();
    assert_eq!(
        login(&shared, ANY, Some("token")).await.unwrap_err().1,
        "TURNSTILE_FAILED"
    );
    set(
        &shared,
        json!({"site_key": "1x00000000000000000000AA", "secret_key": "1x0000000000000000000000000000000AA"}),
    )
    .await
    .unwrap();
    assert_eq!(
        login(&shared, ANY, Some("token")).await.unwrap()["authenticated"],
        true
    );
    shared.manager.lock().await.terminal_shutdown().await;
}
