use super::*;
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::{
    WebSocketStream, client_async,
    tungstenite::{Message, client::IntoClientRequest},
};

type Live = WebSocketStream<TcpStream>;
const PATH: &str = "/api/query-log/live";

fn live_config() -> String {
    format!(
        "{}\n[query_log]\nenabled=true\nmax_entries=100\nretention_secs=60\n[storage]\nflush_interval_ms=100\n[updates]\nauto_check=false\n",
        configuration()
    )
}

/// A raw upgrade request, for rejections that must happen before 101.
fn upgrade_wire(server: &Management, token: Option<&str>, origin: Option<&str>) -> Vec<u8> {
    let mut wire = format!(
        "GET {PATH} HTTP/1.1\r\nHost: {}\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n",
        server.address
    );
    if let Some(origin) = origin {
        wire.push_str(&format!("Origin: {origin}\r\n"));
    }
    if let Some(token) = token {
        let cookie = token.split_once('|').unwrap().0;
        wire.push_str(&format!("Cookie: parins_session_http={cookie}\r\n"));
    }
    wire.push_str("\r\n");
    wire.into_bytes()
}

async fn open(server: &Management, token: &str) -> Live {
    let mut request = format!("ws://{}{PATH}", server.address)
        .into_client_request()
        .unwrap();
    let headers = request.headers_mut();
    headers.insert(
        "origin",
        format!("http://{}", server.address).parse().unwrap(),
    );
    let cookie = token.split_once('|').unwrap().0;
    headers.insert(
        "cookie",
        format!("parins_session_http={cookie}").parse().unwrap(),
    );
    let (socket, response) = timeout(DEADLINE, client_async(request, server.connect().await))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(response.status(), 101);
    socket
}

async fn send(socket: &mut Live, value: Value) {
    socket.send(Message::text(value.to_string())).await.unwrap();
}

async fn authenticated(server: &Management, token: &str) -> Live {
    let mut socket = open(server, token).await;
    let binding = token.split_once('|').unwrap().1;
    send(&mut socket, json!({"type":"auth","binding":binding})).await;
    socket
}

async fn subscribe(socket: &mut Live, interval: Value, cursor: Value, filter: Value) {
    send(
        socket,
        json!({"type":"subscribe","interval":interval,"cursor":cursor,"filter":filter}),
    )
    .await;
}

/// The next JSON message, or the close code as `{"close":code}`.
async fn next(socket: &mut Live, wait: Duration) -> Value {
    timeout(wait, async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Text(text))) => return serde_json::from_str(&text).unwrap(),
                Some(Ok(Message::Close(frame))) => {
                    return json!({"close": frame.map(|f| u16::from(f.code))});
                }
                Some(Ok(_)) => {}
                None | Some(Err(_)) => return json!({"close":null}),
            }
        }
    })
    .await
    .expect("live message deadline")
}

async fn closed(socket: &mut Live, wait: Duration) -> u16 {
    let message = next(socket, wait).await;
    message["close"]
        .as_u64()
        .unwrap_or_else(|| panic!("expected close, got {message}")) as u16
}

async fn pong(socket: &mut Live) {
    send(socket, json!({"type":"ping"})).await;
    assert_eq!(next(socket, DEADLINE).await, json!({"type":"pong"}));
}

async fn resolve(address: SocketAddr, name: &str) {
    let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    socket
        .send_to(&query(name).to_vec().unwrap(), address)
        .await
        .unwrap();
    let mut bytes = [0; 4096];
    timeout(DEADLINE, socket.recv(&mut bytes))
        .await
        .unwrap()
        .unwrap();
}

async fn stored(server: &Management, token: &str, count: u64) {
    timeout(DEADLINE, async {
        loop {
            let status = server
                .request("GET", "/api/status", Some(token), None)
                .await
                .expect(200);
            if status["storage"]["query_log_entries"] == count {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn live_upgrade_requires_same_origin_cookie_and_first_message_binding() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let token = server.setup(&directory, &live_config()).await;
    let origin = format!("http://{}", server.address);
    for (auth, origin, status) in [
        (Some(token.as_str()), Some("http://evil.test"), 403),
        (Some(token.as_str()), Some("https://127.0.0.1"), 403),
        (Some(token.as_str()), None, 403),
        (None, Some(origin.as_str()), 401),
    ] {
        let response = server.raw(&upgrade_wire(&server, auth, origin)).await;
        assert_eq!(response.status, status, "{}", response.body);
        assert!(!response.headers.contains("sec-websocket-accept"));
    }
    let plain = String::from_utf8(upgrade_wire(&server, Some(&token), Some(&origin)))
        .unwrap()
        .replace("Upgrade: websocket\r\n", "");
    server.raw(plain.as_bytes()).await.expect(400);

    let mut wrong = open(&server, &token).await;
    send(&mut wrong, json!({"type":"auth","binding":"wrong"})).await;
    assert_eq!(closed(&mut wrong, DEADLINE).await, 4401);
    let mut missing = open(&server, &token).await;
    send(&mut missing, json!({"type":"ping"})).await;
    assert_eq!(closed(&mut missing, DEADLINE).await, 4401);
    let started = std::time::Instant::now();
    let mut late = open(&server, &token).await;
    assert_eq!(closed(&mut late, DEADLINE).await, 4401);
    assert!(started.elapsed() >= Duration::from_millis(4900));

    let mut socket = authenticated(&server, &token).await;
    pong(&mut socket).await;
    send(&mut socket, json!({"type":"refresh"})).await;
    assert_eq!(closed(&mut socket, DEADLINE).await, 4400);
    server.finish().await;
}

#[tokio::test]
async fn live_interval_and_manual_modes_push_filtered_entries_after_the_cursor() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let token = server.setup(&directory, &live_config()).await;
    let dns = server.dns(&token).await;
    resolve(dns, "example.test.").await;
    resolve(dns, "example.test.").await;
    stored(&server, &token, 2).await;
    let page = server
        .request("POST", "/api/query-log/list", Some(&token), Some(json!({})))
        .await
        .expect(200);
    let newest = page["page"]["entries"][0]["id"].as_u64().unwrap();
    let oldest = page["page"]["entries"][1]["id"].as_u64().unwrap();

    let mut auto = authenticated(&server, &token).await;
    subscribe(
        &mut auto,
        json!(1),
        json!(oldest),
        json!({"status":"blocked","search":null,"cache":null}),
    )
    .await;
    assert_eq!(
        next(&mut auto, DEADLINE).await,
        json!({"type":"state","log":"ok","log_epoch":0})
    );
    let first = next(&mut auto, DEADLINE).await;
    assert_eq!(first["type"], "entries");
    assert_eq!(first["cursor"], newest);
    assert_eq!(first["more"], false);
    let entries = first["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["id"], newest);
    assert_eq!(entries[0]["name"], "example.test.");

    let mut manual = authenticated(&server, &token).await;
    subscribe(&mut manual, Value::Null, json!(newest), json!({})).await;
    assert_eq!(next(&mut manual, DEADLINE).await["type"], "state");

    // The upstream failure does not match the blocked filter.
    resolve(dns, "other.test.").await;
    resolve(dns, "example.test.").await;
    stored(&server, &token, 4).await;
    let pushed = next(&mut auto, DEADLINE).await;
    assert_eq!(pushed["type"], "entries", "{pushed}");
    let entries = pushed["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 1, "{pushed}");
    assert_eq!(entries[0]["status"], "blocked");
    assert!(entries[0]["id"].as_u64().unwrap() > newest);
    assert_eq!(pushed["cursor"], entries[0]["id"]);

    // Manual mode is silent across interval boundaries until refresh.
    sleep(Duration::from_millis(1500)).await;
    pong(&mut manual).await;
    send(&mut manual, json!({"type":"refresh"})).await;
    let refreshed = next(&mut manual, DEADLINE).await;
    assert_eq!(refreshed["type"], "entries");
    let ids: Vec<u64> = refreshed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["id"].as_u64().unwrap())
        .collect();
    assert_eq!(ids.len(), 2);
    assert!(ids[0] > ids[1] && ids[1] > newest);
    assert_eq!(refreshed["cursor"], ids[0]);
    send(&mut manual, json!({"type":"refresh"})).await;
    assert_eq!(
        next(&mut manual, DEADLINE).await,
        json!({"type":"entries","entries":[],"cursor":ids[0],"more":false})
    );

    // Clearing the log is reported as a new epoch on the next poll.
    server
        .request(
            "POST",
            "/api/query-log/clear",
            Some(&token),
            Some(json!({"revision":1,"log_epoch":0})),
        )
        .await
        .expect(200);
    assert_eq!(
        next(&mut auto, DEADLINE).await,
        json!({"type":"state","log":"ok","log_epoch":1})
    );
    server.finish().await;
}

#[tokio::test]
async fn live_sockets_are_limited_per_session_and_close_on_logout_rotation_and_shutdown() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let first = server.setup(&directory, &live_config()).await;
    let second = server
        .request(
            "POST",
            "/api/login",
            None,
            Some(json!({"username":"admin","password":PASSWORD})),
        )
        .await
        .auth();
    let origin = format!("http://{}", server.address);
    let mut a = authenticated(&server, &first).await;
    let mut b = authenticated(&server, &first).await;
    let response = server
        .raw(&upgrade_wire(&server, Some(&first), Some(&origin)))
        .await;
    assert_eq!(response.status, 429, "{}", response.body);
    let mut c = authenticated(&server, &second).await;
    pong(&mut a).await;
    pong(&mut c).await;

    server
        .request("POST", "/api/logout", Some(&first), Some(json!({})))
        .await
        .expect(200);
    assert_eq!(closed(&mut a, DEADLINE).await, 4403);
    assert_eq!(closed(&mut b, DEADLINE).await, 4403);
    pong(&mut c).await;
    let mut d = authenticated(&server, &second).await;
    pong(&mut d).await;

    server
        .request(
            "POST",
            "/api/account/credentials",
            Some(&second),
            Some(json!({"current_password":PASSWORD,"username":"admin","new_password":"rotated-integration-password"})),
        )
        .await
        .expect(200);
    assert_eq!(closed(&mut c, DEADLINE).await, 4403);
    assert_eq!(closed(&mut d, DEADLINE).await, 4403);

    let third = server
        .request(
            "POST",
            "/api/login",
            None,
            Some(json!({"username":"admin","password":"rotated-integration-password"})),
        )
        .await
        .auth();
    let mut e = authenticated(&server, &third).await;
    pong(&mut e).await;
    let started = std::time::Instant::now();
    let finishing = tokio::spawn(server.finish());
    assert_eq!(closed(&mut e, DEADLINE).await, 1001);
    assert!(started.elapsed() < Duration::from_secs(2));
    finishing.await.unwrap();
}

#[tokio::test]
async fn live_socket_without_client_messages_closes_after_idle_timeout() {
    let temporary = tempfile::tempdir().unwrap();
    let directory = temporary.path().join("state");
    let server = Management::start(&directory).await;
    let token = server.setup(&directory, &live_config()).await;
    let mut idle = authenticated(&server, &token).await;
    subscribe(&mut idle, Value::Null, Value::Null, json!({})).await;
    assert_eq!(next(&mut idle, DEADLINE).await["type"], "state");
    let mut active = authenticated(&server, &token).await;
    let started = std::time::Instant::now();
    let pinging = async {
        for _ in 0..4 {
            sleep(Duration::from_secs(9)).await;
            pong(&mut active).await;
        }
    };
    let (code, ()) = tokio::join!(closed(&mut idle, Duration::from_secs(40)), pinging);
    assert_eq!(code, 4408);
    let elapsed = started.elapsed();
    assert!(elapsed >= Duration::from_secs(29), "{elapsed:?}");
    pong(&mut active).await;
    server.finish().await;
}
