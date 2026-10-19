//! `/cable`: the Action Cable WebSocket protocol (actioncable-v1-json),
//! enough of it for the gem's frontend (`@rails/actioncable` 7.2, hooks/
//! use-live.ts) to work unmodified:
//!
//! - the handshake selects subprotocol `actioncable-v1-json`; a request
//!   from another origin, or without a valid signed `session_token` cookie,
//!   gets `{"type":"disconnect","reason":"unauthorized","reconnect":false}`
//!   and is closed (ApplicationCable::Connection#connect);
//! - `{"type":"welcome"}`, then `{"type":"ping","message":<epoch s>}` every
//!   3 s (the client drops a connection silent for 6 s);
//! - `{"command":"subscribe","identifier":"{\"channel\":\"EnvironmentChannel\",\"id\":N}"}`
//!   is confirmed (`confirm_subscription`) when the session still exists
//!   and its user is a member of the environment's account, rejected
//!   otherwise (EnvironmentChannel#subscribed); `unsubscribe` stops it;
//! - each broadcast to stream `environment_<id>` (Rails' name; the
//!   frontend never sees it) arrives as `{"identifier":...,"message":
//!   {"event":"ingested","at":...,"counts":{...}}}`, re-authorized before
//!   it is sent (EnvironmentChannel#transmit_authorized).
//!
//! Broadcasts come from the ingest commit path (followups.rs), at most one
//! per environment every 2 s (Ingest::Batch::THROTTLE_WINDOW). They are a
//! "data changed, refetch" ping, not the data.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use axum::extract::State;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::http::HeaderMap;
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use rusqlite::params;
use serde_json::{Value, json};
use tokio::sync::broadcast;

use super::WebState;
use super::cookies;

pub const THROTTLE: Duration = Duration::from_secs(2);
const PING_EVERY: Duration = Duration::from_secs(3);

/// The in-process pub/sub (Solid Cable's role; one process here).
#[derive(Clone)]
pub struct Hub {
    tx: broadcast::Sender<(i64, Arc<str>)>,
    last: Arc<Mutex<HashMap<i64, Instant>>>,
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

impl Hub {
    pub fn new() -> Hub {
        Hub { tx: broadcast::channel(1024).0, last: Default::default() }
    }

    /// Ingest::Batch#broadcast_live: throttled per environment; true when
    /// it was sent.
    pub fn ingested(&self, env_id: i64, counts: &[(String, i64)]) -> bool {
        let now = Instant::now();
        {
            let mut last = self.last.lock().unwrap();
            if last.get(&env_id).is_some_and(|t| now.duration_since(*t) < THROTTLE) {
                return false;
            }
            last.insert(env_id, now);
        }
        let mut c = serde_json::Map::new();
        for (k, n) in counts {
            c.insert(k.clone(), json!(n));
        }
        let at = crate::time::iso8601(crate::time::now_micros().div_euclid(1_000_000));
        let msg = json!({"event": "ingested", "at": at, "counts": c}).to_string();
        // No subscriber is not an error.
        let _ = self.tx.send((env_id, msg.into()));
        true
    }

    pub fn subscribe(&self) -> broadcast::Receiver<(i64, Arc<str>)> {
        self.tx.subscribe()
    }
}

pub async fn upgrade(
    State(state): State<Arc<WebState>>,
    proxied: Option<axum::Extension<super::proxy_auth::Proxied>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let jar = cookies::parse(&headers);
    let session_id: Option<i64> =
        cookies::get(&jar, cookies::SESSION_TOKEN).and_then(|c| state.keys.verify(cookies::SESSION_TOKEN, c)).and_then(|v| v.parse().ok());
    let origin_ok = super::session::same_origin(&headers);
    // Verified (and the header removed) by proxy_auth::layer.
    let proxied = proxied.map(|axum::Extension(p)| p.0);
    let user = match (origin_ok, proxied, session_id) {
        (true, Some(claims), _) => state.db(move |c| c.tx(|tx| super::proxy_auth::resolve(tx, &claims))).await.ok().map(|(s, u)| (s.id, u.id)),
        (true, None, Some(sid)) => state.db(move |c| c.with(|x| crate::control::find_session(x, sid))).await.ok().flatten().map(|s| (s.id, s.user_id)),
        _ => None,
    };
    ws.protocols(["actioncable-v1-json"]).on_upgrade(move |socket| run(socket, state, user))
}

fn text(v: Value) -> Message {
    Message::Text(Utf8Bytes::from(v.to_string()))
}

/// EnvironmentChannel#authorized?
fn authorized(state: &WebState, session_id: i64, user_id: i64, env_id: i64) -> bool {
    state
        .control
        .with(|x| {
            x.query_row(
                "SELECT EXISTS (SELECT 1 FROM sessions WHERE id = ?1 AND user_id = ?2)
                    AND EXISTS (SELECT 1 FROM environments e JOIN applications ap ON ap.id = e.application_id
                                JOIN memberships m ON m.account_id = ap.account_id WHERE e.id = ?3 AND m.user_id = ?2)",
                params![session_id, user_id, env_id],
                |r| r.get::<_, bool>(0),
            )
        })
        .unwrap_or(false)
}

async fn run(socket: WebSocket, state: Arc<WebState>, user: Option<(i64, i64)>) {
    let (mut tx, mut rx) = socket.split();
    let Some((session_id, user_id)) = user else {
        let _ = tx.send(text(json!({"type": "disconnect", "reason": "unauthorized", "reconnect": false}))).await;
        let _ = tx.close().await;
        return;
    };
    if tx.send(text(json!({"type": "welcome"}))).await.is_err() {
        return;
    }
    let mut events = state.cable.subscribe();
    // identifier string -> environment id
    let mut subs: HashMap<String, i64> = HashMap::new();
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ping.tick() => {
                let now = crate::time::now_micros() / 1_000_000;
                if tx.send(text(json!({"type": "ping", "message": now}))).await.is_err() { return; }
            }
            msg = rx.next() => {
                let Some(Ok(msg)) = msg else { return };
                let Message::Text(t) = msg else {
                    if matches!(msg, Message::Close(_)) { return; }
                    continue;
                };
                let Ok(cmd) = serde_json::from_str::<Value>(t.as_str()) else { continue };
                let identifier = cmd["identifier"].as_str().unwrap_or("").to_string();
                let id: Value = serde_json::from_str(&identifier).unwrap_or(Value::Null);
                match cmd["command"].as_str() {
                    Some("subscribe") => {
                        let env = (id["channel"] == "EnvironmentChannel")
                            .then(|| id["id"].as_i64().or_else(|| id["id"].as_str().and_then(|s| s.parse().ok())))
                            .flatten();
                        let st = state.clone();
                        let ok = match env {
                            Some(e) => tokio::task::spawn_blocking(move || authorized(&st, session_id, user_id, e)).await.unwrap_or(false),
                            None => false,
                        };
                        let reply = if ok {
                            subs.insert(identifier.clone(), env.expect("checked"));
                            "confirm_subscription"
                        } else {
                            "reject_subscription"
                        };
                        if tx.send(text(json!({"identifier": identifier, "type": reply}))).await.is_err() { return; }
                    }
                    Some("unsubscribe") => { subs.remove(&identifier); }
                    _ => {}
                }
            }
            ev = events.recv() => {
                let (env, msg) = match ev {
                    Ok(e) => e,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                let targets: Vec<String> = subs.iter().filter(|(_, e)| **e == env).map(|(i, _)| i.clone()).collect();
                if targets.is_empty() { continue; }
                let st = state.clone();
                let still = tokio::task::spawn_blocking(move || authorized(&st, session_id, user_id, env)).await.unwrap_or(false);
                let message: Value = serde_json::from_str(&msg).unwrap_or(Value::Null);
                for identifier in targets {
                    if still {
                        if tx.send(text(json!({"identifier": identifier, "message": message}))).await.is_err() { return; }
                    } else {
                        subs.remove(&identifier);
                        if tx.send(text(json!({"identifier": identifier, "type": "reject_subscription"}))).await.is_err() { return; }
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn broadcasts_are_throttled_per_environment() {
        let hub = Hub::new();
        let mut rx = hub.subscribe();
        assert!(hub.ingested(1, &[("request".into(), 3)]));
        assert!(!hub.ingested(1, &[("request".into(), 1)]), "within 2 s");
        assert!(hub.ingested(2, &[]), "another environment is independent");
        let (env, msg) = rx.try_recv().unwrap();
        assert_eq!(env, 1);
        let v: Value = serde_json::from_str(&msg).unwrap();
        assert_eq!(v["event"], "ingested");
        assert_eq!(v["counts"]["request"], 3);
        assert!(v["at"].as_str().unwrap().ends_with('Z'));
    }
}
