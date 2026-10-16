//! Observing Cursor Agent sessions ACC didn't start, from their transcripts.
//!
//! The Cursor TUI writes each chat to
//! `~/.cursor/projects/<cwd with non-alphanumerics as dashes>/agent-transcripts/<chat id>/<chat id>.jsonl`:
//! user and assistant messages (`{role, message: {content: [text | tool_use]}}`)
//! and a `{type: "turn_ended", status}` trailer. It is lossy: no tool ids or
//! results, no usage, no timestamps, and an assistant step is written once
//! its tools have run, so a tool call is complete when first seen and an
//! approval dialog leaves no trace (only the screen shows it).
//!
//! The file isn't append-only either. A new turn truncates it back to the
//! start of the trailing `turn_ended` line before writing, and a cancelled
//! turn is removed again, so a byte-offset tail would resume mid-line.
//! [`Transcript`] polls instead ([`Transcript::poll_script`]): it re-reads
//! the file from the last offset nothing rewrites (the start of the last
//! `turn_ended` line) and compares those lines with what it saw last time.

use acc_core::event::{Event, ItemKind};
use acc_core::harness::Harness;
use acc_core::item::ItemFeed;
use acc_core::shell::quote;
use acc_core::state::{RunState, SessionState, StateSource};
use serde_json::{Value, json};

use super::claude::whole_item;
use crate::wire::{owned, str_at};

/// The directory Cursor keeps `cwd`'s transcripts in, relative to `$HOME`.
pub fn transcript_dir(cwd: &str) -> String {
    let encoded: String = cwd
        .trim_start_matches('/')
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    format!(".cursor/projects/{encoded}/agent-transcripts")
}

/// The chat id of a transcript path (`<id>/<id>.jsonl`).
pub fn chat_id_from_path(path: &str) -> Option<&str> {
    let id = path.rsplit('/').next()?.strip_suffix(".jsonl")?;
    (!id.is_empty()).then_some(id)
}

/// Cursor's own tools, as the ACP kind its ACP server reports for them.
fn acp_kind(tool: &str) -> &'static str {
    match tool {
        "Shell" => "execute",
        "Read" | "ReadFile" => "read",
        "Grep" | "Glob" | "rg" => "search",
        "StrReplace" | "Write" | "ApplyPatch" | "EditNotebook" => "edit",
        "Delete" => "delete",
        "WebFetch" | "WebSearch" => "fetch",
        _ => "other",
    }
}

/// The text a user typed: Cursor wraps it in `<user_query>` after a
/// `<timestamp>`.
fn user_query(text: &str) -> String {
    let inner = text
        .split_once("<user_query>")
        .map(|(_, rest)| rest.split("</user_query>").next().unwrap_or(rest))
        .unwrap_or(text);
    inner.trim().to_owned()
}

/// Reads a Cursor transcript by polling it. See the module docs.
#[derive(Debug)]
pub struct Transcript {
    items: ItemFeed,
    /// Bytes before this offset are never rewritten.
    stable: u64,
    /// Complete lines after `stable` as of the last poll.
    tail: Vec<String>,
    /// Lines handled so far, for item ids that stay unique when a cancelled
    /// turn's lines are rewritten.
    seq: u64,
    state: Option<RunState>,
}

impl Default for Transcript {
    fn default() -> Self {
        Self::new()
    }
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            items: ItemFeed::new(Harness::Cursor),
            stable: 0,
            tail: Vec::new(),
            seq: 0,
            state: None,
        }
    }

    pub fn state(&self) -> Option<&RunState> {
        self.state.as_ref()
    }

    /// A turn is under way (the user spoke and it hasn't ended).
    pub fn turn_open(&self) -> bool {
        matches!(self.state, Some(RunState::Working { .. }))
    }

    /// Script printing the transcript's size (`S\t<bytes>`), then the file
    /// from the stable offset. Pass its output to [`Transcript::on_poll`].
    pub fn poll_script(&self, path: &str) -> String {
        let f = quote(path);
        format!(
            "printf 'S\\t%s\\n' \"$(wc -c <{f} 2>/dev/null | tr -d ' ')\"; tail -c +{} {f} 2>/dev/null\n",
            self.stable + 1
        )
    }

    /// The output of [`Transcript::poll_script`].
    pub fn on_poll(&mut self, stdout: &str) -> Vec<Event> {
        let (head, body) = stdout.split_once('\n').unwrap_or((stdout, ""));
        let size = head
            .strip_prefix("S\t")
            .and_then(|s| s.trim().parse::<u64>().ok());
        let Some(size) = size else { return Vec::new() };
        if size < self.stable {
            // Replaced by a shorter file: start over (and repeat its items).
            self.stable = 0;
            self.tail.clear();
            return Vec::new();
        }
        // Only complete lines; the last may still be being written.
        let complete = body.rfind('\n').map_or("", |i| &body[..=i]);
        let lines: Vec<String> = complete.lines().map(str::to_owned).collect();
        let same = self.tail.iter().zip(&lines).take_while(|(a, b)| a == b).count();
        let mut out = Vec::new();
        for line in &lines[same..] {
            self.seq += 1;
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                self.line(&v, &mut out);
            }
        }
        // Everything before a trailing `turn_ended` stays as it is.
        match lines.last() {
            Some(last) if last.contains("\"turn_ended\"") => {
                let before: usize = lines[..lines.len() - 1].iter().map(|l| l.len() + 1).sum();
                self.stable += before as u64;
                self.tail = vec![last.clone()];
            }
            _ => self.tail = lines,
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

    /// Reads a whole transcript at once (for `observer decode`).
    pub fn feed_whole(&mut self, content: &str) -> Vec<Event> {
        self.on_poll(&format!("S\t{}\n{content}", content.len()))
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

    fn line(&mut self, v: &Value, out: &mut Vec<Event>) {
        if str_at(v, "type") == Some("turn_ended") {
            match str_at(v, "status") {
                Some("success") => self.set(RunState::Done, None, out),
                Some("aborted") => self.set(RunState::Idle, None, out),
                _ => {
                    let message = owned(v, "error");
                    self.set(RunState::Error, message, out);
                }
            }
            out.push(Event::TurnCompleted);
            return;
        }
        let blocks = v
            .pointer("/message/content")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let seq = self.seq;
        match str_at(v, "role") {
            Some("user") => {
                let text: Vec<String> = blocks
                    .iter()
                    .filter_map(|b| str_at(b, "text"))
                    .map(user_query)
                    .collect();
                let id = format!("cursor-{seq}");
                whole_item(
                    &mut self.items,
                    out,
                    id,
                    ItemKind::UserMessage,
                    None,
                    text.join("\n"),
                    Value::Null,
                );
                self.set(RunState::Working { progress: None }, None, out);
            }
            Some("assistant") => {
                for (i, b) in blocks.iter().enumerate() {
                    let id = format!("cursor-{seq}-{i}");
                    match str_at(b, "type") {
                        Some("text") => {
                            let text = owned(b, "text").unwrap_or_default();
                            if !text.is_empty() {
                                whole_item(
                                    &mut self.items,
                                    out,
                                    id,
                                    ItemKind::AgentMessage,
                                    None,
                                    text,
                                    Value::Null,
                                );
                            }
                        }
                        Some("tool_use") => self.tool(id, b, out),
                        _ => {}
                    }
                }
                self.set(RunState::Working { progress: None }, None, out);
            }
            _ => {}
        }
    }

    /// A tool call, done by the time it is written.
    fn tool(&mut self, id: String, b: &Value, out: &mut Vec<Event>) {
        let mut name = owned(b, "name").unwrap_or_else(|| "tool".into());
        let mut input = b.get("input").cloned().unwrap_or(Value::Null);
        // Tools loaded on demand run through `CallDynamicTool`.
        if name == "CallDynamicTool" {
            let ns = owned(&input, "namespace");
            let inner = owned(&input, "toolName").unwrap_or_else(|| "tool".into());
            name = match ns.as_deref() {
                Some("cursor") | None => inner,
                Some(ns) => format!("{ns}/{inner}"),
            };
            input = input.get("arguments").cloned().unwrap_or(Value::Null);
        }
        // Looking up those tools is plumbing, not work.
        if name == "GetDynamicTools" || name == "GetMcpTools" {
            return;
        }
        let kind = acp_kind(&name);
        let title = match name.as_str() {
            "Shell" => owned(&input, "command").map(|c| format!("`{c}`")),
            _ => owned(&input, "description"),
        };
        let path = owned(&input, "path");
        let mut native = json!({
            "toolCallId": id,
            "tool": name,
            "title": title,
            "kind": kind,
            "status": "completed",
            "rawInput": input,
        });
        if let Some(p) = &path {
            native["locations"] = json!([{ "path": p }]);
        }
        let item_kind = match name.as_str() {
            "Task" | "Subagent" => ItemKind::Subagent,
            "Shell" => ItemKind::CommandExecution,
            "StrReplace" => {
                native["content"] = json!([{
                    "type": "diff",
                    "path": path,
                    "oldText": input.get("old_string"),
                    "newText": input.get("new_string"),
                }]);
                ItemKind::FileChange
            }
            "Write" => {
                native["content"] =
                    json!([{ "type": "diff", "path": path, "newText": input.get("contents") }]);
                ItemKind::FileChange
            }
            _ if matches!(kind, "edit" | "delete") => ItemKind::FileChange,
            _ => ItemKind::ToolCall,
        };
        self.items.start(id.clone(), item_kind, None, native, out);
        self.items.complete(id, json!({ "status": "completed" }), out);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use acc_core::item::ItemDetail;

    use super::*;

    /// Cursor Agent 2026.10.01 in tmux: an approved shell command, a long
    /// answer cancelled with Escape, then "Say ok.".
    const TRANSCRIPT: &str = include_str!("../../tests/fixtures/cursor/transcript.jsonl");

    fn poll(t: &mut Transcript, file: &str) -> Vec<Event> {
        let from = usize::try_from(t.stable).unwrap();
        t.on_poll(&format!("S\t{}\n{}", file.len(), &file[from.min(file.len())..]))
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

    fn line(v: Value) -> String {
        format!("{v}\n")
    }

    #[test]
    fn replays_a_recorded_transcript() {
        let mut t = Transcript::new();
        let events = t.feed_whole(TRANSCRIPT);
        let shell = events.iter().find_map(|e| match e {
            Event::ItemCompleted {
                detail: ItemDetail::Command { command, .. },
                ..
            } => Some(command.clone()),
            _ => None,
        });
        assert_eq!(shell.as_deref(), Some("echo bye > b.txt"));
        let user = events.iter().find_map(|e| match e {
            Event::ItemDelta { id, text } if id == "cursor-1" => Some(text.clone()),
            _ => None,
        });
        assert_eq!(user.as_deref(), Some("Run the shell command 'echo bye > b.txt'."));
        assert_eq!(
            states(&events),
            [RunState::Working { progress: None }, RunState::Done]
        );
    }

    /// What the file went through while recording: each new turn drops the
    /// last `turn_ended`, and the cancelled turn's prompt was removed again.
    #[test]
    fn follows_a_file_cursor_rewrites() {
        let user = |q: &str| {
            line(json!({"role": "user", "message": {"content": [{"type": "text",
                "text": format!("<timestamp>Friday</timestamp>\n<user_query>\n{q}\n</user_query>")}]}}))
        };
        let said = |s: &str| {
            line(json!({"role": "assistant", "message": {"content": [{"type": "text", "text": s}]}}))
        };
        let ended = |s: &str| line(json!({"type": "turn_ended", "status": s}));
        let turn1 = format!("{}{}", user("one"), said("did one"));

        let mut t = Transcript::new();
        assert_eq!(
            states(&poll(&mut t, &user("one"))),
            [RunState::Working { progress: None }]
        );
        assert_eq!(
            states(&poll(&mut t, &format!("{turn1}{}", ended("success")))),
            [RunState::Done]
        );
        assert!(
            poll(&mut t, &format!("{turn1}{}", ended("success"))).is_empty(),
            "no news"
        );
        // A second prompt, then cancelled: its line goes, an abort trailer comes.
        let second = poll(&mut t, &format!("{turn1}{}", user("essay")));
        assert_eq!(states(&second), [RunState::Working { progress: None }]);
        let aborted = poll(&mut t, &format!("{turn1}{}", ended("aborted")));
        assert_eq!(states(&aborted), [RunState::Idle]);
        // And a third turn after it.
        let third = poll(
            &mut t,
            &format!("{turn1}{}{}{}", user("ok?"), said("ok"), ended("success")),
        );
        assert_eq!(
            states(&third),
            [RunState::Working { progress: None }, RunState::Done]
        );
        let ids: Vec<&String> = third
            .iter()
            .filter_map(|e| match e {
                Event::ItemStarted { id, .. } => Some(id),
                _ => None,
            })
            .collect();
        assert_eq!(ids.len(), 2, "only the new turn's items");
    }

    #[test]
    fn dynamic_tools_unwrap_and_plumbing_is_skipped() {
        let file = line(json!({"role": "assistant", "message": {"content": [
            {"type": "tool_use", "name": "GetDynamicTools", "input": {"namespace": "cursor", "toolName": "Task"}},
            {"type": "tool_use", "name": "CallDynamicTool", "input": {"namespace": "cursor", "toolName": "Task",
                "arguments": {"description": "Explore it", "subagent_type": "explore", "prompt": "look"}}},
            {"type": "tool_use", "name": "StrReplace", "input": {"path": "/w/a.rs", "old_string": "a\n", "new_string": "b\n"}},
        ]}}));
        let events = Transcript::new().feed_whole(&file);
        let details: Vec<&ItemDetail> = events
            .iter()
            .filter_map(|e| match e {
                Event::ItemCompleted { detail, .. } => Some(detail),
                _ => None,
            })
            .collect();
        assert_eq!(details.len(), 2);
        assert!(matches!(details[0], ItemDetail::Subagent { agent: Some(a), .. } if a == "explore"));
        assert!(matches!(details[1], ItemDetail::FileChange { files, .. } if files[0].path == "/w/a.rs"));
    }

    #[test]
    fn paths_and_ids() {
        assert_eq!(
            transcript_dir("/tmp/obs-cu"),
            ".cursor/projects/tmp-obs-cu/agent-transcripts"
        );
        assert_eq!(
            transcript_dir("/home/u/Documents/lapp"),
            ".cursor/projects/home-u-Documents-lapp/agent-transcripts"
        );
        assert_eq!(
            chat_id_from_path("/h/.cursor/projects/p/agent-transcripts/c1/c1.jsonl"),
            Some("c1")
        );
    }
}
