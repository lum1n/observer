//! Rex (Superlogical, invite beta since 2026-10-05) through its `rex` CLI.
//!
//! Rex answers the OSC 7501 probe and keeps each terminal's program-status
//! records, which `rex events --json` reports as `program_status_changed`. So
//! inside Rex, Claude Code and pi report state natively even when ACC didn't
//! start them. The shapes here were checked against rex 0.0.1792519967 (API
//! revision 4) on macOS. Scope flags (`-s`, `-b`) belong to the subcommand,
//! not to `rex` itself, and a block id only resolves inside its session.

use acc_core::shell::join;
use serde_json::Value;

use super::{
    Backend, Exec, Input, Key, Multiplexer, MuxError, PaneInfo, PaneRef, Program, StatusUpdate, StreamKind,
    Target, Watch, status_update,
};
use crate::osc7501::{self, Osc7501};

/// The Rex backend.
#[derive(Debug, Clone, Copy, Default)]
pub struct Rex;

const SESSION_MARKER: &str = "#acc-session ";
const PROCESS_MARKER: &str = "#acc-process ";
const TITLE_MARKER: &str = "#acc-title ";

/// The macOS app keeps `rex` inside its bundle and only puts it on PATH in
/// Rex's own terminals, so an SSH shell looks there too.
const PATH_LINE: &str = "PATH=\"$PATH:$HOME/Library/Application Support/com.superlogical.rex/RexServer/Versions/Current/Rex Server.app/Contents/MacOS\"\n";

/// `rex [-S server] <cmd...> [-s session] <args...>`.
fn rex(server: &str, cmd: &[&str], session: Option<&str>, args: &[&str]) -> String {
    let mut argv = vec!["rex"];
    if !server.is_empty() {
        argv.extend(["-S", server]);
    }
    argv.extend_from_slice(cmd);
    if let Some(s) = session {
        argv.extend(["-s", s]);
    }
    argv.extend_from_slice(args);
    join(argv)
}

fn script(body: String) -> Exec {
    Exec::new(format!("{PATH_LINE}{body}"))
}

fn key_name(key: Key) -> String {
    match key {
        Key::Enter => "enter".into(),
        Key::Escape => "escape".into(),
        Key::Tab => "tab".into(),
        Key::Backspace => "backspace".into(),
        Key::Up => "up".into(),
        Key::Down => "down".into(),
        Key::Left => "left".into(),
        Key::Right => "right".into(),
        Key::Ctrl(c) => format!("ctrl+{}", c.to_ascii_lowercase()),
        Key::Char(c) => c.to_string(),
    }
}

/// Collects every object with a `block_id` from a JSON tree.
fn blocks<'a>(v: &'a Value, out: &mut Vec<&'a serde_json::Map<String, Value>>) {
    match v {
        Value::Object(m) => {
            if m.get("block_id").and_then(Value::as_str).is_some() {
                out.push(m);
            }
            m.values().for_each(|c| blocks(c, out));
        }
        Value::Array(a) => a.iter().for_each(|c| blocks(c, out)),
        _ => {}
    }
}

/// What to call a process from Rex's `process` result: the first of
/// `script`, `argv0` and `name` (Rex's advice; `name` is the executable's
/// file name, `2.1.308` for Claude Code), without a login shell's `-` or a
/// directory.
pub fn process_label(p: &Value) -> Option<String> {
    let raw = ["script", "argv0", "name"]
        .iter()
        .find_map(|k| p.get(*k).and_then(Value::as_str).filter(|s| !s.is_empty()))?;
    let base = raw.rsplit('/').next().unwrap_or(raw);
    let base = base.strip_prefix('-').unwrap_or(base);
    (!base.is_empty()).then(|| base.to_owned())
}

/// Shell for one session: its view, then each block's process and title.
/// `$s` holds the session.
fn session_script(server: &str) -> String {
    let base = rex(server, &[], None, &[]);
    let call = |m: &str| format!("{base} block call -s \"$s\" -b \"$b\" {m} </dev/null 2>/dev/null");
    format!(
        "v=$({base} api call -s \"$s\" session.view </dev/null) || continue\n\
         printf '{SESSION_MARKER}%s\\n%s\\n' \"$s\" \"$v\"\n\
         printf '%s\\n' \"$v\" | grep -o '\"block_id\" *: *\"[^\"]*\"' | sed 's/.*\"\\([^\"]*\\)\"$/\\1/' | sort -u | \
         while IFS= read -r b; do\n\
         printf '{PROCESS_MARKER}%s\\n' \"$b\"; {}; echo\n\
         printf '{TITLE_MARKER}%s\\n' \"$b\"; {}; echo\n\
         done\n",
        call("process"),
        call("title"),
    )
}

impl Backend for Rex {
    fn multiplexer(&self) -> Multiplexer {
        Multiplexer::Rex
    }

    fn list_panes(&self, target: &Target) -> Exec {
        let server = target.server.as_str();
        let each = session_script(server);
        script(match &target.session {
            Some(s) => format!("for s in {}; do\n{each}done\n", acc_core::shell::quote(s)),
            None => format!(
                "{} | tr ',{{' '\\n\\n' | sed -n 's/.*\"session_id\" *: *\"\\([^\"]*\\)\".*/\\1/p' | \
                 while IFS= read -r s; do\n{each}done\n",
                rex(server, &["ls"], None, &["--json", "--autostart=false"]),
            ),
        })
    }

    fn parse_panes(&self, target: &Target, stdout: &str) -> Result<Vec<PaneInfo>, MuxError> {
        enum Part {
            None,
            View(Option<String>),
            Process(String),
            Title(String),
        }
        let mut panes: Vec<PaneInfo> = Vec::new();
        let mut part = Part::None;
        let mut buf = String::new();
        let flush = |part: &Part, buf: &mut String, panes: &mut Vec<PaneInfo>| -> Result<(), MuxError> {
            let text = std::mem::take(buf);
            if text.trim().is_empty() {
                return Ok(());
            }
            match part {
                Part::None => Ok(()),
                Part::View(session) => {
                    let v: Value = serde_json::from_str(&text)
                        .map_err(|e| MuxError::Parse("rex session.view", e.to_string()))?;
                    let focused: Vec<&str> = v
                        .get("windows")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter(|w| w.get("window_id") == v.get("active_window_id"))
                        .filter_map(|w| w.get("focused_block_id").and_then(Value::as_str))
                        .collect();
                    let mut found = Vec::new();
                    blocks(&v, &mut found);
                    for b in found {
                        // Layout nodes carry a block_id too; the block itself has a creator.
                        if b.get("creator_name").is_none() {
                            continue;
                        }
                        let id = b.get("block_id").and_then(Value::as_str).unwrap_or_default();
                        if panes
                            .iter()
                            .any(|p| p.pane.pane == id && p.pane.session == *session)
                        {
                            continue;
                        }
                        panes.push(PaneInfo {
                            pane: PaneRef {
                                multiplexer: Multiplexer::Rex,
                                server: target.server.clone(),
                                session: session.clone(),
                                pane: id.to_owned(),
                            },
                            pid: None,
                            tty: None,
                            command: None,
                            cwd: None,
                            title: b.get("label").and_then(Value::as_str).map(str::to_owned),
                            active: focused.contains(&id),
                            exited: false,
                        });
                    }
                    Ok(())
                }
                // A block that isn't a terminal has no process or title: skip what doesn't parse.
                Part::Process(block) => {
                    let Ok(v) = serde_json::from_str::<Value>(&text) else {
                        return Ok(());
                    };
                    if let Some(p) = panes.iter_mut().rev().find(|p| p.pane.pane == *block) {
                        let fg = v.get("foreground").filter(|f| f.is_object());
                        p.pid = v
                            .pointer("/child/pid")
                            .and_then(Value::as_u64)
                            .and_then(|n| u32::try_from(n).ok());
                        p.command = fg.and_then(process_label);
                        p.cwd = fg
                            .or_else(|| v.get("child"))
                            .and_then(|f| f.get("cwd"))
                            .and_then(Value::as_str)
                            .map(str::to_owned);
                        p.exited = v.get("last_exit").is_some_and(|e| !e.is_null()) && fg.is_none();
                    }
                    Ok(())
                }
                Part::Title(block) => {
                    let Ok(v) = serde_json::from_str::<Value>(&text) else {
                        return Ok(());
                    };
                    if let (Some(t), Some(p)) = (
                        v.get("title").and_then(Value::as_str).filter(|t| !t.is_empty()),
                        panes.iter_mut().rev().find(|p| p.pane.pane == *block),
                    ) {
                        p.title = Some(t.to_owned());
                    }
                    Ok(())
                }
            }
        };
        for line in stdout.lines() {
            let next = if let Some(s) = line.strip_prefix(SESSION_MARKER) {
                Some(Part::View(Some(s.to_owned())))
            } else if let Some(b) = line.strip_prefix(PROCESS_MARKER) {
                Some(Part::Process(b.to_owned()))
            } else {
                line.strip_prefix(TITLE_MARKER).map(|b| Part::Title(b.to_owned()))
            };
            match next {
                Some(n) => {
                    flush(&part, &mut buf, &mut panes)?;
                    part = n;
                }
                None => {
                    // Output without markers is one session.view of the target's session.
                    if matches!(part, Part::None) {
                        part = Part::View(target.session.clone());
                    }
                    buf.push_str(line);
                    buf.push('\n');
                }
            }
        }
        flush(&part, &mut buf, &mut panes)?;
        Ok(panes)
    }

    /// `rex capture` prints the screen only, so `scrollback` is ignored.
    fn capture(&self, pane: &PaneRef, _scrollback: bool) -> Exec {
        script(rex(
            &pane.server,
            &["capture"],
            pane.session.as_deref(),
            &["-b", &pane.pane, "--trim", "--unwrap"],
        ))
    }

    fn send(&self, pane: &PaneRef, input: &Input) -> Exec {
        let x = |cmd: &str, args: &[&str]| rex(&pane.server, &[cmd], pane.session.as_deref(), args);
        let b = pane.pane.as_str();
        let with_stdin = |bytes: Vec<u8>| {
            let mut e = script(x("send", &["-b", b]));
            e.stdin = bytes;
            e
        };
        match input {
            // `rex send` reads stdin when given no text and adds no Enter.
            Input::Text(t) => with_stdin(t.as_bytes().to_vec()),
            Input::Paste(t) => with_stdin([b"\x1b[200~", t.as_bytes(), b"\x1b[201~"].concat()),
            Input::Keys(keys) => script(
                keys.iter()
                    .map(|k| x("send-key", &["-b", b, &key_name(*k)]))
                    .collect::<Vec<_>>()
                    .join(" && "),
            ),
        }
    }

    fn spawn(&self, target: &Target, program: &Program, cwd: &str, name: &str) -> Exec {
        let argv = program.argv();
        let (cmd, mut args) = match &target.session {
            Some(_) => (
                "run",
                vec!["--json", "--label", name, "--focus=false", "--cwd", cwd, "--"],
            ),
            None => ("new", vec!["--json", name, "--cwd", cwd, "--"]),
        };
        args.extend(argv.iter().map(String::as_str));
        script(rex(&target.server, &[cmd], target.session.as_deref(), &args))
    }

    fn parse_spawned(&self, target: &Target, name: &str, stdout: &str) -> Result<PaneRef, MuxError> {
        let v: Value = serde_json::from_str(stdout.trim())
            .map_err(|e| MuxError::Parse("rex run --json", e.to_string()))?;
        // `rex run` answers {window_id, block_ids}; `rex new` {session_id, initial_windows: [{block_ids}]}.
        let block = v
            .pointer("/block_ids/0")
            .or_else(|| v.pointer("/initial_windows/0/block_ids/0"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| MuxError::Parse("rex run --json", stdout.to_owned()))?;
        let session = target.session.clone().or_else(|| {
            v.get("session_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .or_else(|| Some(name.to_owned()))
        });
        Ok(PaneRef {
            multiplexer: Multiplexer::Rex,
            server: target.server.clone(),
            session,
            pane: block,
        })
    }

    fn kill(&self, pane: &PaneRef) -> Result<Exec, MuxError> {
        let session = pane.session.as_deref().ok_or(MuxError::Unsupported(
            Multiplexer::Rex,
            "close a block without its session",
        ))?;
        Ok(script(rex(
            &pane.server,
            &["block", "close"],
            Some(session),
            &[&pane.pane],
        )))
    }

    /// Streams the session's events; filter by block with [`parse_event`].
    fn watch(&self, pane: &PaneRef) -> Watch {
        Watch {
            exec: script(rex(
                &pane.server,
                &["events"],
                pane.session.as_deref(),
                &match &pane.session {
                    Some(_) => vec!["--json"],
                    None => vec!["--json", "--all"],
                },
            )),
            stream: StreamKind::RexEvents,
            cleanup: None,
        }
    }
}

/// A terminal event from `rex events --json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RexEvent {
    /// A program-status (OSC 7501) record was set or cleared.
    ProgramStatus {
        block: String,
        update: StatusUpdate,
    },
    /// Records were dropped, e.g. because the program exited.
    ProgramStatusRemoved {
        block: String,
        ids: Vec<String>,
    },
    Title {
        block: String,
        title: String,
    },
    Bell {
        block: String,
    },
    /// The foreground process changed; `None` when Rex reports none.
    Process {
        block: String,
        foreground: Option<String>,
    },
    ChildExited {
        block: String,
        code: Option<i64>,
    },
    BlockClosed {
        block: String,
    },
    /// Any other event.
    Other {
        name: String,
    },
}

impl RexEvent {
    /// The block the event is about, if any.
    pub fn block(&self) -> Option<&str> {
        match self {
            RexEvent::ProgramStatus { block, .. }
            | RexEvent::ProgramStatusRemoved { block, .. }
            | RexEvent::Title { block, .. }
            | RexEvent::Bell { block }
            | RexEvent::Process { block, .. }
            | RexEvent::ChildExited { block, .. }
            | RexEvent::BlockClosed { block } => Some(block),
            RexEvent::Other { .. } => None,
        }
    }
}

/// Parses one line of `rex events --json`. `now_ms` stamps state updates.
pub fn parse_event(line: &str, now_ms: u64) -> Option<RexEvent> {
    let v: Value = serde_json::from_str(line).ok()?;
    let ty = v.get("type")?.as_str()?;
    if ty != "session_event" {
        return Some(RexEvent::Other { name: ty.to_owned() });
    }
    let ev = v.pointer("/payload/event")?;
    let ety = ev.get("type")?.as_str()?;
    let p = ev.get("payload")?;
    if ety == "block_closed" {
        return Some(RexEvent::BlockClosed {
            block: p.get("block_id")?.as_str()?.to_owned(),
        });
    }
    if ety != "block_event" {
        return Some(RexEvent::Other { name: ety.to_owned() });
    }
    let block = p.get("block_id")?.as_str()?.to_owned();
    let name = p.get("name")?.as_str()?;
    let data = p.get("payload").unwrap_or(&Value::Null);
    let s = |k: &str| data.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(match name {
        "program_status_changed" => RexEvent::ProgramStatus {
            update: record_update(s("id"), data.get("record").unwrap_or(&Value::Null), now_ms)?,
            block,
        },
        "program_status_removed" => RexEvent::ProgramStatusRemoved {
            ids: data
                .get("ids")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_str).map(str::to_owned).collect())
                .unwrap_or_default(),
            block,
        },
        "title_changed" => RexEvent::Title {
            title: s("title").unwrap_or_default(),
            block,
        },
        "bell" => RexEvent::Bell { block },
        "process_changed" => RexEvent::Process {
            foreground: data.get("foreground").and_then(process_label),
            block,
        },
        "child_exited" => RexEvent::ChildExited {
            code: data.get("exit_code").and_then(Value::as_i64),
            block,
        },
        other => RexEvent::Other {
            name: other.to_owned(),
        },
    })
}

/// Maps a program-status record (`state`, `kind`, `progress`, `app`,
/// `effective_app`, `title`, `msg`). Rex decodes the OSC 7501 base64, so
/// `title` and `msg` arrive as plain text. A null record is a clear.
fn record_update(id: Option<String>, record: &Value, now_ms: u64) -> Option<StatusUpdate> {
    let id = id.filter(|i| !i.is_empty());
    let get = |k: &str| record.get(k).and_then(Value::as_str);
    let state = get("state").unwrap_or("clear");
    let mut payload = format!("state={state}");
    if let Some(kind) = get("kind") {
        payload.push_str(&format!(":kind={kind}"));
    }
    if let Some(p) = record.get("progress").and_then(Value::as_u64) {
        payload.push_str(&format!(":progress={p}"));
    }
    let Some(Osc7501::Report(mut report)) = osc7501::parse_payload(&payload) else {
        return None;
    };
    report.id = id;
    report.app = get("app").or_else(|| get("effective_app")).map(str::to_owned);
    report.title = get("title").map(str::to_owned);
    report.message = get("msg").map(str::to_owned);
    Some(status_update(report, now_ms))
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;
    use acc_core::state::{BlockKind, RunState, StateSource};

    // Lines and results below are from rex 0.0.1792519967 (API revision 4).
    fn wrap(name: &str, payload: &str) -> String {
        format!(
            r#"{{"type":"session_event","payload":{{"session_id":"session:01a1","event":{{"type":"block_event","payload":{{"block_id":"block:01b2","creator_name":"com.superlogical.terminal","name":"{name}","payload":{payload}}}}}}}}}"#
        )
    }

    #[test]
    fn parses_child_exited_and_other_events() {
        let line = wrap("child_exited", r#"{"exit_code":2,"error":"exit status 2"}"#);
        assert_eq!(
            parse_event(&line, 0),
            Some(RexEvent::ChildExited {
                block: "block:01b2".into(),
                code: Some(2)
            })
        );
        let closed = r#"{"type":"session_event","payload":{"session_id":"session:01a1","event":{"type":"block_closed","payload":{"block_id":"block:01b2"}}}}"#;
        assert_eq!(
            parse_event(closed, 0),
            Some(RexEvent::BlockClosed {
                block: "block:01b2".into()
            })
        );
        let connected = r#"{"type":"session_event","payload":{"session_id":"session:01a1","event":{"type":"client_connected","payload":{"client_id":"client:1"}}}}"#;
        assert_eq!(
            parse_event(connected, 0),
            Some(RexEvent::Other {
                name: "client_connected".into()
            })
        );
    }

    #[test]
    fn process_changed_uses_argv0_over_the_file_name() {
        let line = wrap(
            "process_changed",
            r#"{"generation":3,"foreground":{"pid":44588,"user_id":501,"name":"2.1.308","path":"/Users/v/.local/share/claude/versions/2.1.308","argv0":"claude","cwd":"/src"}}"#,
        );
        assert_eq!(
            parse_event(&line, 0),
            Some(RexEvent::Process {
                block: "block:01b2".into(),
                foreground: Some("claude".into())
            })
        );
        let none = wrap("process_changed", r#"{"generation":1,"foreground":null}"#);
        assert_eq!(
            parse_event(&none, 0),
            Some(RexEvent::Process {
                block: "block:01b2".into(),
                foreground: None
            })
        );
        let login: Value = serde_json::from_str(r#"{"name":"zsh","argv0":"-zsh"}"#).unwrap();
        assert_eq!(process_label(&login).as_deref(), Some("zsh"));
        let node: Value = serde_json::from_str(
            r#"{"name":"node","argv0":"node","script":"/usr/lib/node_modules/pi/cli.js"}"#,
        )
        .unwrap();
        assert_eq!(process_label(&node).as_deref(), Some("cli.js"));
    }

    #[test]
    fn program_status_record_maps_to_state() {
        let line = wrap(
            "program_status_changed",
            r#"{"id":"","record":{"state":"blocked","kind":"question","app":"probe","effective_app":"probe","msg":"Which file?","updated_at":"2026-10-22T09:51:14.208023+02:00","owner":{"pid":2415,"name":"bash"}},"reason":"report"}"#,
        );
        let Some(RexEvent::ProgramStatus { block, update }) = parse_event(&line, 7) else {
            panic!("expected program status")
        };
        assert_eq!(block, "block:01b2");
        assert_eq!(update.id, None);
        let s = update.state.unwrap();
        assert_eq!(
            s.state,
            RunState::Blocked {
                kind: BlockKind::Question
            }
        );
        assert_eq!(s.message.as_deref(), Some("Which file?"));
        assert_eq!(s.source, StateSource::Osc7501);

        // Plain text that happens to be valid base64 stays as it is.
        let plain = wrap(
            "program_status_changed",
            r#"{"id":"","record":{"state":"working","effective_app":"claude-code","msg":"test"},"reason":"report"}"#,
        );
        let Some(RexEvent::ProgramStatus { update, .. }) = parse_event(&plain, 0) else {
            panic!("expected program status")
        };
        assert_eq!(update.state.unwrap().message.as_deref(), Some("test"));

        let cleared = wrap(
            "program_status_changed",
            r#"{"id":"agents/1","record":null,"reason":"clear"}"#,
        );
        let Some(RexEvent::ProgramStatus { update, .. }) = parse_event(&cleared, 0) else {
            panic!("expected program status")
        };
        assert_eq!(update.id.as_deref(), Some("agents/1"));
        assert!(update.state.is_none());
    }

    #[test]
    fn scope_flags_follow_the_subcommand() {
        let t = Target {
            multiplexer: Multiplexer::Rex,
            server: String::new(),
            session: Some("work".into()),
        };
        let prog = Program {
            program: "pi".into(),
            args: vec![],
            env: vec![],
        };
        let spawn = Rex.spawn(&t, &prog, "/src", "acc-1").script;
        assert!(spawn.starts_with(PATH_LINE));
        assert!(spawn.ends_with("rex run -s work --json --label acc-1 --focus=false --cwd /src -- pi"));
        let p = Rex
            .parse_spawned(
                &t,
                "acc-1",
                r#"{"window_id":"window:1","block_ids":["block:9"],"revision":2}"#,
            )
            .unwrap();
        assert_eq!(p.pane, "block:9");
        let keys = Rex
            .send(&p, &Input::Keys(vec![Key::Escape, Key::Ctrl('c')]))
            .script;
        assert!(
            keys.ends_with(
                "rex send-key -s work -b block:9 escape && rex send-key -s work -b block:9 ctrl+c"
            )
        );
        assert_eq!(Rex.send(&p, &Input::Text("hi".into())).stdin, b"hi");
        assert!(
            Rex.capture(&p, true)
                .script
                .ends_with("rex capture -s work -b block:9 --trim --unwrap")
        );
        assert!(
            Rex.kill(&p)
                .unwrap()
                .script
                .ends_with("rex block close -s work block:9")
        );
        assert!(Rex.watch(&p).exec.script.ends_with("rex events -s work --json"));

        let remote = PaneRef {
            server: "studio".into(),
            ..p
        };
        assert!(
            Rex.capture(&remote, false)
                .script
                .ends_with("rex -S studio capture -s work -b block:9 --trim --unwrap")
        );
    }

    #[test]
    fn new_session_reports_its_first_block() {
        let t = Target {
            multiplexer: Multiplexer::Rex,
            server: String::new(),
            session: None,
        };
        let out = r#"{
  "session_id": "session:01a16297",
  "initial_windows": [
    {
      "window_id": "window:01a16297",
      "block_ids": ["block:01a16297-b9c9"],
      "revision": 1
    }
  ]
}"#;
        let p = Rex.parse_spawned(&t, "acc-1", out).unwrap();
        assert_eq!(p.pane, "block:01a16297-b9c9");
        assert_eq!(p.session.as_deref(), Some("session:01a16297"));
    }

    #[test]
    fn panes_from_view_process_and_title() {
        let t = Target {
            multiplexer: Multiplexer::Rex,
            server: "studio".into(),
            session: None,
        };
        let out = r#"#acc-session session:01a1
{"revision":14,"session_id":"session:01a1","label":"work","active_window_id":"window:1","windows":[{"window_id":"window:1","label":"Window 1","focused_block_id":"block:1","layers":[{"layer_id":"layer:1","kind":"tiled","bounds":[0,0,1,1],"layout":{"rect":[0,0,1,1],"block_id":"block:1"},"blocks":[{"block_id":"block:1","label":"shell","creator_name":"com.superlogical.terminal","flavor":"com.superlogical.terminal.shell","owner":"client:1"}]}]}]}
#acc-process block:1
{"last_exit":null,"child":{"pid":13953,"user_id":0,"user":"root","name":"login","path":"/usr/bin/login"},"foreground":{"pid":44588,"user_id":501,"name":"2.1.308","argv0":"claude","cwd":"/src"},"home":"/Users/v"}
#acc-title block:1
{"title":"Rex support","owner":{"pid":44588,"name":"2.1.308"}}
"#;
        let panes = Rex.parse_panes(&t, out).unwrap();
        assert_eq!(panes.len(), 1);
        let p = &panes[0];
        assert_eq!(p.pane.pane, "block:1");
        assert_eq!(p.pane.session.as_deref(), Some("session:01a1"));
        assert_eq!(p.pane.server, "studio");
        assert_eq!(p.command.as_deref(), Some("claude"));
        assert_eq!(p.pid, Some(13953));
        assert_eq!(p.cwd.as_deref(), Some("/src"));
        assert_eq!(p.title.as_deref(), Some("Rex support"));
        assert!(p.active);
        assert!(!p.exited);
    }
}
