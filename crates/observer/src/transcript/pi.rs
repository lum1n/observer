//! Observing pi sessions ACC didn't start, from their session files.
//!
//! pi appends every session to a JSONL file under `~/.pi/agent/sessions`.
//! The message objects are the ones pi's RPC mode sends, so the helpers that
//! read them come from [`acc_core::pi`].

use std::collections::HashMap;

use acc_core::event::{Event, ItemKind, Usage};
use acc_core::harness::Harness;
use acc_core::item::ItemFeed;
use acc_core::ndjson::{Frame, LineFramer};
use acc_core::pi::{MAX_LINE, add_usage, async_run, content_text, subagent_notify, tool_kind, user_text};
use acc_core::state::{RunState, SessionState, StateSource};
use serde_json::{Value, json};

/// Default directory pi stores sessions for `cwd` in, relative to `$HOME`:
/// `.pi/agent/sessions/--<cwd with separators as dashes>--`.
pub fn session_dir(cwd: &str) -> String {
    let trimmed = cwd.trim_start_matches(['/', '\\']);
    let encoded: String = trimmed
        .chars()
        .map(|c| if matches!(c, '/' | '\\' | ':') { '-' } else { c })
        .collect();
    format!(".pi/agent/sessions/--{encoded}--")
}

/// Reads a pi session file (JSONL) incrementally, as appended by a running pi.
///
/// State comes from the transcript ([`StateSource::Transcript`]): a user
/// message means working, an assistant message that stops for tool use means
/// still working, a final one means done, an error stop means error and an
/// aborted one means idle, and a final one with async pi-subagents runs still
/// out means waiting. pi never shows "blocked" in its transcript.
#[derive(Debug)]
pub struct Transcript {
    items: ItemFeed,
    framer: LineFramer,
    open_tools: Vec<String>,
    /// Async pi-subagents runs not yet reported back, as in [`acc_core::pi::PiDriver`].
    background: HashMap<String, String>,
    usage: Usage,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            items: ItemFeed::new(Harness::Pi),
            framer: LineFramer::new(MAX_LINE),
            open_tools: Vec::new(),
            background: HashMap::new(),
            usage: Usage::default(),
        }
    }

    /// Tool calls with no result yet.
    pub fn pending_tools(&self) -> usize {
        self.open_tools.len()
    }

    /// Feeds bytes from the session file (from a `tail -c +N -f`).
    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for frame in self.framer.feed(bytes) {
            if let Frame::Line(line) = frame
                && let Ok(v) = serde_json::from_str::<Value>(&line)
            {
                self.entry(&v, &mut out);
            }
        }
        out
    }

    /// [`Self::feed`] with `items` as the item feed, for readers whose items
    /// mix with another reader's: a subagent's reader reports to its parent
    /// call, so the parent's reader and its children share one feed.
    pub fn feed_shared(&mut self, bytes: &[u8], items: &mut ItemFeed) -> Vec<Event> {
        std::mem::swap(&mut self.items, items);
        let out = self.feed(bytes);
        std::mem::swap(&mut self.items, items);
        out
    }

    fn state(state: RunState, message: Option<String>, at: u64) -> Event {
        Event::StateChanged(SessionState {
            state,
            message,
            source: StateSource::Transcript,
            observed_at_ms: at,
        })
    }

    fn entry(&mut self, v: &Value, out: &mut Vec<Event>) {
        match v.get("type").and_then(Value::as_str) {
            Some("session") => out.push(Event::SessionStarted {
                harness: Harness::Pi,
                native_id: str_field(v, "id"),
                model: None,
            }),
            Some("message") => {
                let id = str_field(v, "id");
                let Some(msg) = v.get("message") else { return };
                let at = msg.get("timestamp").and_then(Value::as_u64).unwrap_or(0);
                match msg.get("role").and_then(Value::as_str) {
                    Some("user") => {
                        self.items
                            .start(id.clone(), ItemKind::UserMessage, None, Value::Null, out);
                        self.items.delta(id.clone(), user_text(msg.get("content")), out);
                        self.items.complete(id, Value::Null, out);
                        out.push(Self::state(RunState::Working { progress: None }, None, at));
                    }
                    Some("assistant") => self.assistant(id, msg, at, out),
                    Some("toolResult") => {
                        let call = str_field(msg, "toolCallId");
                        self.open_tools.retain(|t| *t != call);
                        let is_error = msg.get("isError").and_then(Value::as_bool).unwrap_or(false);
                        let details = msg.get("details");
                        let extensions = json!({
                            "result": { "content": msg.get("content"), "details": details },
                            "is_error": is_error,
                        });
                        if let Some(run) = async_run(&str_field(msg, "toolName"), details, is_error) {
                            self.background.insert(run.to_owned(), call.clone());
                            self.items.update(call, extensions, out);
                        } else {
                            self.items.complete(call, extensions, out);
                        }
                    }
                    _ => {}
                }
            }
            Some("custom_message") => subagent_notify(&mut self.items, &mut self.background, v, out),
            _ => {}
        }
    }

    fn assistant(&mut self, id: String, msg: &Value, at: u64, out: &mut Vec<Event>) {
        self.items
            .start(id.clone(), ItemKind::AgentMessage, None, Value::Null, out);
        let text = content_text(msg.get("content"));
        if !text.is_empty() {
            self.items.delta(id.clone(), text, out);
        }
        let stop = msg.get("stopReason").and_then(Value::as_str).unwrap_or_default();
        self.items.complete(
            id,
            json!({
                "provider": msg.get("provider"),
                "model": msg.get("model"),
                "stop_reason": stop,
            }),
            out,
        );
        for block in msg.get("content").and_then(Value::as_array).into_iter().flatten() {
            if block.get("type").and_then(Value::as_str) == Some("toolCall") {
                let call = str_field(block, "id");
                let tool = str_field(block, "name");
                self.open_tools.push(call.clone());
                self.items.start(
                    call,
                    tool_kind(&tool),
                    None,
                    json!({ "tool": tool, "args": block.get("arguments") }),
                    out,
                );
            }
        }
        if let Some(u) = msg.get("usage") {
            add_usage(&mut self.usage, u);
            out.push(Event::Usage(self.usage.clone()));
        }
        let state = match stop {
            "toolUse" => RunState::Working { progress: None },
            "error" => RunState::Error,
            "aborted" => RunState::Idle,
            _ if !self.background.is_empty() => RunState::Waiting,
            _ => RunState::Done,
        };
        let message = (stop == "error")
            .then(|| msg.get("errorMessage").and_then(Value::as_str))
            .flatten()
            .and_then(|m| m.lines().next())
            .map(str::to_owned);
        out.push(Self::state(state, message, at));
        if stop == "stop" {
            out.push(Event::TurnCompleted);
        }
    }
}

fn str_field(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or_default().to_owned()
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    const SESSION: &str = include_str!("../../tests/fixtures/pi/session.jsonl");

    fn states(events: &[Event]) -> Vec<RunState> {
        events
            .iter()
            .filter_map(|e| match e {
                Event::StateChanged(s) => Some(s.state.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn transcript_waits_on_an_async_subagent() {
        let session = r#"{"type":"message","id":"u1","message":{"role":"user","content":"map it","timestamp":1}}
{"type":"message","id":"a1","message":{"role":"assistant","content":[{"type":"toolCall","id":"call_1","name":"subagent","arguments":{"agent":"scout","async":true}}],"stopReason":"toolUse","timestamp":2}}
{"type":"message","id":"r1","message":{"role":"toolResult","toolCallId":"call_1","toolName":"subagent","content":[{"type":"text","text":"Async: scout [run-7]"}],"details":{"asyncId":"run-7","runId":"run-7"},"isError":false,"timestamp":3}}
{"type":"message","id":"a2","message":{"role":"assistant","content":[{"type":"text","text":"Launched."}],"stopReason":"stop","timestamp":4}}
{"type":"custom_message","id":"c1","customType":"subagent-notify","content":"scout completed","display":false,"details":{"runs":[{"agent":"scout","status":"completed","runId":"run-7"}]}}
{"type":"message","id":"u2","message":{"role":"user","content":"Inspect subagent updates above.","timestamp":5}}
{"type":"message","id":"a3","message":{"role":"assistant","content":[{"type":"text","text":"Done."}],"stopReason":"stop","timestamp":6}}
"#;
        let events = Transcript::new().feed(session.as_bytes());
        assert_eq!(
            states(&events),
            [
                RunState::Working { progress: None },
                RunState::Working { progress: None },
                RunState::Waiting,
                RunState::Working { progress: None },
                RunState::Done,
            ]
        );
        let call = |completed: bool| {
            events.iter().position(|e| match e {
                Event::ItemCompleted { id, .. } => completed && id == "call_1",
                Event::ItemUpdated { id, .. } => !completed && id == "call_1",
                _ => false,
            })
        };
        assert!(call(false) < call(true), "launched, then reported back");
    }

    #[test]
    fn transcript_replays_recorded_session() {
        let mut t = Transcript::new();
        let events = t.feed(SESSION.as_bytes());
        assert!(
            matches!(&events[0], Event::SessionStarted { native_id, .. } if native_id.starts_with("01a1"))
        );
        assert_eq!(
            states(&events),
            [
                RunState::Working { progress: None },
                RunState::Working { progress: None },
                RunState::Done,
            ]
        );
        assert!(
            events
                .iter()
                .any(|e| matches!(e, Event::ItemCompleted { id, .. } if id == "call_1"))
        );
        assert!(matches!(events.last(), Some(Event::TurnCompleted)));
    }

    #[test]
    fn session_dir_encoding() {
        assert_eq!(
            session_dir("/home/u/my proj"),
            ".pi/agent/sessions/--home-u-my proj--"
        );
        assert_eq!(session_dir("C:\\src\\x"), ".pi/agent/sessions/--C--src-x--");
    }
}
