//! Observing opencode sessions ACC didn't start, through opencode's server.
//!
//! opencode 2.x keeps every session in one SQLite database and serves it
//! from a background service its TUIs connect to; there is no file to tail.
//! `opencode api` (which finds the service and signs in itself) reads it:
//! `GET /api/session` lists sessions with their directory,
//! `/api/session/<id>/message` returns typed messages newest first (`user`,
//! `assistant` with reasoning, text and tool parts, `idle` with the turn's
//! outcome, `synthetic` subagent reports), and `/permission` and `/form`
//! hold the approvals and questions waiting on the user. [`Session`] polls
//! those ([`Session::poll_script`]) and turns what changed into items and
//! state, shaped like the tool calls opencode's ACP server reports.
//! opencode 1.x has no `api` command and isn't read.

use std::collections::{HashMap, HashSet};

use acc_core::event::{Event, ItemKind, Usage};
use acc_core::harness::Harness;
use acc_core::item::ItemFeed;
use acc_core::shell::quote;
use acc_core::state::{BlockKind, RunState, SessionState, StateSource};
use serde_json::{Value, json};

use super::claude::whole_item;
use crate::wire::{owned, str_at, u64_at};

/// opencode's tools, as the ACP kind its ACP server reports for them.
fn acp_kind(tool: &str) -> &'static str {
    match tool {
        "shell" | "bash" => "execute",
        "read" => "read",
        "edit" | "write" | "patch" | "multiedit" | "apply_patch" => "edit",
        "glob" | "grep" | "list" | "ls" | "codesearch" => "search",
        "webfetch" | "websearch" => "fetch",
        _ => "other",
    }
}

fn is_subagent(tool: &str) -> bool {
    matches!(tool, "subagent" | "task")
}

/// What has been reported of one item.
#[derive(Debug, Default)]
struct Seen {
    /// Bytes of text sent as deltas.
    text: usize,
    /// The native JSON last reported, for tools.
    native: Value,
    tool: bool,
    started: bool,
    done: bool,
}

/// Follows one opencode session (and its subagents' sessions) by polling.
#[derive(Debug)]
pub struct Session {
    items: ItemFeed,
    cwd: String,
    id: Option<String>,
    /// `time.updated` of the session at the last poll.
    updated: Option<u64>,
    seen: HashMap<String, Seen>,
    /// Messages handled whole (user prompts, idles, reports).
    handled: HashSet<String>,
    /// Child session id → the subagent call running it.
    children: HashMap<String, String>,
    /// Child sessions still running, polled with the session.
    running: HashSet<String>,
    /// Subagent calls launched in the background, not yet reported back.
    background: HashSet<String>,
    /// Message ids already counted in `usage`.
    counted: HashSet<String>,
    permissions: bool,
    questions: bool,
    state: Option<RunState>,
    usage: Usage,
}

impl Session {
    /// The session `id` when known (from `--session`), else the newest one
    /// in `cwd`, found on the first poll.
    pub fn new(cwd: &str, id: Option<String>) -> Self {
        Self {
            items: ItemFeed::new(Harness::Opencode),
            cwd: cwd.to_owned(),
            id,
            updated: None,
            seen: HashMap::new(),
            handled: HashSet::new(),
            children: HashMap::new(),
            running: HashSet::new(),
            background: HashSet::new(),
            counted: HashSet::new(),
            permissions: false,
            questions: false,
            state: None,
            usage: Usage::default(),
        }
    }

    pub fn id(&self) -> Option<&str> {
        self.id.as_deref()
    }

    pub fn state(&self) -> Option<&RunState> {
        self.state.as_ref()
    }

    /// Tool calls still running, but for background subagents.
    pub fn pending_tools(&self) -> usize {
        self.seen
            .iter()
            .filter(|(id, s)| s.tool && !s.done && !self.background.contains(*id))
            .count()
    }

    /// Background subagents still running; they die with the TUI's service
    /// only if it stops, but resuming headless doesn't wait for them.
    pub fn background_agents(&self) -> usize {
        self.background.len()
    }

    /// Script printing what changed: `L` the session list (while the id is
    /// unknown), `S` the session, `P` and `F` its pending permissions and
    /// questions, and `M\t<session>` the messages of the session (when it
    /// changed or may be busy) and of running subagents. One JSON line each.
    /// Pass the output to [`Session::on_poll`].
    pub fn poll_script(&self) -> String {
        let mut s = format!(
            "{}\noc() {{ printf '%s\\t' \"$1\"; opencode api GET \"$2\" 2>/dev/null | tr -d '\\n'; printf '\\n'; }}\n",
            acc_core::detached::login_path_prelude()
        );
        let Some(id) = &self.id else {
            s.push_str("oc L /api/session\n");
            return s;
        };
        let q = |path: String| quote(&path);
        s.push_str(&format!(
            "s=$(opencode api GET {} 2>/dev/null | tr -d '\\n'); printf 'S\\t%s\\n' \"$s\"\n",
            q(format!("/api/session/{id}"))
        ));
        s.push_str(&format!("oc P {}\n", q(format!("/api/session/{id}/permission"))));
        s.push_str(&format!("oc F {}\n", q(format!("/api/session/{id}/form"))));
        let messages = format!(
            "oc {} {}",
            q(format!("M:{id}")),
            q(format!("/api/session/{id}/message"))
        );
        let settled = matches!(
            self.state,
            Some(RunState::Done | RunState::Idle | RunState::Error)
        ) && self.running.is_empty();
        // Messages only when the session moved, or may be moving.
        match self.updated.filter(|_| settled) {
            Some(u) => s.push_str(&format!(
                "case $s in *'\"updated\":{u},'*|*'\"updated\":{u}}}'*) ;; *) {messages} ;; esac\n"
            )),
            None => {
                s.push_str(&messages);
                s.push('\n');
            }
        }
        let mut running: Vec<&String> = self.running.iter().collect();
        running.sort();
        for child in running {
            s.push_str(&format!(
                "oc {} {}\n",
                q(format!("M:{child}")),
                q(format!("/api/session/{child}/message"))
            ));
        }
        s
    }

    /// The output of [`Session::poll_script`].
    pub fn on_poll(&mut self, stdout: &str) -> Vec<Event> {
        let mut out = Vec::new();
        for line in stdout.lines() {
            let Some((tag, body)) = line.split_once('\t') else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(body) else {
                continue;
            };
            let data = v.get("data").unwrap_or(&Value::Null);
            match tag {
                "L" => self.pick(data, &mut out),
                "S" => self.updated = data.pointer("/time/updated").and_then(Value::as_u64),
                "P" => self.permissions = data.as_array().is_some_and(|a| !a.is_empty()),
                "F" => self.questions = data.as_array().is_some_and(|a| !a.is_empty()),
                _ => {
                    let Some(session) = tag.strip_prefix("M:") else {
                        continue;
                    };
                    let messages = data.as_array().map_or(&[][..], Vec::as_slice);
                    if Some(session) == self.id.as_deref() {
                        self.messages(None, messages, &mut out);
                    } else if let Some(call) = self.children.get(session).cloned() {
                        self.messages(Some((session.to_owned(), call)), messages, &mut out);
                    }
                }
            }
        }
        if self.permissions {
            self.set(
                RunState::Blocked {
                    kind: BlockKind::Permission,
                },
                &mut out,
            );
        } else if self.questions {
            self.set(
                RunState::Blocked {
                    kind: BlockKind::Question,
                },
                &mut out,
            );
        } else if matches!(self.state, Some(RunState::Blocked { .. })) {
            self.set(RunState::Working { progress: None }, &mut out);
        }
        out
    }

    /// [`Self::on_poll`] with `items` as the item feed, as the other readers
    /// take it.
    pub fn on_poll_shared(&mut self, stdout: &str, items: &mut ItemFeed) -> Vec<Event> {
        std::mem::swap(&mut self.items, items);
        let out = self.on_poll(stdout);
        std::mem::swap(&mut self.items, items);
        out
    }

    /// The newest top-level session in the directory the TUI runs in.
    fn pick(&mut self, sessions: &Value, out: &mut Vec<Event>) {
        let newest = sessions
            .as_array()
            .into_iter()
            .flatten()
            .filter(|s| s.get("parentID").is_none_or(Value::is_null))
            .filter(|s| s.pointer("/location/directory").and_then(Value::as_str) == Some(self.cwd.as_str()))
            .max_by_key(|s| s.pointer("/time/updated").and_then(Value::as_u64).unwrap_or(0));
        if let Some(id) = newest.and_then(|s| owned(s, "id")) {
            self.start(id, out);
        }
    }

    fn start(&mut self, id: String, out: &mut Vec<Event>) {
        out.push(Event::SessionStarted {
            harness: Harness::Opencode,
            native_id: id.clone(),
            model: None,
        });
        self.id = Some(id);
    }

    fn set(&mut self, state: RunState, out: &mut Vec<Event>) {
        if self.state.as_ref() == Some(&state) {
            return;
        }
        self.state = Some(state.clone());
        out.push(Event::StateChanged(SessionState {
            state,
            message: None,
            source: StateSource::Transcript,
            observed_at_ms: 0,
        }));
    }

    /// One session's messages, newest first as the server sends them.
    /// `child` is a subagent's session and the call it reports to.
    fn messages(&mut self, child: Option<(String, String)>, messages: &[Value], out: &mut Vec<Event>) {
        let parent = child.as_ref().map(|(_, call)| call.clone());
        for m in messages.iter().rev() {
            let Some(id) = owned(m, "id") else { continue };
            match str_at(m, "type") {
                Some("user") => {
                    if self.handled.insert(id.clone()) && child.is_none() {
                        let text = owned(m, "text").unwrap_or_default();
                        whole_item(
                            &mut self.items,
                            out,
                            id,
                            ItemKind::UserMessage,
                            None,
                            text,
                            Value::Null,
                        );
                        self.set(RunState::Working { progress: None }, out);
                    }
                }
                Some("assistant") => self.assistant(&id, m, parent.clone(), out),
                Some("idle") => {
                    if !self.handled.insert(id) {
                        continue;
                    }
                    match &child {
                        Some((session, _)) => {
                            self.running.remove(session);
                        }
                        None => {
                            let state = match str_at(m, "outcome") {
                                Some("succeeded") if !self.background.is_empty() => RunState::Waiting,
                                Some("succeeded") => RunState::Done,
                                Some("interrupted" | "aborted" | "cancelled") => RunState::Idle,
                                _ => RunState::Error,
                            };
                            self.set(state, out);
                            out.push(Event::TurnCompleted);
                        }
                    }
                }
                // A background subagent reporting back.
                Some("synthetic") if child.is_none() => {
                    if !self.handled.insert(id) {
                        continue;
                    }
                    let Some(session) = m.pointer("/metadata/childID").and_then(Value::as_str) else {
                        continue;
                    };
                    if let Some(call) = self.children.get(session).cloned()
                        && self.background.remove(&call)
                    {
                        let text = owned(m, "text").unwrap_or_default();
                        let status = m
                            .pointer("/metadata/state")
                            .and_then(Value::as_str)
                            .unwrap_or("completed");
                        self.items.complete(
                            call.clone(),
                            json!({
                                "status": if status == "completed" { "completed" } else { "failed" },
                                "content": [{ "type": "content", "content": { "type": "text", "text": text } }],
                                "rawOutput": { "metadata": { "sessionID": session, "status": status } },
                            }),
                            out,
                        );
                        if let Some(s) = self.seen.get_mut(&call) {
                            s.done = true;
                        }
                    }
                }
                _ => {}
            }
        }
    }

    fn assistant(&mut self, msg: &str, m: &Value, parent: Option<String>, out: &mut Vec<Event>) {
        let done = m.pointer("/time/completed").is_some();
        let parts = m
            .get("content")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        if parent.is_none() && !done {
            self.set(RunState::Working { progress: None }, out);
        }
        for (i, p) in parts.iter().enumerate() {
            match str_at(p, "type") {
                Some(kind @ ("text" | "reasoning")) => {
                    let id = format!("{msg}:{i}");
                    let item = if kind == "text" {
                        ItemKind::AgentMessage
                    } else {
                        ItemKind::Reasoning
                    };
                    let finished = done || p.pointer("/time/completed").is_some();
                    self.text(
                        id,
                        item,
                        parent.clone(),
                        str_at(p, "text").unwrap_or_default(),
                        finished,
                        out,
                    );
                }
                Some("tool") => self.tool(p, parent.clone(), out),
                _ => {}
            }
        }
        if done && parent.is_none() && self.counted.insert(msg.to_owned()) {
            self.add_usage(m, out);
        }
    }

    fn text(
        &mut self,
        id: String,
        kind: ItemKind,
        parent: Option<String>,
        text: &str,
        finished: bool,
        out: &mut Vec<Event>,
    ) {
        if text.is_empty() && !finished {
            return;
        }
        let seen = self.seen.entry(id.clone()).or_default();
        if seen.done {
            return;
        }
        if !seen.started {
            seen.started = true;
            self.items.start(id.clone(), kind, parent, Value::Null, out);
        }
        let seen = self.seen.entry(id.clone()).or_default();
        if let Some(more) = text.get(seen.text..).filter(|t| !t.is_empty()) {
            seen.text = text.len();
            self.items.delta(id.clone(), more.to_owned(), out);
        }
        if finished {
            seen.done = true;
            self.items.complete(id, Value::Null, out);
        }
    }

    fn tool(&mut self, p: &Value, parent: Option<String>, out: &mut Vec<Event>) {
        let Some(id) = owned(p, "id") else { return };
        let name = owned(p, "name").unwrap_or_else(|| "tool".into());
        let state = p.get("state").unwrap_or(&Value::Null);
        let input = state.get("input").cloned().unwrap_or(Value::Null);
        let metadata = state.get("metadata").cloned().unwrap_or(Value::Null);
        let status = str_at(state, "status").unwrap_or("pending");
        let kind = acp_kind(&name);
        let text: Vec<&str> = state
            .get("content")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|c| str_at(c, "text"))
            .collect();
        let path = owned(&input, "path").or_else(|| owned(&input, "filePath"));
        let mut native = json!({
            "toolCallId": id,
            "tool": name,
            "title": if kind == "execute" { owned(&input, "command") } else { owned(&input, "description") },
            "kind": kind,
            "status": match status {
                "running" => "in_progress",
                "error" => "failed",
                s => s,
            },
            "rawInput": input,
            "rawOutput": { "metadata": metadata },
        });
        if let Some(code) = metadata.get("exit") {
            native["rawOutput"]["exitCode"] = code.clone();
        }
        if let Some(p) = &path {
            native["locations"] = json!([{ "path": p }]);
        }
        let mut content: Vec<Value> = Vec::new();
        if kind == "edit" {
            let new = ["newString", "new_string", "content"]
                .iter()
                .find_map(|k| input.get(*k));
            let old = ["oldString", "old_string"].iter().find_map(|k| input.get(*k));
            if new.is_some() {
                content.push(json!({ "type": "diff", "path": path, "oldText": old, "newText": new }));
            }
        } else {
            content.extend(
                text.iter()
                    .map(|t| json!({ "type": "content", "content": { "type": "text", "text": t } })),
            );
        }
        if !content.is_empty() {
            native["content"] = Value::Array(content);
        }
        let subagent = is_subagent(&name);
        if subagent
            && let Some(child) = metadata.get("sessionID").and_then(Value::as_str)
            && !self.children.contains_key(child)
        {
            self.children.insert(child.to_owned(), id.clone());
            self.running.insert(child.to_owned());
        }
        // A background launch completes at once; its report comes later.
        let background = subagent
            && input.get("background").and_then(Value::as_bool) == Some(true)
            && metadata.get("status").and_then(Value::as_str) == Some("running");
        let finished = matches!(status, "completed" | "error") && !background;
        let seen = self.seen.entry(id.clone()).or_default();
        if seen.done {
            return;
        }
        let first = !seen.started;
        let changed = seen.native != native;
        seen.started = true;
        seen.tool = true;
        seen.native = native.clone();
        if first {
            let item = if subagent {
                ItemKind::Subagent
            } else {
                match kind {
                    "execute" => ItemKind::CommandExecution,
                    "edit" => ItemKind::FileChange,
                    _ => ItemKind::ToolCall,
                }
            };
            self.items
                .start(id.clone(), item, parent.clone(), native.clone(), out);
        }
        if background {
            self.background.insert(id.clone());
        }
        if finished {
            if let Some(s) = self.seen.get_mut(&id) {
                s.done = true;
            }
            self.items.complete(id, native, out);
        } else if changed && !first {
            self.items.update(id, native, out);
        }
    }

    fn add_usage(&mut self, m: &Value, out: &mut Vec<Event>) {
        let Some(t) = m.get("tokens") else { return };
        let add = |total: &mut Option<u64>, n: Option<u64>| {
            if let Some(n) = n {
                *total = Some(total.unwrap_or(0) + n);
            }
        };
        let input = u64_at(t, "input");
        let output = u64_at(t, "output");
        let read = t.get("cache").and_then(|c| u64_at(c, "read"));
        let write = t.get("cache").and_then(|c| u64_at(c, "write"));
        add(&mut self.usage.input_tokens, input);
        add(&mut self.usage.output_tokens, output);
        add(&mut self.usage.cache_read_tokens, read);
        add(&mut self.usage.cache_write_tokens, write);
        if let Some(cost) = m.get("cost").and_then(Value::as_f64) {
            self.usage.cost_usd = Some(self.usage.cost_usd.unwrap_or(0.0) + cost);
        }
        self.usage.context_used = Some([input, output, read, write].into_iter().flatten().sum());
        out.push(Event::Usage(self.usage.clone()));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use acc_core::event::ItemStatus;
    use acc_core::item::ItemDetail;

    use super::*;

    /// opencode 2.0.14's TUI with Big Pickle: a question answered "Blue",
    /// then a foreground `explore` subagent reading a.txt.
    const MAIN: &str = include_str!("../../tests/fixtures/opencode/main.messages.json");
    const CHILD: &str = include_str!("../../tests/fixtures/opencode/child.messages.json");
    const SESSIONS: &str = include_str!("../../tests/fixtures/opencode/sessions.json");
    const ID: &str = "ses_ebb14c447ffeVaKhJoyBEWqF5S";
    const CHILD_ID: &str = "ses_ebb142aa6ffeK2m1VtSP7yqfov";

    fn tagged(tag: &str, json: &str) -> String {
        format!("{tag}\t{}\n", json.trim())
    }

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
    fn finds_the_newest_session_in_the_tuis_directory() {
        let mut s = Session::new("/tmp/obs-oc", None);
        assert!(s.poll_script().contains("oc L /api/session"));
        let events = s.on_poll(&tagged("L", SESSIONS));
        assert!(matches!(&events[0], Event::SessionStarted { native_id, .. } if native_id == ID));
        assert!(s.poll_script().contains(&format!("/api/session/{ID}/message")));
    }

    #[test]
    fn replays_a_session_with_its_subagent() {
        let mut s = Session::new("/tmp/obs-oc", Some(ID.into()));
        let events = s.on_poll(&format!(
            "{}{}",
            tagged("P", r#"{"data":[]}"#),
            tagged(&format!("M:{ID}"), MAIN)
        ));
        assert_eq!(states(&events).last(), Some(&RunState::Done));
        let call = "call_648d69495acf4396991cf8b8";
        let detail = events.iter().find_map(|e| match e {
            Event::ItemCompleted {
                id, detail, status, ..
            } if id == call => Some((status, detail)),
            _ => None,
        });
        let (
            status,
            ItemDetail::Subagent {
                agent, child, output, ..
            },
        ) = detail.unwrap()
        else {
            panic!("subagent")
        };
        assert_eq!(*status, ItemStatus::Completed);
        assert_eq!(agent.as_deref(), Some("explore"));
        assert_eq!(child.as_deref(), Some(CHILD_ID));
        assert!(output.as_deref().is_some_and(|o| o.contains("hello")));
        // The child session is polled until it goes idle, under the call.
        assert!(
            s.poll_script()
                .contains(&format!("/api/session/{CHILD_ID}/message"))
        );
        let child_events = s.on_poll(&tagged(&format!("M:{CHILD_ID}"), CHILD));
        let parents: Vec<&Option<String>> = child_events
            .iter()
            .filter_map(|e| match e {
                Event::ItemStarted { parent, .. } => Some(parent),
                _ => None,
            })
            .collect();
        assert!(!parents.is_empty() && parents.iter().all(|p| p.as_deref() == Some(call)));
        assert!(
            states(&child_events).is_empty(),
            "a child's idle isn't the session's"
        );
        assert!(!s.poll_script().contains(CHILD_ID), "done: not polled any more");
        // Nothing new: no events.
        assert!(s.on_poll(&tagged(&format!("M:{ID}"), MAIN)).is_empty());
        assert!(s.usage.input_tokens.is_some());
    }

    #[test]
    fn pending_question_and_streaming_text() {
        let mut s = Session::new("/w", Some("ses_1".into()));
        let msgs = |parts: Value, completed: bool| {
            let mut time = json!({"created": 1});
            if completed {
                time["completed"] = json!(2);
            }
            tagged(
                "M:ses_1",
                &json!({"data": [
                    {"id": "m2", "type": "assistant", "time": time, "content": parts},
                    {"id": "m1", "type": "user", "text": "hi"},
                ]})
                .to_string(),
            )
        };
        let first = s.on_poll(&msgs(json!([{"type": "text", "text": "Hel"}]), false));
        assert_eq!(states(&first), [RunState::Working { progress: None }]);
        let more = s.on_poll(&format!(
            "{}{}",
            tagged("F", r#"{"data":[{"id":"frm_1"}]}"#),
            msgs(json!([{"type": "text", "text": "Hello"}]), false)
        ));
        assert!(
            more.iter()
                .any(|e| matches!(e, Event::ItemDelta { text, .. } if text == "lo"))
        );
        assert_eq!(
            states(&more),
            [RunState::Blocked {
                kind: BlockKind::Question
            }]
        );
        let answered = s.on_poll(&format!(
            "{}{}",
            tagged("F", r#"{"data":[]}"#),
            msgs(json!([{"type": "text", "text": "Hello"}]), true)
        ));
        assert_eq!(states(&answered), [RunState::Working { progress: None }]);
        assert!(
            answered
                .iter()
                .any(|e| matches!(e, Event::ItemCompleted { id, .. } if id == "m2:0"))
        );
    }
}
