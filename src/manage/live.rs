//! Live query log WebSocket: `GET /api/query-log/live`.
//!
//! - Admission: `handle_inner` has already applied the usual Host/Origin and
//!   fetch-metadata checks. This endpoint also requires one Origin header, a
//!   valid session Cookie and a version 13 upgrade, and reserves one of
//!   [`GLOBAL`] process slots and one of [`PER_SESSION`] slots of that session
//!   before answering 101. Failures are ordinary JSON errors without an upgrade
//!   (403, 401, 400, 429, or 503 while stopping).
//! - Authentication: the first client message, within [`AUTH_TIMEOUT`], must be
//!   `{"type":"auth","binding":…}` equal to the session's in-memory binding,
//!   else close 4401. The binding never travels in the URL and is never logged.
//! - Ownership: [`Hub`] owns every socket task in one JoinSet. An upgraded
//!   socket leaves its HTTP connection permit and the 90 s HTTP lifetime; it is
//!   bounded instead by the slots above, [`MESSAGE_LIMIT`] inbound messages,
//!   [`OUTBOUND_LIMIT`] per push, [`IDLE`] without client messages and
//!   [`SEND_TIMEOUT`] per write.
//! - Closing: 4403 when the session leaves Active (logout, credential change,
//!   realm change, eviction) or expires; 4408 idle; 4400 protocol error; 1003
//!   binary data; 1009 oversized message; 1001 on management shutdown, which
//!   awaits the tasks inside its existing drain bound.
//! - Polling runs on the storage worker through at most [`QUERIES`] concurrent
//!   requests, leaving its bounded control queue to other operations. State
//!   comes from the cached storage status. Manager is locked only to clone
//!   RuntimeServices before the upgrade, never across socket IO.
use super::{ApiError, SESSION_LIFETIME, Shared, Snapshot, error};
use crate::{query_log::ListOptions, runtime_services::RuntimeServices};
use axum::{
    body::Body,
    http::{HeaderMap, StatusCode, header, request::Parts},
    response::Response,
};
use futures_util::{SinkExt, StreamExt};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::{
    sync::{Semaphore, watch},
    task::JoinSet,
    time::{Instant, sleep_until, timeout},
};
use tokio_tungstenite::{
    WebSocketStream,
    tungstenite::{
        Error as WsError, Message,
        handshake::derive_accept_key,
        protocol::{CloseFrame, Role, WebSocketConfig},
    },
};

pub(super) const PATH: &str = "/api/query-log/live";
const GLOBAL: usize = 16;
const PER_SESSION: usize = 2;
const QUERIES: usize = 2;
const MESSAGE_LIMIT: usize = 4096;
const OUTBOUND_LIMIT: usize = 1 << 20;
const AUTH_TIMEOUT: Duration = Duration::from_secs(5);
const IDLE: Duration = Duration::from_secs(30);
const SEND_TIMEOUT: Duration = Duration::from_secs(10);
const CLOSE_TIMEOUT: Duration = Duration::from_secs(1);

type Socket = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;

pub(super) struct Hub {
    /// `None` once management stops: no socket is admitted after the drain starts.
    tasks: Mutex<Option<JoinSet<()>>>,
    stop: watch::Sender<bool>,
    connections: Arc<Semaphore>,
    queries: Arc<Semaphore>,
}

impl Default for Hub {
    fn default() -> Self {
        Self {
            tasks: Mutex::new(Some(JoinSet::new())),
            stop: watch::Sender::new(false),
            connections: Arc::new(Semaphore::new(GLOBAL)),
            queries: Arc::new(Semaphore::new(QUERIES)),
        }
    }
}

impl Hub {
    /// Stop admission and ask every socket to close with 1001. The caller
    /// awaits the returned tasks within its shutdown bound.
    pub(super) fn close(&self) -> JoinSet<()> {
        let tasks = self.tasks.lock().unwrap().take().unwrap_or_default();
        self.stop.send_replace(true);
        tasks
    }
}

/// One of a session's live slots, released when its socket task ends.
struct Slot(Arc<AtomicUsize>);
impl Slot {
    fn reserve(count: &Arc<AtomicUsize>) -> Option<Self> {
        count
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n < PER_SESSION).then_some(n + 1)
            })
            .ok()?;
        Some(Self(count.clone()))
    }
}
impl Drop for Slot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

fn has_token(headers: &HeaderMap, name: header::HeaderName, token: &str) -> bool {
    headers.get_all(name).iter().any(|value| {
        value.to_str().is_ok_and(|value| {
            value
                .split(',')
                .any(|part| part.trim().eq_ignore_ascii_case(token))
        })
    })
}

fn accept_key(headers: &HeaderMap) -> Option<String> {
    let mut keys = headers.get_all(header::SEC_WEBSOCKET_KEY).iter();
    let key = keys.next()?;
    (keys.next().is_none()
        && !key.is_empty()
        && has_token(headers, header::CONNECTION, "upgrade")
        && has_token(headers, header::UPGRADE, "websocket")
        && headers
            .get(header::SEC_WEBSOCKET_VERSION)
            .is_some_and(|v| v == "13"))
    .then(|| derive_accept_key(key.as_bytes()))
}

pub(super) async fn upgrade(
    shared: &Arc<Shared>,
    transport: &Arc<Snapshot>,
    mut parts: Parts,
) -> Result<Response, ApiError> {
    let stopping = || {
        error(
            StatusCode::SERVICE_UNAVAILABLE,
            "STOPPING",
            "Management is stopping",
        )
    };
    if *shared.live.stop.borrow() {
        return Err(stopping());
    }
    // Browsers always send Origin with WebSocket requests; handle_inner has
    // compared its value. Requiring it here rejects non-browser hijacking.
    if parts.headers.get_all(header::ORIGIN).iter().count() != 1 {
        return Err(error(
            StatusCode::FORBIDDEN,
            "ORIGIN",
            "Unexpected Host or Origin",
        ));
    }
    let session = shared
        .find_session(&parts.headers, transport)?
        .ok_or_else(|| error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED", "Sign in required"))?;
    let upgrade_required = || {
        error(
            StatusCode::BAD_REQUEST,
            "WEBSOCKET_REQUIRED",
            "WebSocket upgrade required",
        )
    };
    let accept = accept_key(&parts.headers).ok_or_else(upgrade_required)?;
    let busy = || {
        error(
            StatusCode::TOO_MANY_REQUESTS,
            "LIVE_LIMIT",
            "Too many live query log connections",
        )
    };
    let connection = shared
        .live
        .connections
        .clone()
        .try_acquire_owned()
        .map_err(|_| busy())?;
    let slot = Slot::reserve(&session.live).ok_or_else(busy)?;
    let pending = parts
        .extensions
        .remove::<hyper::upgrade::OnUpgrade>()
        .ok_or_else(upgrade_required)?;
    let services = shared.manager.lock().await.services.clone();
    let live = Live {
        // Only a receiver: the Session copy below drops at return, so removal
        // from Active closes this channel.
        revoked: session.revoked.subscribe(),
        binding: session.binding.clone(),
        expires: Instant::from_std(session.created + SESSION_LIFETIME),
        services,
        queries: shared.live.queries.clone(),
        stop: shared.live.stop.subscribe(),
    };
    drop(session);
    {
        let mut tasks = shared.live.tasks.lock().unwrap();
        let tasks = tasks.as_mut().ok_or_else(stopping)?;
        while tasks.try_join_next().is_some() {}
        tasks.spawn(async move {
            let _reserved = (connection, slot);
            let Ok(Ok(upgraded)) = timeout(AUTH_TIMEOUT, pending).await else {
                return;
            };
            let config = WebSocketConfig::default()
                .max_message_size(Some(MESSAGE_LIMIT))
                .max_frame_size(Some(MESSAGE_LIMIT));
            let socket = WebSocketStream::from_raw_socket(
                TokioIo::new(upgraded),
                Role::Server,
                Some(config),
            )
            .await;
            live.run(socket).await;
        });
    }
    Ok(Response::builder()
        .status(StatusCode::SWITCHING_PROTOCOLS)
        .header(header::CONNECTION, "upgrade")
        .header(header::UPGRADE, "websocket")
        .header(header::SEC_WEBSOCKET_ACCEPT, accept)
        .body(Body::empty())
        .expect("static WebSocket response"))
}

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
enum Incoming {
    Auth {
        binding: String,
    },
    Subscribe {
        interval: Option<u64>,
        cursor: Option<u64>,
        #[serde(default)]
        filter: Filter,
    },
    Refresh {},
    Ping {},
}

#[derive(Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Filter {
    search: Option<String>,
    status: Option<String>,
    cache: Option<String>,
}

struct Subscription {
    interval: Option<Duration>,
    cursor: Option<u64>,
    filter: ListOptions,
}

impl Subscription {
    fn parse(interval: Option<u64>, cursor: Option<u64>, filter: Filter) -> Option<Self> {
        let filter = ListOptions {
            search: filter.search,
            status: filter.status,
            cache: filter.cache,
            ..Default::default()
        };
        (interval.is_none_or(|seconds| matches!(seconds, 1 | 3 | 5 | 10))
            && cursor.is_none_or(|id| id <= i64::MAX as u64)
            && filter.validate().is_ok())
        .then(|| Self {
            interval: interval.map(Duration::from_secs),
            cursor,
            filter,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct State {
    log: &'static str,
    log_epoch: u64,
}

impl State {
    fn current(services: &RuntimeServices) -> Self {
        let status = services.status();
        let log = if !services.storage.log_enabled() {
            "disabled"
        } else {
            match status.health.as_str() {
                "starting" | "checking" => "checking",
                "unavailable" => "unavailable",
                _ => "ok",
            }
        };
        Self {
            log,
            log_epoch: status.log_epoch,
        }
    }
}

/// Close code to send, or `None` when the peer is gone or already closed.
type End = Option<u16>;

fn read_end(error: WsError) -> End {
    match error {
        WsError::Capacity(_) => Some(1009),
        WsError::Protocol(_) | WsError::Utf8(_) => Some(1002),
        _ => None,
    }
}

async fn send(socket: &mut Socket, text: String) -> Result<(), End> {
    match timeout(SEND_TIMEOUT, socket.send(Message::text(text))).await {
        Ok(Ok(())) => Ok(()),
        _ => Err(None),
    }
}

struct Live {
    binding: String,
    revoked: watch::Receiver<()>,
    expires: Instant,
    services: Arc<RuntimeServices>,
    queries: Arc<Semaphore>,
    stop: watch::Receiver<bool>,
}

impl Live {
    async fn run(mut self, mut socket: Socket) {
        let mut stop = self.stop.clone();
        let end = tokio::select! {
            biased;
            _ = stop.wait_for(|stopping| *stopping) => Some(1001),
            end = self.serve(&mut socket) => end,
        };
        if let Some(code) = end {
            let frame = CloseFrame {
                code: code.into(),
                reason: "".into(),
            };
            let _ = timeout(CLOSE_TIMEOUT, socket.close(Some(frame))).await;
        }
    }

    async fn serve(&mut self, socket: &mut Socket) -> End {
        match timeout(AUTH_TIMEOUT, socket.next()).await {
            Ok(Some(Ok(Message::Text(text)))) => match serde_json::from_str(&text) {
                Ok(Incoming::Auth { binding }) if binding == self.binding => {}
                _ => return Some(4401),
            },
            Ok(None) => return None,
            Ok(Some(Err(error))) => return read_end(error),
            Ok(Some(Ok(_))) | Err(_) => return Some(4401),
        }
        if self.revoked.has_changed().is_err() {
            return Some(4403);
        }
        let mut idle = Instant::now() + IDLE;
        let mut subscription: Option<Subscription> = None;
        let mut sent: Option<State> = None;
        let mut next: Option<Instant> = None;
        loop {
            let refresh = tokio::select! {
                biased;
                _ = self.revoked.changed() => return Some(4403),
                _ = sleep_until(self.expires) => return Some(4403),
                _ = sleep_until(idle) => return Some(4408),
                _ = sleep_until(next.unwrap_or(idle)), if next.is_some() => false,
                message = socket.next() => {
                    idle = Instant::now() + IDLE;
                    let text = match message {
                        None => return None,
                        Some(Err(error)) => return read_end(error),
                        Some(Ok(Message::Text(text))) => text,
                        Some(Ok(Message::Ping(_) | Message::Pong(_))) => {
                            // Flush the automatic Pong reply.
                            match timeout(SEND_TIMEOUT, socket.flush()).await {
                                Ok(Ok(())) => continue,
                                _ => return None,
                            }
                        }
                        Some(Ok(Message::Close(_))) => {
                            let _ = timeout(CLOSE_TIMEOUT, socket.flush()).await;
                            return None;
                        }
                        Some(Ok(_)) => return Some(1003),
                    };
                    match serde_json::from_str(&text) {
                        Ok(Incoming::Ping {}) => {
                            if let Err(end) = send(socket, r#"{"type":"pong"}"#.into()).await {
                                return end;
                            }
                            continue;
                        }
                        Ok(Incoming::Subscribe { interval, cursor, filter }) => {
                            let Some(next_subscription) =
                                Subscription::parse(interval, cursor, filter)
                            else {
                                return Some(4400);
                            };
                            sent = None;
                            if next_subscription.interval.is_none() {
                                // Manual mode reports state now and entries only on refresh.
                                next = None;
                                subscription = Some(next_subscription);
                                let state = State::current(&self.services);
                                if let Err(end) = self.send_state(socket, state, &mut sent).await {
                                    return end;
                                }
                                continue;
                            }
                            subscription = Some(next_subscription);
                            false
                        }
                        Ok(Incoming::Refresh {}) if subscription.is_some() => true,
                        _ => return Some(4400),
                    }
                }
            };
            let subscription = subscription
                .as_mut()
                .expect("polling requires a subscription");
            if let Err(end) = self.poll(socket, subscription, &mut sent, refresh).await {
                return end;
            }
            next = subscription
                .interval
                .map(|interval| Instant::now() + interval);
        }
    }

    async fn send_state(
        &self,
        socket: &mut Socket,
        state: State,
        sent: &mut Option<State>,
    ) -> Result<(), End> {
        if *sent == Some(state) {
            return Ok(());
        }
        send(
            socket,
            serde_json::json!({"type":"state","log":state.log,"log_epoch":state.log_epoch})
                .to_string(),
        )
        .await?;
        *sent = Some(state);
        Ok(())
    }

    /// Report a changed state, then entries newer than the cursor: only when
    /// there are some, or always for an explicit refresh.
    async fn poll(
        &self,
        socket: &mut Socket,
        subscription: &mut Subscription,
        sent: &mut Option<State>,
        refresh: bool,
    ) -> Result<(), End> {
        let mut state = State::current(&self.services);
        let mut newer = None;
        if state.log == "ok" {
            let _permit = self.queries.acquire().await.map_err(|_| None)?;
            match self
                .services
                .storage
                .newer_logs(subscription.cursor, subscription.filter.clone())
                .await
            {
                Ok(result) => {
                    state.log_epoch = result.log_epoch;
                    newer = Some(result);
                }
                // A full control queue only delays this poll.
                Err(crate::storage::Error::Busy) => {}
                Err(_) => state = State::current(&self.services),
            }
        }
        self.send_state(socket, state, sent).await?;
        let (entries, mut more, scanned_to) = newer.map_or((Vec::new(), false, None), |n| {
            (n.entries, n.more, Some(n.scanned_to))
        });
        // Rows up to scanned_to were checked against the filter; unless the batch
        // was capped (the client then reloads over HTTP), skip past them.
        if !more && let Some(scanned_to) = scanned_to {
            subscription.cursor = Some(subscription.cursor.unwrap_or(0).max(scanned_to));
        }
        if entries.is_empty() && !refresh {
            return Ok(());
        }
        // Newest first: past the byte budget, older entries are left out and
        // reported through `more`, exactly like the count cap.
        let mut body = String::new();
        for entry in &entries {
            let json = serde_json::to_string(entry).map_err(|_| Some(1011))?;
            if !body.is_empty() {
                if body.len() + json.len() > OUTBOUND_LIMIT {
                    more = true;
                    break;
                }
                body.push(',');
            }
            body.push_str(&json);
        }
        if let Some(newest) = entries.first() {
            subscription.cursor = Some(subscription.cursor.unwrap_or(0).max(newest.id));
        }
        let cursor = serde_json::to_string(&subscription.cursor).expect("cursor JSON");
        send(
            socket,
            format!(r#"{{"type":"entries","entries":[{body}],"cursor":{cursor},"more":{more}}}"#),
        )
        .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_tagged_strict_and_bounded() {
        let parse = |text: &str| serde_json::from_str::<Incoming>(text);
        assert!(matches!(parse(r#"{"type":"ping"}"#), Ok(Incoming::Ping {})));
        assert!(matches!(
            parse(r#"{"type":"refresh"}"#),
            Ok(Incoming::Refresh {})
        ));
        assert!(parse(r#"{"type":"ping","extra":1}"#).is_err());
        assert!(parse(r#"{"type":"auth"}"#).is_err());
        assert!(parse(r#"{"binding":"x"}"#).is_err());
        let Ok(Incoming::Subscribe {
            interval,
            cursor,
            filter,
        }) = parse(
            r#"{"type":"subscribe","interval":null,"cursor":7,"filter":{"search":"a","status":null,"cache":"cached"}}"#,
        )
        else {
            panic!("subscribe");
        };
        assert!(Subscription::parse(interval, cursor, filter).is_some());
        assert!(
            parse(r#"{"type":"subscribe","interval":3,"cursor":null,"filter":{"limit":1}}"#)
                .is_err()
        );
        for (interval, cursor, status) in [
            (Some(2), None, None),
            (Some(1), Some(u64::MAX), None),
            (None, None, Some("unknown".to_owned())),
        ] {
            let filter = Filter {
                status,
                ..Default::default()
            };
            assert!(Subscription::parse(interval, cursor, filter).is_none());
        }
    }

    #[test]
    fn handshake_requires_websocket_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(header::CONNECTION, "keep-alive, Upgrade".parse().unwrap());
        headers.insert(header::UPGRADE, "websocket".parse().unwrap());
        headers.insert(header::SEC_WEBSOCKET_VERSION, "13".parse().unwrap());
        assert!(accept_key(&headers).is_none());
        headers.insert(
            header::SEC_WEBSOCKET_KEY,
            "dGhlIHNhbXBsZSBub25jZQ==".parse().unwrap(),
        );
        // RFC 6455 section 1.3 example.
        assert_eq!(
            accept_key(&headers).as_deref(),
            Some("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=")
        );
        headers.append(header::SEC_WEBSOCKET_KEY, "other".parse().unwrap());
        assert!(accept_key(&headers).is_none());
    }

    #[tokio::test]
    async fn global_and_session_limits_and_stop_reject_before_upgrade() {
        use super::super::{Active, Manager, Store, auth_budget};
        let temp = tempfile::tempdir().unwrap();
        let address = "127.0.0.1:3000".parse().unwrap();
        let active = Arc::new(Mutex::new(Active {
            snapshot: Arc::new(Snapshot::initial()),
            sessions: Vec::new(),
        }));
        let manager = Manager::open(
            Store::open(&temp.path().join("state")).unwrap(),
            address,
            active.clone(),
        )
        .await
        .unwrap();
        let shared = Arc::new(Shared {
            filter_tasks: tokio::sync::Mutex::new(JoinSet::new()),
            live: Default::default(),
            manager: Arc::new(tokio::sync::Mutex::new(manager)),
            active,
            auth: auth_budget::Budget::new(),
            mutation: Arc::new(Semaphore::new(1)),
            address,
        });
        let transport = shared.active.lock().unwrap().snapshot.clone();
        let session = shared.session(&transport).unwrap();
        let parts = || {
            let request = axum::http::Request::get(PATH)
                .header(header::ORIGIN, "http://127.0.0.1:3000")
                .header(
                    header::COOKIE,
                    format!("parins_session_http={}", session.token),
                )
                .header(header::CONNECTION, "Upgrade")
                .header(header::UPGRADE, "websocket")
                .header(header::SEC_WEBSOCKET_VERSION, "13")
                .header(header::SEC_WEBSOCKET_KEY, "dGhlIHNhbXBsZSBub25jZQ==")
                .body(())
                .unwrap();
            request.into_parts().0
        };
        let status = |result: Result<Response, ApiError>| result.unwrap_err().0;
        // Without limits the only missing piece is hyper's upgrade handle.
        assert_eq!(
            status(upgrade(&shared, &transport, parts()).await),
            StatusCode::BAD_REQUEST
        );
        let all = shared
            .live
            .connections
            .clone()
            .acquire_many_owned(GLOBAL as u32)
            .await
            .unwrap();
        assert_eq!(
            status(upgrade(&shared, &transport, parts()).await),
            StatusCode::TOO_MANY_REQUESTS
        );
        drop(all);
        session.live.store(PER_SESSION, Ordering::Release);
        assert_eq!(
            status(upgrade(&shared, &transport, parts()).await),
            StatusCode::TOO_MANY_REQUESTS
        );
        session.live.store(0, Ordering::Release);
        assert!(shared.live.close().is_empty());
        assert_eq!(
            status(upgrade(&shared, &transport, parts()).await),
            StatusCode::SERVICE_UNAVAILABLE
        );
    }

    #[test]
    fn session_slots_are_bounded_and_released() {
        let count = Arc::new(AtomicUsize::new(0));
        let first = Slot::reserve(&count).unwrap();
        let _second = Slot::reserve(&count).unwrap();
        assert!(Slot::reserve(&count).is_none());
        drop(first);
        assert!(Slot::reserve(&count).is_some());
    }
}
