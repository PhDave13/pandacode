//! The one WebSocket to the relay: reconnects with backoff, pings, tracks presence,
//! and seals and opens every frame between this computer and the others.
use super::{auth, config, crypto, emit_presence, emit_status, host, remote, sign_out_local, Device, Inner};
use crate::Result;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::{self, client::IntoClientRequest, Message};

const PING: Duration = Duration::from_secs(30);
const SILENT: Duration = Duration::from_secs(75);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A socket must stay up this long before the backoff resets.
const STABLE: Duration = Duration::from_secs(30);
const MAX_FRAME: usize = 256 * 1024;
const PEER_QUEUE: usize = 64;
const MAX_PEERS: usize = 64;
/// Relay close codes: another socket replaced this one, or the device was revoked.
const REPLACED: u16 = 4000;
const REVOKED: u16 = 4001;

#[derive(Debug, PartialEq)]
enum Outcome {
    Closed { up: Duration, code: Option<u16> },
    Failed,
    Unauthorized,
}

/// (wait now, base for the next failure), or None to sign out.
fn backoff(delay: Duration, outcome: &Outcome) -> Option<(Duration, Duration)> {
    let base = match outcome {
        Outcome::Unauthorized | Outcome::Closed { code: Some(REVOKED), .. } => return None,
        Outcome::Closed { up, .. } if *up >= STABLE => Duration::from_secs(1),
        _ => delay,
    };
    let wait = if matches!(outcome, Outcome::Closed { code: Some(REPLACED), .. }) { base.max(MAX_BACKOFF) } else { base };
    Some((wait, (base * 2).min(MAX_BACKOFF)))
}

/// Starts (or restarts) the socket task while signed in.
pub(crate) fn start(app: &tauri::AppHandle) {
    let generation = {
        let state = remote(app);
        let mut inner = state.lock();
        if inner.token.is_none() {
            return;
        }
        inner.generation += 1;
        inner.outbox = None;
        inner.generation
    };
    tauri::async_runtime::spawn(run(app.clone(), generation));
}

fn current(app: &tauri::AppHandle, generation: u64) -> bool {
    remote(app).lock().generation == generation
}

async fn run(app: tauri::AppHandle, generation: u64) {
    let mut delay = Duration::from_secs(1);
    while current(&app, generation) {
        let Some(token) = remote(&app).lock().token.clone() else { return };
        let outcome = session(&app, generation, &token).await;
        let changed = {
            let state = remote(&app);
            let mut inner = state.lock();
            let changed = inner.generation == generation && inner.connected;
            if inner.generation == generation {
                inner.connected = false;
                inner.outbox = None;
                inner.presence = false;
                inner.fail_pending(None, "The connection to the relay was lost.");
            }
            changed
        };
        let Some((wait, next)) = backoff(delay, &outcome) else {
            if current(&app, generation) {
                sign_out_local(&app, Some(&token));
            }
            return;
        };
        if changed {
            emit_status(&app);
        }
        tokio::time::sleep(wait).await;
        delay = next;
    }
}

async fn session(app: &tauri::AppHandle, generation: u64, token: &str) -> Outcome {
    let Some(build) = config::build() else { return Outcome::Failed };
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
    let hosting = remote(app).lock().settings.host_enabled;
    let url = format!("wss://{}/v1/connect?host={}", build.relay.trim_start_matches("https://"), u8::from(hosting));
    let Ok(mut request) = url.into_client_request() else { return Outcome::Failed };
    let Ok(authorization) = format!("Bearer {token}").parse() else { return Outcome::Unauthorized };
    request.headers_mut().insert("Authorization", authorization);
    let stream = match tokio::time::timeout(Duration::from_secs(15), tokio_tungstenite::connect_async(request)).await {
        Ok(Ok((stream, _))) => stream,
        Ok(Err(tungstenite::Error::Http(response))) if matches!(response.status().as_u16(), 401 | 403) => return Outcome::Unauthorized,
        _ => return Outcome::Failed,
    };
    let opened = Instant::now();
    let (mut write, mut read) = stream.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    let host_enabled = {
        let state = remote(app);
        let mut inner = state.lock();
        if inner.generation != generation {
            return Outcome::Failed;
        }
        inner.outbox = Some(tx);
        inner.connected = true;
        inner.settings.host_enabled
    };
    emit_status(app);
    let closed = |code| Outcome::Closed { up: opened.elapsed(), code };
    // Hosting was toggled while connecting, before the outbox could carry it.
    if host_enabled != hosting && write.send(Message::text(json!({"kind": "host", "enabled": host_enabled}).to_string())).await.is_err() {
        return closed(None);
    }
    // One ordered request queue per peer; dropping the map at the end stops their workers.
    let mut peers: HashMap<String, mpsc::Sender<Request>> = HashMap::new();
    let mut ping = tokio::time::interval_at(tokio::time::Instant::now() + PING, PING);
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            // None once stop() or a restart drops the outbox.
            outgoing = rx.recv() => match outgoing {
                Some(text) => if write.send(Message::text(text)).await.is_err() { return closed(None) },
                None => {
                    let _ = write.send(Message::Close(None)).await;
                    return closed(None);
                }
            },
            _ = ping.tick() => {
                if heard.elapsed() > SILENT || write.send(Message::text("ping")).await.is_err() {
                    return closed(None);
                }
            }
            incoming = read.next() => match incoming {
                Some(Ok(Message::Text(text))) => {
                    heard = Instant::now();
                    frame(app, text.as_str(), &mut peers);
                }
                Some(Ok(Message::Close(close))) => return closed(close.map(|frame| u16::from(frame.code))),
                Some(Err(_)) | None => return closed(None),
                Some(Ok(_)) => heard = Instant::now(),
            },
        }
    }
}

/// An opened request, the pair key to answer with, and whether the caller is trusted here.
type Request = (Value, crypto::Key, bool);

/// One relay frame: presence, delivery failures and errors, or an encrypted message.
/// Responses and events are handled at once; only requests wait in their peer's queue.
fn frame(app: &tauri::AppHandle, text: &str, peers: &mut HashMap<String, mpsc::Sender<Request>>) {
    if text == "pong" {
        return;
    }
    let Ok(value) = serde_json::from_str::<Value>(text) else { return };
    match value["kind"].as_str() {
        Some("presence") => presence(app, &value["devices"]),
        Some("undeliverable") => {
            if let Some(to) = value["to"].as_str() {
                let error = if value["reason"] == "rate" { "Too many requests to that computer. Try again in a moment." } else { "That computer is offline." };
                remote(app).lock().fail_ref(to, value["ref"].as_str(), error);
            }
        }
        Some("error") => eprintln!("PandaCode Connect relay: {}", value["error"].as_str().unwrap_or("error")),
        _ => {
            let (Some(from), Some(to), Some(body)) = (value["from"].as_str(), value["to"].as_str(), value["body"].as_str()) else { return };
            if host::valid_id(from).is_err() {
                return;
            }
            let me = remote(app).device_id();
            if to != me || from == me {
                return;
            }
            let Ok((key, trusted)) = peer_key(app, from) else { return };
            let Ok(plain) = crypto::open(&key, from, to, body) else { return };
            let Ok(message) = serde_json::from_slice::<Value>(&plain) else { return };
            match message["type"].as_str() {
                Some("req") => queue(app, from, (message, key, trusted), peers),
                // Only a host trusted here may answer or report changes.
                Some("res") if trusted => super::client::resolve(app, from, &message),
                Some("event") if trusted && message["name"] == "tasks.changed" => {
                    use tauri::Emitter;
                    for event in task_changes(from, &message["data"]) {
                        let _ = app.emit("remote://task-changed", event);
                    }
                }
                _ => {}
            }
        }
    }
}

fn queue(app: &tauri::AppHandle, from: &str, request: Request, peers: &mut HashMap<String, mpsc::Sender<Request>>) {
    if !peers.contains_key(from) {
        if peers.len() >= MAX_PEERS {
            return;
        }
        let (tx, rx) = mpsc::channel(PEER_QUEUE);
        tauri::async_runtime::spawn(peer(app.clone(), from.to_owned(), rx));
        peers.insert(from.to_owned(), tx);
    }
    if peers[from].try_send(request).is_err() {
        eprintln!("PandaCode Connect: dropped a request from a busy peer");
    }
}

/// Answers one peer's requests strictly in arrival order.
async fn peer(app: tauri::AppHandle, from: String, mut requests: mpsc::Receiver<Request>) {
    while let Some((message, key, trusted)) = requests.recv().await {
        if let Some(response) = host::handle(&app, &from, &message, trusted).await {
            // An untrusted caller only ever gets an error back, never task content.
            if let Err(error) = send_sealed(&app, &from, &key, &response) {
                let failure = json!({"type": "res", "id": response["id"], "ok": false, "error": error});
                let _ = send_sealed(&app, &from, &key, &failure);
            }
        }
    }
}

/// One `remote://task-changed` payload per valid id; the focused task carries its live state.
fn task_changes(from: &str, data: &Value) -> Vec<Value> {
    let focus = &data["focus"];
    let focused = focus["sessionId"].as_str();
    let ids = data["sessionIds"].as_array().map(Vec::as_slice).unwrap_or_default();
    ids.iter()
        .take(500)
        .filter_map(Value::as_str)
        .filter(|id| host::valid_id(id).is_ok())
        .map(|session| {
            let mut event = json!({"deviceId": from, "sessionId": session});
            if focused == Some(session) {
                event["live"] = json!({
                    "liveText": focus["liveText"].as_str().unwrap_or_default(),
                    "status": focus["status"].as_str().unwrap_or_default(),
                    "historyVersion": focus["historyVersion"].as_u64(),
                });
            }
            event
        })
        .collect()
}

/// Applies a presence list, which carries the full device rows.
fn presence(app: &tauri::AppHandle, list: &Value) {
    let Some(list) = list.as_array() else { return };
    apply_presence(&mut remote(app).lock(), list);
    emit_presence(app);
}

/// The list replaces the known devices. A device missing from it is gone, with its subscription.
fn apply_presence(inner: &mut Inner, list: &[Value]) {
    inner.devices = parse_devices(list);
    inner.presence = true;
    let Inner { devices, subscribers, keys, .. } = inner;
    subscribers.retain(|id, _| devices.iter().any(|d| &d.id == id));
    keys.retain(|id, _| devices.iter().any(|d| &d.id == id));
}

fn parse_devices(list: &[Value]) -> Vec<Device> {
    list.iter().filter_map(|d| serde_json::from_value(d.clone()).ok()).filter(|d: &Device| host::valid_id(&d.id).is_ok()).take(64).collect()
}

/// Fetches the account's devices; used only before the socket's first presence frame.
pub(crate) async fn refresh_devices(app: &tauri::AppHandle) -> Result<()> {
    let build = config::build().ok_or("Remote access is not configured in this build.")?;
    let token = remote(app).lock().token.clone().ok_or("Sign in to PandaCode Connect first.")?;
    let reply = match auth::relay(&build, &token, reqwest::Method::GET, "/v1/devices").await {
        Err(error) if error == auth::SIGNED_OUT => {
            sign_out_local(app, Some(&token));
            return Err(error);
        }
        other => other?,
    };
    let devices = parse_devices(reply["devices"].as_array().map(Vec::as_slice).unwrap_or_default());
    {
        let state = remote(app);
        let mut inner = state.lock();
        // Presence that arrived meanwhile is newer.
        if !inner.presence {
            inner.subscribers.retain(|id, _| devices.iter().any(|d| &d.id == id));
            inner.devices = devices;
        }
    }
    emit_presence(app);
    Ok(())
}

/// The pair key with a device and whether it is trusted here (its pinned key is its
/// current key). Keys are cached by (device id, public key).
fn peer_key(app: &tauri::AppHandle, device_id: &str) -> Result<(crypto::Key, bool)> {
    let state = remote(app);
    let (public, trusted) = {
        let inner = state.lock();
        let public = &inner.devices.iter().find(|d| d.id == device_id).filter(|d| !d.public_key.is_empty()).ok_or(super::UNKNOWN_DEVICE)?.public_key;
        let trusted = inner.settings.is_trusted(device_id, public);
        if let Some((_, key)) = inner.keys.get(device_id).filter(|(cached, _)| cached == public) {
            return Ok((*key, trusted));
        }
        (public.clone(), trusted)
    };
    let key = crypto::pair_key(&state.secret()?, &state.device_id(), &public, device_id)?;
    state.lock().keys.insert(device_id.to_owned(), (public, key));
    Ok((key, trusted))
}

/// Seals a message with a known key and queues it on the socket as `{to, body, ref}`.
fn send_sealed(app: &tauri::AppHandle, to: &str, key: &crypto::Key, message: &Value) -> Result<()> {
    let me = remote(app).device_id();
    let body = crypto::seal(key, &me, to, message.to_string().as_bytes())?;
    let mut frame = json!({"to": to, "body": body});
    if let Some(id) = message["id"].as_str() {
        frame["ref"] = json!(id);
    }
    let frame = frame.to_string();
    if frame.len() > MAX_FRAME {
        return Err("That reply is too large to send to another computer.".into());
    }
    let state = remote(app);
    let inner = state.lock();
    let outbox = inner.outbox.as_ref().ok_or("Not connected to the relay.")?;
    outbox.send(frame).map_err(|_| "Not connected to the relay.".into())
}

/// Requests and task content, sealed only to a device trusted here with its current key.
pub(crate) fn send_trusted(app: &tauri::AppHandle, to: &str, message: &Value) -> Result<()> {
    remote(app).lock().callable(to)?;
    let (key, _) = peer_key(app, to)?;
    send_sealed(app, to, &key, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_resets_only_after_a_stable_socket() {
        let s = Duration::from_secs;
        assert_eq!(backoff(s(8), &Outcome::Failed), Some((s(8), s(16))));
        assert_eq!(backoff(s(40), &Outcome::Failed), Some((s(40), s(60))));
        assert_eq!(backoff(s(8), &Outcome::Closed { up: s(5), code: None }), Some((s(8), s(16))));
        assert_eq!(backoff(s(8), &Outcome::Closed { up: s(30), code: None }), Some((s(1), s(2))));
        assert_eq!(backoff(s(1), &Outcome::Closed { up: s(300), code: Some(REPLACED) }), Some((s(60), s(2))));
        assert_eq!(backoff(s(1), &Outcome::Closed { up: s(300), code: Some(REVOKED) }), None);
        assert_eq!(backoff(s(1), &Outcome::Unauthorized), None);
    }

    #[test]
    fn presence_is_the_device_list_and_prunes_subscribers() {
        let mut inner = Inner::default();
        let until = Instant::now();
        inner.subscribers.insert("a".into(), super::super::Subscriber { until, focus: None });
        inner.subscribers.insert("b".into(), super::super::Subscriber { until, focus: None });
        apply_presence(&mut inner, &[json!({"id": "a", "name": "Mac", "platform": "macos", "publicKey": "k", "host": false, "online": true, "lastSeen": 5}), json!({"id": "c", "online": false, "host": true}), json!({"id": "bad id"})]);
        assert!(inner.presence);
        assert_eq!(inner.devices.iter().map(|d| d.id.as_str()).collect::<Vec<_>>(), ["a", "c"]);
        assert_eq!((inner.devices[0].name.as_str(), inner.devices[0].online, inner.devices[0].host), ("Mac", true, false));
        assert_eq!(inner.subscribers.keys().collect::<Vec<_>>(), ["a"]);
        apply_presence(&mut inner, &[]);
        assert!(inner.devices.is_empty() && inner.subscribers.is_empty());
    }

    #[test]
    fn only_the_focused_task_carries_live_state() {
        let events = task_changes("host", &json!({"sessionIds": ["s1", "s2", "bad id"], "focus": {"sessionId": "s2", "liveText": "hi", "status": "running", "historyVersion": 7}}));
        assert_eq!(events, vec![json!({"deviceId": "host", "sessionId": "s1"}), json!({"deviceId": "host", "sessionId": "s2", "live": {"liveText": "hi", "status": "running", "historyVersion": 7}})]);
    }

    #[test]
    fn trust_follows_the_pinned_key() {
        let (host, old, new) = (crypto::generate(), crypto::generate(), crypto::generate());
        let mut settings = config::Settings::default();
        settings.trusted.insert("viewer".into(), crypto::public_key(&old));
        assert!(settings.is_trusted("viewer", &crypto::public_key(&old)));
        // The relay (or the device) reports a new key: untrusted until trusted again.
        assert!(!settings.is_trusted("viewer", &crypto::public_key(&new)));
        assert!(!settings.is_trusted("other", &crypto::public_key(&old)));
        // A frame sealed by the new key never opens with the pinned pair key.
        let pinned = crypto::pair_key(&host, "host", &crypto::public_key(&old), "viewer").unwrap();
        let swapped = crypto::pair_key(&new, "viewer", &crypto::public_key(&host), "host").unwrap();
        let body = crypto::seal(&swapped, "viewer", "host", b"{}").unwrap();
        assert!(crypto::open(&pinned, "viewer", "host", &body).is_err());
        let view = Device { id: "viewer".into(), name: String::new(), platform: String::new(), public_key: crypto::public_key(&new), host: false, online: true, last_seen: Value::Null }.view("host", &settings);
        assert_eq!(view["trusted"], false);
    }
}
