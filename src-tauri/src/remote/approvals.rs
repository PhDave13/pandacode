//! The text of each pending approval, so `task.get` can show it to another computer.
//! Ids come from the running agent's set; this only remembers what each one asked.
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

const TITLE: usize = 160;
const DETAIL: usize = 4 * 1024;

/// Per task: (request id, text) in arrival order.
type Texts = Mutex<HashMap<String, Vec<(String, String)>>>;
fn texts() -> &'static Texts {
    static TEXTS: std::sync::OnceLock<Texts> = std::sync::OnceLock::new();
    TEXTS.get_or_init(Default::default)
}

pub(crate) fn record(session_id: &str, request_id: &str, text: &str) {
    let mut map = texts().lock().unwrap_or_else(|e| e.into_inner());
    let list = map.entry(session_id.to_owned()).or_default();
    list.retain(|(id, _)| id != request_id);
    list.push((request_id.to_owned(), text.to_owned()));
}

pub(crate) fn settle(session_id: &str, request_id: &str) {
    let mut map = texts().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(list) = map.get_mut(session_id) {
        list.retain(|(id, _)| id != request_id);
        if list.is_empty() {
            map.remove(session_id);
        }
    }
}

pub(crate) fn clear(session_id: &str) {
    texts().lock().unwrap_or_else(|e| e.into_inner()).remove(session_id);
}

/// The task's live approvals; answered ones (no longer in `live`) are forgotten.
pub(crate) fn pending(session_id: &str, live: &HashSet<String>) -> Vec<Value> {
    let mut map = texts().lock().unwrap_or_else(|e| e.into_inner());
    let list = map.entry(session_id.to_owned()).or_default();
    list.retain(|(id, _)| live.contains(id));
    let mut views: Vec<Value> = list.iter().map(|(id, text)| view(id, text)).collect();
    // A request asked before this module saw it still gets a button.
    views.extend(live.iter().filter(|id| !list.iter().any(|(known, _)| known == *id)).map(|id| view(id, "")));
    if list.is_empty() {
        map.remove(session_id);
    }
    views
}

fn truncate(text: &str, max: usize) -> &str {
    let mut end = text.len().min(max);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// First line as `title` (≤160 chars), the rest as `detail` (≤4 KB).
pub(crate) fn view(request_id: &str, text: &str) -> Value {
    let text = text.trim();
    let (first, rest) = text.split_once('\n').unwrap_or((text, ""));
    let mut view = json!({"requestId": request_id});
    let title: String = first.trim().chars().take(TITLE).collect();
    if !title.is_empty() {
        view["title"] = json!(title);
    }
    let detail = truncate(rest.trim(), DETAIL);
    if !detail.is_empty() {
        view["detail"] = json!(detail);
    }
    view
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_maps_to_a_short_title_and_bounded_detail() {
        assert_eq!(view("r1", "Run `ls`\n  in /tmp\nok "), json!({"requestId": "r1", "title": "Run `ls`", "detail": "in /tmp\nok"}));
        assert_eq!(view("r2", ""), json!({"requestId": "r2"}));
        let long = view("r3", &format!("{}\n{}", "é".repeat(200), "ü".repeat(3000)));
        assert_eq!(long["title"].as_str().unwrap().chars().count(), TITLE);
        let detail = long["detail"].as_str().unwrap();
        assert!(detail.len() <= DETAIL && detail.len() > DETAIL - 2);
    }

    #[test]
    fn only_live_requests_are_listed() {
        record("task-a", "r1", "First\nmore");
        record("task-a", "r2", "Second");
        let live: HashSet<String> = ["r2".to_string(), "r3".to_string()].into();
        let views = pending("task-a", &live);
        assert_eq!(views[0], json!({"requestId": "r2", "title": "Second"}));
        assert_eq!(views[1], json!({"requestId": "r3"}));
        settle("task-a", "r2");
        assert_eq!(pending("task-a", &HashSet::new()), Vec::<Value>::new());
    }
}
