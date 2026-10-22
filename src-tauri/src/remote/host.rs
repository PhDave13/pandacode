//! Answers requests from the account's other computers, only while
//! "Allow remote control" is on and only from computers trusted here (their key pinned),
//! and sends them batched `tasks.changed` events. Remote prompts run under the same
//! approval policy as local ones.
use super::{approvals, remote, socket, Subscriber};
use crate::{message_queue, overlay, AppState, Result};
use serde_json::{json, Value};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};
use tauri::{Emitter, Manager};

pub(crate) const METHODS: [&str; 7] = ["subscribe", "tasks.list", "task.get", "task.start", "task.send", "task.stop", "approval.respond"];
pub(crate) const OFF: &str = "Remote control is off on this computer";
const MAX_TEXT: usize = 100 * 1024;
const SUBSCRIPTION: Duration = Duration::from_secs(10 * 60);
const BATCH: Duration = Duration::from_millis(250);
const MAX_IDS: usize = 500;
const WINDOW_MS: i64 = 60_000;
const MAX_SEEN: usize = 20_000;
/// A `task.get` reply's JSON budget: sealing adds a third (base64) plus a small envelope,
/// so this stays under the relay's 256 KB frame.
const PAGE_BUDGET: usize = 180 * 1024;
/// A single message larger than this is cut, with [`CUT`] marking the cut.
const MAX_MESSAGE: usize = 120 * 1024;
/// Live text keeps only its tail beyond this.
const MAX_LIVE: usize = 32 * 1024;
const CUT: &str = "\n…[cut on the host to fit a remote page]";

pub(crate) fn untrusted(host_name: &str) -> String {
    format!("This computer isn't trusted on {host_name}. Trust it in Settings → Remote access on that computer.")
}

/// Hosting must be on, and the frame must have been sealed with the caller's pinned key.
pub(crate) fn gate(host_enabled: bool, trusted: bool, host_name: &str) -> Result<()> {
    if !host_enabled {
        return Err(OFF.into());
    }
    if !trusted {
        return Err(untrusted(host_name));
    }
    Ok(())
}

/// Request nonces seen in the last minute. Requests outside ±60 s or seen before are refused.
#[derive(Default)]
pub(crate) struct Replay {
    seen: HashMap<String, i64>,
}

impl Replay {
    pub(crate) fn check(&mut self, ts: Option<i64>, nonce: Option<&str>, now_ms: i64) -> Result<()> {
        const REFUSED: &str = "This request expired or was already received. Check that both computers' clocks are right.";
        let (Some(ts), Some(nonce)) = (ts, nonce) else { return Err(REFUSED.into()) };
        if !(16..=64).contains(&nonce.len()) || !nonce.chars().all(|c| c.is_ascii_graphic()) || (now_ms - ts).abs() > WINDOW_MS {
            return Err(REFUSED.into());
        }
        // An accepted nonce only needs remembering while its ts is still inside the window.
        self.seen.retain(|_, seen| *seen >= now_ms - WINDOW_MS);
        if self.seen.contains_key(nonce) {
            return Err(REFUSED.into());
        }
        if self.seen.len() >= MAX_SEEN {
            return Err("This computer is busy. Try again in a moment.".into());
        }
        self.seen.insert(nonce.to_owned(), ts);
        Ok(())
    }
}

/// Ids are UUIDs or provider request ids: short, printable, no spaces.
pub(crate) fn valid_id(id: &str) -> Result<()> {
    if id.is_empty() || id.len() > 200 || !id.chars().all(|c| c.is_ascii_graphic()) {
        return Err("Invalid id.".into());
    }
    Ok(())
}

#[derive(Debug, PartialEq)]
pub(crate) enum Call {
    Subscribe { session_id: Option<String> },
    List,
    Get { session_id: String, before_id: Option<i64>, history_version: Option<u64> },
    Start { project_id: Option<String>, text: String },
    Send { session_id: String, text: String },
    Stop { session_id: String },
    Approve { session_id: String, request_id: String, approve: bool },
}

fn id_param(params: &Value, key: &str) -> Result<String> {
    let id = params[key].as_str().ok_or_else(|| format!("Missing {key}."))?;
    valid_id(id).map_err(|_| format!("Invalid {key}."))?;
    Ok(id.to_owned())
}

fn optional_id(params: &Value, key: &str) -> Result<Option<String>> {
    params.get(key).filter(|v| !v.is_null()).map(|_| id_param(params, key)).transpose()
}

fn text_param(params: &Value) -> Result<String> {
    let text = params["text"].as_str().map(str::trim).unwrap_or_default();
    if text.is_empty() || text.len() > MAX_TEXT {
        return Err("Send between 1 byte and 100 KB of text.".into());
    }
    Ok(text.to_owned())
}

/// Validates a request. Everything is refused while hosting is off.
pub(crate) fn parse(host_enabled: bool, method: &str, params: &Value) -> Result<Call> {
    if !host_enabled {
        return Err(OFF.into());
    }
    Ok(match method {
        "subscribe" => Call::Subscribe { session_id: optional_id(params, "sessionId")? },
        "tasks.list" => Call::List,
        "task.get" => Call::Get { session_id: id_param(params, "sessionId")?, before_id: params["beforeId"].as_i64(), history_version: params["historyVersion"].as_u64() },
        "task.start" => Call::Start { project_id: optional_id(params, "projectId")?, text: text_param(params)? },
        "task.send" => Call::Send { session_id: id_param(params, "sessionId")?, text: text_param(params)? },
        "task.stop" => Call::Stop { session_id: id_param(params, "sessionId")? },
        "approval.respond" => Call::Approve { session_id: id_param(params, "sessionId")?, request_id: id_param(params, "requestId")?, approve: params["approve"].as_bool().ok_or("Missing approve.")? },
        _ => return Err("Unknown method.".into()),
    })
}

/// The response to one request frame, or None when it has no usable id.
/// `trusted` is true only when the frame opened with the caller's pinned key.
pub(crate) async fn handle(app: &tauri::AppHandle, from: &str, message: &Value, trusted: bool) -> Option<Value> {
    let id = message["id"].as_str().filter(|id| valid_id(id).is_ok())?;
    let name = super::config::device_name();
    let admitted = {
        let state = remote(app);
        let mut inner = state.lock();
        let now_ms = chrono::Utc::now().timestamp_millis();
        let enabled = inner.settings.host_enabled;
        // Trust first: an untrusted caller never fills the nonce set.
        gate(enabled, trusted, &name).and_then(|_| inner.replay.check(message["ts"].as_i64(), message["nonce"].as_str(), now_ms)).map(|_| enabled)
    };
    let result = match admitted.and_then(|enabled| parse(enabled, message["method"].as_str().unwrap_or_default(), &message["params"])) {
        Ok(call) => run(app, from, call).await,
        Err(error) => Err(error),
    };
    Some(match result {
        Ok(result) => json!({"type": "res", "id": id, "ok": true, "result": result}),
        Err(error) => json!({"type": "res", "id": id, "ok": false, "error": error}),
    })
}

async fn run(app: &tauri::AppHandle, from: &str, call: Call) -> Result<Value> {
    let state = app.state::<AppState>();
    match call {
        Call::Subscribe { session_id } => {
            remote(app).lock().subscribers.insert(from.to_owned(), Subscriber { until: Instant::now() + SUBSCRIPTION, focus: session_id });
            Ok(json!({}))
        }
        Call::List => to_json(&overlay::overlay_snapshot(state, app.state()).await?),
        Call::Get { session_id, before_id, history_version } => {
            let live = state.agents.lock().map_err(|e| e.to_string())?.get(&session_id).filter(|agent| !agent.stopped.load(std::sync::atomic::Ordering::Acquire)).map(|agent| agent.approvals.lock().map(|set| set.clone()).unwrap_or_default()).unwrap_or_default();
            let mut thread = thread(app, &session_id, before_id, history_version, true).await?;
            thread["approvals"] = json!(approvals::pending(&session_id, &live));
            cap_page(&mut thread);
            Ok(thread)
        }
        Call::Start { project_id, text } => {
            let session_id = overlay::overlay_launch(app.clone(), state, app.state(), text, project_id).await?;
            Ok(json!({"sessionId": session_id}))
        }
        Call::Send { session_id, text } => {
            // Like Enter in the composer: a running task queues the message.
            let busy = || state.agents.lock().map(|agents| agents.contains_key(&session_id)).map_err(|e| e.to_string());
            if busy()? {
                let entry = message_queue::add(&*state.store.lock().map_err(|e| e.to_string())?, &session_id, &text, &json!([]))?;
                // If the run ended between the check and the add, nothing would deliver the
                // entry: take it back (unless a run end already did) and send it now.
                if busy()? || message_queue::take(&*state.store.lock().map_err(|e| e.to_string())?, &entry.id).is_err() {
                    let _ = app.emit("queue-changed", json!({"sessionId": session_id}));
                    return Ok(json!({"queued": true}));
                }
            }
            crate::send_message(app.clone(), state, session_id, text, None, None).await?;
            let _ = app.emit("sessions-changed", ());
            Ok(json!({"queued": false}))
        }
        Call::Stop { session_id } => {
            crate::stop_session(app.clone(), state, session_id).await?;
            Ok(json!({}))
        }
        Call::Approve { session_id, request_id, approve } => {
            crate::respond_approval(state, session_id.clone(), request_id.clone(), approve).await?;
            approvals::settle(&session_id, &request_id);
            // Clear the card in this computer's window, as bot approvals do.
            let event = crate::providers::AgentEvent { session_id, kind: "approval_cancelled".into(), text: String::new(), request_id: Some(request_id), data: None, tool: None };
            let _ = app.emit("agent-event", event);
            Ok(json!({}))
        }
    }
}

fn to_json(value: &impl serde::Serialize) -> Result<Value> {
    serde_json::to_value(value).map_err(|e| e.to_string())
}

/// This task's history version: it changes when one of its messages is added, removed or
/// rewritten to another length, never because another task changed. Kept under 2^53 so
/// JavaScript reads it exactly.
// ponytail: a same-length rewrite in place goes unseen; add an updated_at column if that matters.
pub(crate) fn history_version(db: &rusqlite::Connection, session_id: &str) -> Result<u64> {
    use std::hash::{Hash, Hasher};
    let row: (i64, i64, i64) = db
        .query_row("SELECT COALESCE(MAX(id),0), COUNT(*), COALESCE(SUM(length(content)),0) FROM messages WHERE session_id=?1", [session_id], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .map_err(|e| e.to_string())?;
    let mut hasher = std::hash::DefaultHasher::new();
    row.hash(&mut hasher);
    Ok(hasher.finish() & ((1 << 53) - 1))
}

/// Puts this task's own version on an overlay thread; an unchanged history sends no messages.
fn mark(thread: &mut Value, version: u64, unchanged: bool) {
    thread["historyVersion"] = json!(version);
    thread["historyUnchanged"] = json!(unchanged);
    if unchanged {
        thread["messages"] = json!([]);
        thread["hasOlder"] = json!(false);
    }
}

/// The overlay thread with per-task versioning; `messages: false` reads only the live state.
async fn thread(app: &tauri::AppHandle, session_id: &str, before_id: Option<i64>, known: Option<u64>, messages: bool) -> Result<Value> {
    let state = app.state::<AppState>();
    // Read before the page, so a message added meanwhile is fetched again next time.
    let (version, revision) = {
        let store = state.store.lock().map_err(|e| e.to_string())?;
        (history_version(&store.db, session_id)?, store.db.total_changes())
    };
    let unchanged = before_id.is_none() && (!messages || known == Some(version));
    // Given its own current revision, the overlay skips reading the page.
    let snapshot = overlay::overlay_thread(state, session_id.to_owned(), None, before_id, unchanged.then_some(revision)).await?;
    let mut thread = to_json(&snapshot)?;
    mark(&mut thread, version, unchanged);
    Ok(thread)
}

/// The focused task's live state for a `tasks.changed` event.
async fn focus(app: &tauri::AppHandle, session_id: &str) -> Option<Value> {
    let thread = thread(app, session_id, None, None, false).await.ok()?;
    Some(json!({"sessionId": session_id, "liveText": tail(thread["liveText"].as_str().unwrap_or_default(), MAX_LIVE), "status": thread["status"], "historyVersion": thread["historyVersion"]}))
}

/// The last `max` bytes of `text`, on a character boundary.
fn tail(text: &str, max: usize) -> &str {
    let mut start = text.len().saturating_sub(max);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn head(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn longest_string(value: &mut Value) -> Option<&mut String> {
    match value {
        Value::String(text) => Some(text),
        Value::Array(items) => items.iter_mut().filter_map(longest_string).max_by_key(|text| text.len()),
        Value::Object(map) => map.values_mut().filter_map(longest_string).max_by_key(|text| text.len()),
        _ => None,
    }
}

/// Halves the longest string in `value`, marking the cut, until it serializes under `max`.
fn shrink(value: &mut Value, max: usize) {
    while value.to_string().len() > max {
        let Some(longest) = longest_string(value).filter(|text| text.len() > 2 * CUT.len()) else { return };
        *longest = format!("{}{CUT}", head(longest, longest.len() / 2));
    }
}

/// Cuts one oversized message. JSON content (tool calls) is cut inside, so it still parses.
fn cut(message: &mut Value) {
    let parsed = message["content"].as_str().and_then(|content| serde_json::from_str::<Value>(content).ok()).filter(Value::is_object);
    if let Some(mut content) = parsed {
        shrink(&mut content, MAX_MESSAGE * 3 / 4);
        message["content"] = json!(content.to_string());
    }
    shrink(message, MAX_MESSAGE);
}

/// Keeps a `task.get` reply inside one relay frame: live text keeps its tail, a huge
/// message is cut with a marker, and the oldest messages are left for the next page.
fn cap_page(thread: &mut Value) {
    if let Some(live) = thread["liveText"].as_str().filter(|text| text.len() > MAX_LIVE) {
        thread["liveText"] = json!(tail(live, MAX_LIVE));
    }
    let Some(messages) = thread.get_mut("messages").and_then(Value::as_array_mut).map(std::mem::take) else { return };
    let total = messages.len();
    let mut room = PAGE_BUDGET.saturating_sub(thread.to_string().len());
    let mut kept = Vec::new();
    for mut message in messages.into_iter().rev() {
        if message.to_string().len() > MAX_MESSAGE {
            cut(&mut message);
        }
        let size = message.to_string().len() + 1;
        if size > room {
            break;
        }
        room -= size;
        kept.push(message);
    }
    if kept.len() < total {
        thread["hasOlder"] = json!(true);
    }
    kept.reverse();
    thread["messages"] = Value::Array(kept);
}

/// Changed task ids, flushed to every subscriber at most once per 250 ms.
#[derive(Default)]
pub(crate) struct Batch {
    dirty: BTreeSet<String>,
    scheduled: bool,
    last: Option<Instant>,
}

impl Batch {
    /// Marks a task changed; Some(delay) means the caller should start the flush timer.
    pub(crate) fn note(&mut self, id: &str, now: Instant) -> Option<Duration> {
        self.dirty.insert(id.to_owned());
        if self.scheduled {
            return None;
        }
        self.scheduled = true;
        Some(self.last.map_or(Duration::ZERO, |last| BATCH.saturating_sub(now.saturating_duration_since(last))))
    }

    /// Up to [`MAX_IDS`] changed ids, and whether more are left for the next flush.
    pub(crate) fn take(&mut self, now: Instant) -> (Vec<String>, bool) {
        self.last = Some(now);
        let ids: Vec<String> = self.dirty.iter().take(MAX_IDS).cloned().collect();
        for id in &ids {
            self.dirty.remove(id);
        }
        self.scheduled = !self.dirty.is_empty();
        (ids, self.scheduled)
    }
}

/// Drops subscribers that expired, left the account or lost trust; true if any remain.
fn prune_subscribers(inner: &mut super::Inner, now: Instant) -> bool {
    let super::Inner { subscribers, devices, settings, .. } = inner;
    subscribers.retain(|id, subscriber| subscriber.until > now && devices.iter().any(|d| &d.id == id && settings.is_trusted(id, &d.public_key)));
    !subscribers.is_empty()
}

/// Called on the agent-event hot path: no allocation unless someone subscribes.
pub(crate) fn task_changed(app: &tauri::AppHandle, session_id: &str) {
    let Some(state) = app.try_state::<super::Remote>() else { return };
    let delay = {
        let mut inner = state.lock();
        if !inner.settings.host_enabled || inner.outbox.is_none() || inner.subscribers.is_empty() {
            return;
        }
        let now = Instant::now();
        if !prune_subscribers(&mut inner, now) {
            return;
        }
        inner.batch.note(session_id, now)
    };
    let Some(delay) = delay else { return };
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(delay).await;
        loop {
            let (ids, devices, more) = {
                let state = remote(&app);
                let mut inner = state.lock();
                let now = Instant::now();
                let (ids, more) = inner.batch.take(now);
                let devices: Vec<(String, Option<String>)> = if inner.settings.host_enabled && prune_subscribers(&mut inner, now) {
                    inner.subscribers.iter().map(|(id, subscriber)| (id.clone(), subscriber.focus.clone())).collect()
                } else {
                    Vec::new()
                };
                (ids, devices, more)
            };
            // Each focused task is read once per flush, however many devices watch it.
            let mut focused: HashMap<String, Option<Value>> = HashMap::new();
            for (device, focus_id) in devices {
                let mut data = json!({"sessionIds": ids});
                if let Some(session) = focus_id.filter(|session| ids.contains(session)) {
                    if !focused.contains_key(&session) {
                        let live = focus(&app, &session).await;
                        focused.insert(session.clone(), live);
                    }
                    if let Some(live) = &focused[&session] {
                        data["focus"] = live.clone();
                    }
                }
                let _ = socket::send_trusted(&app, &device, &json!({"type": "event", "name": "tasks.changed", "data": data}));
            }
            if !more {
                break;
            }
            tokio::time::sleep(BATCH).await;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn everything_is_refused_while_hosting_is_off() {
        for method in METHODS {
            assert_eq!(parse(false, method, &json!({"sessionId": "s", "text": "hi", "requestId": "r", "approve": true})), Err(OFF.to_string()));
        }
        assert_eq!(parse(false, "nope", &json!({})), Err(OFF.to_string()));
    }

    #[test]
    fn requests_are_validated() {
        assert_eq!(parse(true, "subscribe", &json!({})), Ok(Call::Subscribe { session_id: None }));
        assert_eq!(parse(true, "subscribe", &json!({"sessionId": "s1"})), Ok(Call::Subscribe { session_id: Some("s1".into()) }));
        assert!(parse(true, "subscribe", &json!({"sessionId": "a b"})).is_err());
        assert_eq!(parse(true, "task.send", &json!({"sessionId": "s1", "text": " hi "})), Ok(Call::Send { session_id: "s1".into(), text: "hi".into() }));
        assert_eq!(parse(true, "task.start", &json!({"text": "go", "projectId": null})), Ok(Call::Start { project_id: None, text: "go".into() }));
        assert!(parse(true, "task.start", &json!({"text": "go", "projectId": "bad id"})).is_err());
        assert!(parse(true, "task.send", &json!({"sessionId": "s1", "text": "x".repeat(MAX_TEXT + 1)})).is_err());
        assert!(parse(true, "task.send", &json!({"sessionId": "s1", "text": "  "})).is_err());
        assert!(parse(true, "task.get", &json!({"sessionId": "a\nb"})).is_err());
        assert!(parse(true, "task.stop", &json!({})).is_err());
        assert!(parse(true, "approval.respond", &json!({"sessionId": "s", "requestId": "r"})).is_err());
        assert!(parse(true, "shell.exec", &json!({})).is_err());
        assert!(valid_id(&"a".repeat(201)).is_err());
    }

    #[test]
    fn untrusted_callers_are_refused_by_name() {
        assert_eq!(gate(false, true, "Mac"), Err(OFF.to_string()));
        assert_eq!(gate(true, false, "Mac"), Err("This computer isn't trusted on Mac. Trust it in Settings → Remote access on that computer.".to_string()));
        assert_eq!(gate(true, true, "Mac"), Ok(()));
    }

    #[test]
    fn replays_and_stale_requests_are_refused() {
        let mut replay = Replay::default();
        let now = 1_000_000_000;
        let nonce = "0123456789abcdef0123";
        assert!(replay.check(Some(now - 1000), Some(nonce), now).is_ok());
        assert!(replay.check(Some(now - 1000), Some(nonce), now + 10).is_err(), "same nonce twice");
        assert!(replay.check(Some(now - 60_001), Some("fresh-nonce-000001"), now).is_err(), "too old");
        assert!(replay.check(Some(now + 60_001), Some("fresh-nonce-000002"), now).is_err(), "too new");
        assert!(replay.check(None, Some("fresh-nonce-000003"), now).is_err());
        assert!(replay.check(Some(now), Some("short"), now).is_err());
        assert!(replay.check(Some(now), None, now).is_err());
        // Seen nonces are pruned once their ts leaves the window.
        assert!(replay.check(Some(now + 61_500), Some("fresh-nonce-000004"), now + 61_500).is_ok());
        assert!(!replay.seen.contains_key(nonce));
    }

    #[test]
    fn changes_batch_into_one_event_per_250_ms() {
        let mut batch = Batch::default();
        let t0 = Instant::now();
        assert_eq!(batch.note("b", t0), Some(Duration::ZERO));
        assert_eq!(batch.note("a", t0), None);
        assert_eq!(batch.note("b", t0), None);
        assert_eq!(batch.take(t0), (vec!["a".to_string(), "b".to_string()], false));
        assert_eq!(batch.note("c", t0 + Duration::from_millis(100)), Some(Duration::from_millis(150)));
        assert_eq!(batch.take(t0 + Duration::from_millis(250)), (vec!["c".to_string()], false));
        assert_eq!(batch.note("d", t0 + Duration::from_millis(900)), Some(Duration::ZERO));
        for i in 0..MAX_IDS {
            batch.note(&format!("t{i:04}"), t0 + Duration::from_millis(900));
        }
        let (ids, more) = batch.take(t0 + Duration::from_millis(900));
        assert_eq!((ids.len(), more), (MAX_IDS, true));
        assert_eq!(batch.take(t0 + Duration::from_millis(1150)).0.len(), 1);
    }

    #[test]
    fn history_versions_are_per_task() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE messages(id INTEGER PRIMARY KEY AUTOINCREMENT, session_id TEXT NOT NULL, content TEXT NOT NULL); INSERT INTO messages(session_id,content) VALUES ('a','hi'),('b','yo');").unwrap();
        let first = history_version(&db, "a").unwrap();
        assert!(first < 1 << 53);
        db.execute("INSERT INTO messages(session_id,content) VALUES ('b','another task')", []).unwrap();
        assert_eq!(history_version(&db, "a").unwrap(), first, "another task's change keeps this version");
        db.execute("UPDATE messages SET content='{\"status\":\"completed\"}' WHERE session_id='a'", []).unwrap();
        let rewritten = history_version(&db, "a").unwrap();
        assert_ne!(rewritten, first, "a tool updated in place changes it");
        db.execute("INSERT INTO messages(session_id,content) VALUES ('a','next')", []).unwrap();
        assert_ne!(history_version(&db, "a").unwrap(), rewritten);
        assert_ne!(history_version(&db, "empty").unwrap(), first);
    }

    #[test]
    fn an_unchanged_history_sends_no_messages() {
        let mut thread = json!({"messages": [{"id": 1}], "hasOlder": true, "liveText": "x", "historyVersion": 99});
        mark(&mut thread, 42, true);
        assert_eq!(thread, json!({"messages": [], "hasOlder": false, "liveText": "x", "historyVersion": 42, "historyUnchanged": true}));
        let mut thread = json!({"messages": [{"id": 1}], "hasOlder": true});
        mark(&mut thread, 42, false);
        assert_eq!(thread, json!({"messages": [{"id": 1}], "hasOlder": true, "historyVersion": 42, "historyUnchanged": false}));
    }

    #[test]
    fn pages_fit_one_frame() {
        let message = |id: i64, content: String| json!({"id": id, "sessionId": "s", "role": "assistant", "content": content, "createdAt": 0});
        let mut messages: Vec<Value> = (1..=40).map(|id| message(id, "a".repeat(10 * 1024))).collect();
        messages.push(message(41, "b".repeat(10 * 1024)));
        messages.push(message(42, "é".repeat(200 * 1024)));
        let mut thread = json!({"messages": messages, "hasOlder": false, "liveText": "l".repeat(100 * 1024), "status": "running"});
        cap_page(&mut thread);
        assert!(thread.to_string().len() <= PAGE_BUDGET);
        assert_eq!(thread["hasOlder"], true);
        assert_eq!(thread["liveText"].as_str().unwrap().len(), MAX_LIVE);
        let kept = thread["messages"].as_array().unwrap();
        // The newest messages stay, in order; huge ones are cut with a marker.
        let ids: Vec<i64> = kept.iter().map(|m| m["id"].as_i64().unwrap()).collect();
        assert_eq!(ids.last(), Some(&42));
        assert!(ids.windows(2).all(|pair| pair[1] == pair[0] + 1));
        let newest = &kept[kept.len() - 1];
        assert!(newest.to_string().len() <= MAX_MESSAGE && newest["content"].as_str().unwrap().ends_with(CUT));
        assert!(ids.len() > 1 && ids.len() < 42);
        let mut tool = message(43, json!({"status": "completed", "output": "o".repeat(300 * 1024)}).to_string());
        cut(&mut tool);
        assert!(tool.to_string().len() <= MAX_MESSAGE);
        let tool = serde_json::from_str::<Value>(tool["content"].as_str().unwrap()).expect("tool content still parses");
        assert!(tool["output"].as_str().unwrap().ends_with(CUT) && tool["status"] == "completed");
        // A sealed frame for this page stays under the relay's limit.
        let key = [7u8; 32];
        let body = super::super::crypto::seal(&key, "a", "b", json!({"type": "res", "id": "r", "ok": true, "result": thread}).to_string().as_bytes()).unwrap();
        assert!(json!({"to": "b", "body": body, "ref": "r"}).to_string().len() < 256 * 1024);
        // A small page is untouched.
        let mut small = json!({"messages": [message(1, "hi".into())], "hasOlder": false, "liveText": ""});
        let before = small.clone();
        cap_page(&mut small);
        assert_eq!(small, before);
    }
}
