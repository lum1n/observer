//! Observing Copilot CLI sessions ACC didn't start, from their event logs.
//!
//! Copilot records every session, whichever front end runs it (the TUI,
//! `-p`, ACP), to `~/.copilot/session-state/<session id>/events.jsonl`: one
//! typed event per line (`user.message`, `assistant.message`,
//! `tool.execution_start`, `permission.requested`, ...). A subagent's events
//! go to the same file, marked with its `agentId`, and its tool calls name
//! the `task` call that runs it as `parentToolCallId`. Items are shaped like
//! the tool calls Copilot's ACP server reports, so an observed session
//! renders like one ACC drives.

use std::collections::{HashMap, HashSet};

use acc_core::event::{Event, ItemKind, Usage};
use acc_core::harness::Harness;
use acc_core::item::ItemFeed;
use acc_core::ndjson::{Frame, LineFramer};
use acc_core::state::{BlockKind, RunState, SessionState, StateSource};
use serde_json::{Value, json};

use super::claude::whole_item;
use crate::wire::{self, owned, str_at, u64_at};

/// Where Copilot keeps its sessions, relative to `$HOME`.
pub const SESSION_STATE: &str = ".copilot/session-state";

/// The session id of an `events.jsonl` path (its directory's name).
pub fn session_id_from_path(path: &str) -> Option<&str> {
    let dir = path.strip_suffix("/events.jsonl")?;
    let id = dir.rsplit('/').next()?;
    (!id.is_empty()).then_some(id)
}

/// The ACP tool kind Copilot reports for one of its tools.
fn acp_kind(tool: &str) -> &'static str {
    match tool {
        "bash" | "powershell" => "execute",
        "view" => "read",
        "create" | "edit" | "apply_patch" => "edit",
        "grep" | "glob" => "search",
        "web_fetch" | "web_search" => "fetch",
        _ => "other",
    }
}

/// Tools that wait on the user.
fn asks_user(tool: &str) -> bool {
    matches!(tool, "ask_user")
}

/// An open tool call.
#[derive(Debug)]
struct Open {
    tool: String,
    /// Run by a subagent rather than the session itself.
    nested: bool,
}

/// Turns Copilot's event log into transcript-sourced items, state and usage.
#[derive(Debug)]
pub struct Observer {
    items: ItemFeed,
    framer: LineFramer,
    started: bool,
    state: Option<RunState>,
    open: HashMap<String, Open>,
    /// Titles from the assistant message that requested each call.
    intents: HashMap<String, String>,
    /// Subagent id → the `task` call running it.
    agents: HashMap<String, String>,
    /// `task` calls launched in the background, not yet reported back.
    background: HashSet<String>,
    /// Open permission requests and questions.
    waiting: HashSet<String>,
    /// The session's last assistant message asked for tools.
    wants_tools: bool,
    usage: Usage,
}

impl Default for Observer {
    fn default() -> Self {
        Self::new()
    }
}

impl Observer {
    pub fn new() -> Self {
        Self {
            items: ItemFeed::new(Harness::Copilot),
            framer: LineFramer::new(wire::MAX_FRAME),
            started: false,
            state: None,
            open: HashMap::new(),
            intents: HashMap::new(),
            agents: HashMap::new(),
            background: HashSet::new(),
            waiting: HashSet::new(),
            wants_tools: false,
            usage: Usage::default(),
        }
    }

    pub fn state(&self) -> Option<&RunState> {
        self.state.as_ref()
    }

    /// The session's own tool calls with no result yet.
    pub fn pending_tools(&self) -> usize {
        self.open.values().filter(|o| !o.nested).count()
    }

    /// Background subagents still running; they die with the TUI.
    pub fn background_agents(&self) -> usize {
        self.background.len()
    }

    pub fn feed(&mut self, bytes: &[u8]) -> Vec<Event> {
        let mut out = Vec::new();
        for frame in self.framer.feed(bytes) {
            if let Frame::Line(line) = frame
                && let Ok(v) = serde_json::from_str::<Value>(&line)
            {
                self.event(&v, &mut out);
            }
        }
        out
    }

    /// [`Self::feed`] with `items` as the item feed, as the other readers
    /// take it (Copilot's subagents write to the same file).
    pub fn feed_shared(&mut self, bytes: &[u8], items: &mut ItemFeed) -> Vec<Event> {
        std::mem::swap(&mut self.items, items);
        let out = self.feed(bytes);
        std::mem::swap(&mut self.items, items);
        out
    }

    fn set(&mut self, state: RunState, message: Option<String>, out: &mut Vec<Event>) {
        if self.state.as_ref() == Some(&state) && message.is_none() {
            return;
        }
        self.state = Some(state.clone());
        out.push(Event::StateChanged(SessionState {
            state,
            message,
            source: StateSource::Transcript,
            observed_at_ms: 0,
        }));
    }

    fn working(&mut self, out: &mut Vec<Event>) {
        if self.waiting.is_empty() {
            self.set(RunState::Working { progress: None }, None, out);
        }
    }

    fn event(&mut self, v: &Value, out: &mut Vec<Event>) {
        let null = Value::Null;
        let d = v.get("data").unwrap_or(&null);
        // The `task` call a subagent's event belongs to.
        let parent = owned(d, "parentToolCallId")
            .or_else(|| str_at(v, "agentId").and_then(|a| self.agents.get(a).cloned()));
        let nested = v.get("agentId").is_some() || parent.is_some();
        match str_at(v, "type").unwrap_or_default() {
            "session.start" | "session.resume" if !self.started => {
                let id = owned(d, "sessionId").or_else(|| owned(v, "sessionId"));
                if let Some(native_id) = id {
                    self.started = true;
                    out.push(Event::SessionStarted {
                        harness: Harness::Copilot,
                        native_id,
                        model: owned(d, "selectedModel").filter(|m| m != "auto"),
                    });
                }
            }
            "session.title_changed" => {
                if let Some(title) = owned(d, "title") {
                    out.push(Event::Native {
                        kind: "title".into(),
                        payload: json!({ "title": title }),
                    });
                }
            }
            // A subagent's prompt is its call's input.
            "user.message" if !nested => {
                let id = owned(d, "messageId").unwrap_or_else(|| owned(v, "id").unwrap_or_default());
                let text = owned(d, "content").unwrap_or_default();
                whole_item(
                    &mut self.items,
                    out,
                    id,
                    ItemKind::UserMessage,
                    None,
                    text,
                    Value::Null,
                );
                self.wants_tools = false;
                self.working(out);
            }
            "assistant.turn_start" if !nested => self.working(out),
            "assistant.reasoning" => {
                let id = owned(d, "reasoningId").unwrap_or_else(|| owned(v, "id").unwrap_or_default());
                let text = owned(d, "content").unwrap_or_default();
                if !text.is_empty() {
                    whole_item(
                        &mut self.items,
                        out,
                        id,
                        ItemKind::Reasoning,
                        parent,
                        text,
                        Value::Null,
                    );
                }
            }
            "assistant.message" => self.assistant(v, d, parent, nested, out),
            "tool.execution_start" => self.tool_start(d, parent, out),
            "tool.execution_complete" => self.tool_complete(d, out),
            "subagent.started" => {
                if let (Some(agent), Some(call)) = (owned(v, "agentId"), owned(d, "toolCallId")) {
                    if str_at(d, "executionMode") == Some("background") {
                        self.background.insert(call.clone());
                    }
                    self.items.update(
                        call.clone(),
                        json!({ "status": "in_progress", "rawOutput": { "sessionId": agent } }),
                        out,
                    );
                    self.agents.insert(agent, call);
                }
            }
            kind @ ("subagent.completed" | "subagent.failed") => {
                if let Some(call) = owned(d, "toolCallId")
                    && self.background.remove(&call)
                {
                    let status = if kind == "subagent.failed" {
                        "failed"
                    } else {
                        "completed"
                    };
                    let agent = owned(v, "agentId");
                    self.items.complete(
                        call,
                        json!({ "status": status, "rawOutput": { "sessionId": agent } }),
                        out,
                    );
                }
            }
            "permission.requested" => {
                if let Some(id) = owned(d, "requestId") {
                    self.waiting.insert(id);
                    self.set(
                        RunState::Blocked {
                            kind: BlockKind::Permission,
                        },
                        None,
                        out,
                    );
                }
            }
            "user_input.requested" | "elicitation.requested" => {
                let id = owned(d, "requestId").unwrap_or_else(|| owned(v, "id").unwrap_or_default());
                self.waiting.insert(id);
                self.set(
                    RunState::Blocked {
                        kind: BlockKind::Question,
                    },
                    None,
                    out,
                );
            }
            "permission.completed" | "user_input.completed" | "elicitation.completed" => {
                if let Some(id) = owned(d, "requestId") {
                    self.waiting.remove(&id);
                }
                self.working(out);
            }
            "session.usage_record" if !nested => self.usage(d, out),
            "assistant.turn_end" if !nested => {
                if !self.wants_tools && self.pending_tools() == 0 {
                    let done = if self.background.is_empty() {
                        RunState::Done
                    } else {
                        RunState::Waiting
                    };
                    self.set(done, None, out);
                    out.push(Event::TurnCompleted);
                }
            }
            "abort" => {
                self.close_all(out);
                self.set(RunState::Idle, None, out);
                out.push(Event::TurnCompleted);
            }
            "session.error" => {
                let message = str_at(d, "message")
                    .and_then(|m| m.lines().next())
                    .map(str::to_owned);
                self.set(RunState::Error, message, out);
            }
            _ => {}
        }
    }

    fn assistant(
        &mut self,
        v: &Value,
        d: &Value,
        parent: Option<String>,
        nested: bool,
        out: &mut Vec<Event>,
    ) {
        let id = owned(d, "messageId").unwrap_or_else(|| owned(v, "id").unwrap_or_default());
        let text = owned(d, "content").unwrap_or_default();
        let requests = d
            .get("toolRequests")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        for r in requests {
            if let (Some(call), Some(intent)) = (owned(r, "toolCallId"), owned(r, "intentionSummary")) {
                self.intents.insert(call, intent);
            }
        }
        if !nested {
            self.wants_tools = !requests.is_empty();
        }
        if !text.is_empty() {
            whole_item(
                &mut self.items,
                out,
                id,
                ItemKind::AgentMessage,
                parent,
                text,
                Value::Null,
            );
        }
    }

    fn tool_start(&mut self, d: &Value, parent: Option<String>, out: &mut Vec<Event>) {
        let Some(call) = owned(d, "toolCallId") else {
            return;
        };
        let tool = owned(d, "toolName").unwrap_or_else(|| "tool".into());
        let input = d.get("arguments").cloned().unwrap_or(Value::Null);
        let kind = acp_kind(&tool);
        let title = owned(&input, "description")
            .or_else(|| self.intents.remove(&call))
            .unwrap_or_else(|| tool.clone());
        let mut native = json!({
            "toolCallId": call,
            "tool": tool,
            "title": title,
            "kind": kind,
            "status": "pending",
            "rawInput": input,
        });
        let path = owned(&input, "path");
        if let Some(p) = &path {
            native["locations"] = json!([{ "path": p }]);
        }
        let item_kind = match (tool.as_str(), kind) {
            ("task", _) => ItemKind::Subagent,
            (_, "execute") => ItemKind::CommandExecution,
            ("create", _) => {
                native["content"] = json!([{
                    "type": "diff",
                    "path": path,
                    "newText": input.get("file_text"),
                }]);
                ItemKind::FileChange
            }
            ("edit", _) => {
                native["content"] = json!([{
                    "type": "diff",
                    "path": path,
                    "oldText": input.get("old_str"),
                    "newText": input.get("new_str"),
                }]);
                ItemKind::FileChange
            }
            (_, "edit") => ItemKind::FileChange,
            _ => ItemKind::ToolCall,
        };
        if asks_user(&tool) {
            self.waiting.insert(call.clone());
            self.set(
                RunState::Blocked {
                    kind: BlockKind::Question,
                },
                None,
                out,
            );
        }
        self.open.insert(
            call.clone(),
            Open {
                tool,
                nested: parent.is_some(),
            },
        );
        self.items.start(call, item_kind, parent, native, out);
    }

    fn tool_complete(&mut self, d: &Value, out: &mut Vec<Event>) {
        let Some(call) = owned(d, "toolCallId") else {
            return;
        };
        let Some(open) = self.open.remove(&call) else {
            return;
        };
        let result = d.get("result").cloned().unwrap_or(Value::Null);
        let text = str_at(&result, "content").unwrap_or_default();
        let ok = d.get("success").and_then(Value::as_bool).unwrap_or(true);
        let mut native = json!({
            "status": if ok { "completed" } else { "failed" },
            "rawOutput": result,
        });
        if let Some(code) = d.pointer("/shellExecution/exitCode") {
            native["rawOutput"]["exitCode"] = code.clone();
        }
        if open.tool == "task" {
            let agent = self
                .agents
                .iter()
                .find(|(_, c)| **c == call)
                .map(|(a, _)| a.clone());
            native["rawOutput"]["sessionId"] = json!(agent);
        }
        let file_change = matches!(open.tool.as_str(), "create" | "edit");
        if !text.is_empty() && !file_change {
            native["content"] = json!([{ "type": "content", "content": { "type": "text", "text": text } }]);
        }
        if self.waiting.remove(&call) {
            self.working(out);
        }
        if self.background.contains(&call) {
            // Launched: the call reports back when the subagent finishes.
            self.items.update(call, native, out);
        } else {
            self.items.complete(call, native, out);
        }
    }

    /// Ends every open call: the turn was cancelled.
    fn close_all(&mut self, out: &mut Vec<Event>) {
        let mut calls: Vec<String> = self.open.drain().map(|(c, _)| c).collect();
        calls.sort();
        for call in calls {
            self.items.complete(call, json!({ "status": "failed" }), out);
        }
        self.waiting.clear();
        self.wants_tools = false;
    }

    fn usage(&mut self, d: &Value, out: &mut Vec<Event>) {
        let Some(u) = d.get("usage") else { return };
        let add = |total: &mut Option<u64>, key: &str| {
            if let Some(n) = u64_at(u, key) {
                *total = Some(total.unwrap_or(0) + n);
            }
        };
        add(&mut self.usage.input_tokens, "inputTokens");
        add(&mut self.usage.output_tokens, "outputTokens");
        add(&mut self.usage.cache_read_tokens, "cacheReadTokens");
        add(&mut self.usage.cache_write_tokens, "cacheWriteTokens");
        // Input counts the cached prompt too: it is the context so far.
        let input = u64_at(u, "inputTokens");
        let output = u64_at(u, "outputTokens").unwrap_or(0);
        self.usage.context_used = input.map(|i| i + output);
        self.usage.context_window = u64_at(u, "maxPromptTokens").or(self.usage.context_window);
        out.push(Event::Usage(self.usage.clone()));
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use acc_core::event::ItemStatus;
    use acc_core::item::ItemDetail;

    use super::*;

    /// Copilot CLI 1.0.98 in tmux: a shell call, an approved write, an
    /// `ask_user` question, a sync `explore` subagent, then resumed and a
    /// slow command cancelled with Ctrl+C.
    const EVENTS: &str = include_str!("../../tests/fixtures/copilot/events.jsonl");

    fn states(events: &[Event]) -> Vec<RunState> {
        let mut out: Vec<RunState> = Vec::new();
        for e in events {
            if let Event::StateChanged(s) = e
                && out.last() != Some(&s.state)
            {
                out.push(s.state.clone());
            }
        }
        out
    }

    fn completed<'a>(events: &'a [Event], id: &str) -> Option<(&'a ItemStatus, &'a ItemDetail)> {
        events.iter().find_map(|e| match e {
            Event::ItemCompleted {
                id: i,
                status,
                detail,
                ..
            } if i == id => Some((status, detail)),
            _ => None,
        })
    }

    #[test]
    fn replays_a_recorded_session() {
        let mut o = Observer::new();
        let events = o.feed(EVENTS.as_bytes());
        assert!(matches!(
            &events[0],
            Event::SessionStarted { native_id, .. } if native_id == "518fde60-17de-4177-9500-b278d6770baa"
        ));
        let working = RunState::Working { progress: None };
        assert_eq!(
            states(&events),
            [
                working.clone(),
                RunState::Done,
                working.clone(),
                RunState::Blocked {
                    kind: BlockKind::Permission
                },
                working.clone(),
                RunState::Done,
                working.clone(),
                RunState::Blocked {
                    kind: BlockKind::Question
                },
                working.clone(),
                RunState::Done,
                working.clone(),
                RunState::Done,
                working,
                RunState::Idle,
            ]
        );
        let (_, cat) = completed(&events, "call_BjMjxgzKPGmrlQBWa3ZUtFIB").unwrap();
        let ItemDetail::Command {
            command,
            output,
            exit_code,
            ..
        } = cat
        else {
            panic!("command: {cat:?}")
        };
        assert_eq!(command, "cd /tmp/obs-cp && cat a.txt");
        assert!(output.starts_with("hello"));
        assert_eq!(*exit_code, Some(0));
        assert_eq!(o.pending_tools(), 0, "the cancelled call is closed");
    }

    #[test]
    fn subagent_items_hang_under_the_task_call() {
        let events = Observer::new().feed(EVENTS.as_bytes());
        let task = "call_Vj6rpdPijjALZnFN1IgEBsBq";
        let view = events.iter().find_map(|e| match e {
            Event::ItemStarted { id, parent, .. } if id == "call_1vCuv85TuQumpTR0H7PBGjFz" => Some(parent),
            _ => None,
        });
        assert_eq!(view, Some(&Some(task.to_owned())));
        let (status, detail) = completed(&events, task).unwrap();
        assert_eq!(*status, ItemStatus::Completed);
        let ItemDetail::Subagent {
            child,
            prompt,
            output,
            ..
        } = detail
        else {
            panic!("subagent: {detail:?}")
        };
        assert_eq!(child.as_deref(), Some("7665f591-d2dc-4491-9640-6b64f3741d97"));
        assert!(prompt.as_deref().is_some_and(|p| p.contains("a.txt")));
        assert_eq!(output.as_deref(), Some("hello"));
        // The subagent's usage isn't the session's.
        let last = events.iter().rev().find_map(|e| match e {
            Event::Usage(u) => Some(u),
            _ => None,
        });
        assert!(last.unwrap().context_window.is_some());
    }

    #[test]
    fn background_subagent_keeps_the_session_waiting() {
        let lines = [
            json!({"type": "user.message", "data": {"content": "go", "messageId": "u1"}}),
            json!({"type": "assistant.message", "data": {"messageId": "m1", "content": "",
                "toolRequests": [{"toolCallId": "t1", "name": "task"}]}}),
            json!({"type": "tool.execution_start", "data": {"toolCallId": "t1", "toolName": "task",
                "arguments": {"description": "Read it", "prompt": "read a.txt", "agent_type": "explore", "mode": "background"}}}),
            json!({"type": "subagent.started", "agentId": "ag", "data": {"toolCallId": "t1", "executionMode": "background"}}),
            json!({"type": "tool.execution_complete", "data": {"toolCallId": "t1", "success": true,
                "result": {"content": "Agent started in background"}}}),
            json!({"type": "assistant.turn_end", "data": {"turnId": "0"}}),
            json!({"type": "assistant.message", "data": {"messageId": "m2", "content": "launched", "toolRequests": []}}),
            json!({"type": "assistant.turn_end", "data": {"turnId": "1"}}),
            json!({"type": "subagent.completed", "agentId": "ag", "data": {"toolCallId": "t1"}}),
        ];
        let text: String = lines.iter().map(|l| format!("{l}\n")).collect();
        let mut o = Observer::new();
        let events = o.feed(text.as_bytes());
        assert_eq!(
            states(&events),
            [RunState::Working { progress: None }, RunState::Waiting]
        );
        let launched = events
            .iter()
            .position(|e| matches!(e, Event::ItemUpdated { id, .. } if id == "t1"));
        let reported = events
            .iter()
            .position(|e| matches!(e, Event::ItemCompleted { id, .. } if id == "t1"));
        assert!(launched < reported && launched.is_some());
        assert_eq!(o.background_agents(), 0);
    }

    #[test]
    fn session_id_from_its_directory() {
        assert_eq!(
            session_id_from_path("/home/u/.copilot/session-state/518fde60/events.jsonl"),
            Some("518fde60")
        );
        assert_eq!(session_id_from_path("/x/events.json"), None);
    }
}
