//! Cursor, Copilot and opencode TUIs, live: started from a shell in a tmux
//! pane like a user would, found by the discovery script, followed through
//! [`Observed`] while a prompt runs, taken over, resumed headless with
//! `acc run`, and handed back to the pane.
//!
//! These run the real harnesses with the signed-in `$HOME` and spend a few
//! model calls each, so they are ignored. Run one with
//! `cargo test -p observer --test live_tuis -- --ignored cursor`.
//! `ACC_BIN` names the acc binary (default: `../acc/target/debug/acc`);
//! `OBSERVER_LIVE_COPILOT` is a command line to run as `copilot` (such as
//! `npx -y @github/copilot`) when it isn't installed.
#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::io::Write as _;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use acc_core::command::Command as AccCommand;
use acc_core::state::RunState;
use acc_core::{Event, Exec, Harness};
use observer::discover::{self, ForeignAgent};
use observer::multiplexer::{self, Input, Key, Multiplexer, PaneRef, Program, tui};
use observer::observe::Observed;
use observer::takeover::{self, Takeover, TakeoverAction, TakeoverInput};

const PROMPT: &str = "Run the shell command 'echo hi' and then reply with the single word done.";

struct Live {
    harness: Harness,
    dir: PathBuf,
    sock: String,
    bin: PathBuf,
    start: Instant,
    trusted: std::cell::Cell<bool>,
}

impl Live {
    fn new(harness: Harness) -> Self {
        let tag = harness.program();
        let dir = std::env::temp_dir().join(format!("observer-live-{tag}-{}", std::process::id()));
        let bin = dir.join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        let work = dir.join("work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::write(work.join("a.txt"), "hello\n").unwrap();
        if harness == Harness::Copilot
            && let Ok(cmd) = std::env::var("OBSERVER_LIVE_COPILOT")
        {
            let wrapper = bin.join("copilot");
            std::fs::write(&wrapper, format!("#!/bin/sh\nexec {cmd} \"$@\"\n")).unwrap();
            Command::new("chmod").arg("+x").arg(&wrapper).status().unwrap();
        }
        let sock = dir.join("tmux.sock").display().to_string();
        let live = Self {
            harness,
            dir,
            sock,
            bin,
            start: Instant::now(),
            trusted: std::cell::Cell::new(false),
        };
        live.sh(&format!(
            "tmux -S {} new-session -d -s live -x 160 -y 45 -c {} sh",
            live.sock,
            live.cwd()
        ));
        live
    }

    fn cwd(&self) -> String {
        self.dir.join("work").display().to_string()
    }

    fn now(&self) -> u64 {
        u64::try_from(self.start.elapsed().as_millis()).unwrap()
    }

    fn sh(&self, script: &str) -> String {
        self.run(&Exec::new(script.to_owned()))
    }

    fn run(&self, exec: &Exec) -> String {
        let path = format!(
            "{}:{}",
            self.bin.display(),
            std::env::var("PATH").unwrap_or_default()
        );
        let mut child = Command::new("sh")
            .arg("-c")
            .arg(&exec.script)
            .env("PATH", path)
            .env_remove("TMUX")
            .env_remove("TMUX_PANE")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(&exec.stdin).unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn pane(&self) -> PaneRef {
        let id = self.sh(&format!(
            "tmux -S {} display -p -t live '#{{pane_id}}'",
            self.sock
        ));
        PaneRef {
            multiplexer: Multiplexer::Tmux,
            server: self.sock.clone(),
            session: Some("live".into()),
            pane: id.trim().to_owned(),
        }
    }

    fn send(&self, pane: &PaneRef, inputs: &[Input]) {
        let b = multiplexer::backend(Multiplexer::Tmux);
        for i in inputs {
            self.run(&b.send(pane, i));
            std::thread::sleep(Duration::from_millis(300));
        }
    }

    fn screen(&self, pane: &PaneRef) -> String {
        self.run(&multiplexer::backend(Multiplexer::Tmux).capture(pane, false))
    }

    /// The TUI waits at its prompt.
    fn ready(&self, screen: &str) -> bool {
        let marker = match self.harness {
            Harness::Cursor => "Plan, search, build anything",
            Harness::Copilot => "/ commands",
            _ => "ctrl+p commands",
        };
        screen.contains(marker)
    }

    /// Answers the dialogs a fresh directory and the prompt bring up:
    /// folder trust (once: Cursor leaves the box drawn), and running `echo`.
    fn answer_dialogs(&self, pane: &PaneRef, screen: &str) -> bool {
        let bottom: Vec<&str> = screen.lines().rev().take(20).collect();
        let near = |s: &str| bottom.iter().any(|l| l.contains(s));
        let trust = screen.contains("Trust this workspace") || near("Do you trust the files in this folder");
        let key = if trust && !self.trusted.get() {
            self.trusted.set(true);
            if self.harness == Harness::Cursor {
                Key::Char('a')
            } else {
                Key::Enter
            }
        } else if self.harness == Harness::Copilot && near("Do you want to run") {
            Key::Enter
        } else if tui::shows_approval(self.harness, screen) {
            Key::Char('y')
        } else {
            return false;
        };
        self.send(pane, &[Input::Keys(vec![key])]);
        true
    }

    fn until(&self, what: &str, secs: u64, mut check: impl FnMut() -> bool) {
        let deadline = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < deadline {
            if check() {
                return;
            }
            std::thread::sleep(Duration::from_millis(1_000));
        }
        let pane = self.pane();
        panic!(
            "timed out waiting for {what}; the pane shows:\n{}",
            self.screen(&pane)
        );
    }

    fn discover(&self) -> Option<ForeignAgent> {
        discover::parse_discover(&self.sh(&discover::discover_script()))
            .into_iter()
            .find(|a| a.harness == self.harness && a.cwd.as_deref() == Some(self.cwd().as_str()))
    }
}

impl Drop for Live {
    fn drop(&mut self) {
        let _ = self.sh(&format!("tmux -S {} kill-server", self.sock));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// One probe (and a file read, for tailed transcripts) into `o`.
fn step(live: &Live, o: &mut Observed, events: &mut Vec<Event>) {
    let now = live.now();
    let screen = o.screen_exec(now).map(|e| live.run(&e));
    let probe = live.run(&o.probe_exec());
    events.extend(
        o.tick(now, &probe, screen.as_deref())
            .into_iter()
            .map(|u| u.event),
    );
    if let Some(entry) = o.feed_entry()
        && let acc_core::detached::FeedPath::File(path) = &entry.path
    {
        let bytes = std::fs::read(path).unwrap_or_default();
        let from = usize::try_from(entry.offset).unwrap().min(bytes.len());
        events.extend(o.feed(&bytes[from..], now).into_iter().map(|u| u.event));
    }
}

fn live_tui(harness: Harness) {
    let live = Live::new(harness);
    let pane = live.pane();
    // Start it from the pane's shell, as a user would.
    let program: Program = tui::launch(harness, None).unwrap();
    let line = acc_core::shell::join(program.argv().iter().map(String::as_str));
    live.send(&pane, &tui::submit(&line));
    live.until("the TUI to start", 90, || {
        let screen = live.screen(&pane);
        live.answer_dialogs(&pane, &screen);
        live.ready(&screen) && live.discover().is_some()
    });
    std::thread::sleep(Duration::from_secs(3));
    live.send(&pane, &tui::inputs(harness, &AccCommand::prompt(PROMPT)).unwrap());

    // Found, and followed until the turn ends.
    let mut agent = None;
    live.until("its session to show up", 60, || {
        agent = live
            .discover()
            .filter(|a| harness == Harness::Opencode || a.transcript.is_some());
        agent.is_some()
    });
    let agent = agent.unwrap();
    eprintln!("discovered: {agent:?}");
    assert_eq!(
        agent.pane.as_ref().map(|p| p.pane.as_str()),
        Some(pane.pane.as_str())
    );
    let mut o = Observed::new(agent.clone());
    assert!(o.is_readable());
    let mut events = Vec::new();
    let mut blocked = false;
    live.until("the turn to end", 180, || {
        step(&live, &mut o, &mut events);
        let state = o.state().map(|s| s.state.clone());
        if matches!(state, Some(RunState::Blocked { .. })) {
            blocked = true;
            live.answer_dialogs(&pane, &live.screen(&pane));
        } else {
            live.answer_dialogs(&pane, &live.screen(&pane));
        }
        events.iter().any(|e| {
            matches!(
                e,
                Event::ItemStarted {
                    kind: acc_core::event::ItemKind::UserMessage,
                    ..
                }
            )
        }) && matches!(state, Some(RunState::Done))
    });
    let states: Vec<String> = events
        .iter()
        .filter_map(|e| match e {
            Event::StateChanged(s) => Some(format!("{:?}", s.state)),
            _ => None,
        })
        .collect();
    eprintln!("states: {states:?} (blocked seen: {blocked})");
    let commands: Vec<String> = o
        .transcript()
        .items()
        .iter()
        .map(|i| format!("{:?} {:?}", i.kind, i.detail))
        .collect();
    eprintln!("items:\n  {}", commands.join("\n  "));
    assert!(
        o.transcript()
            .items()
            .iter()
            .any(|i| format!("{:?}", i.detail).contains("echo hi")),
        "the shell call is an item"
    );
    let native = o.native_id().expect("the session id").to_owned();

    // Take it over: close the TUI, then resume headless.
    let mut t = Takeover::new(o.agent().clone(), Some(native.clone()), false).unwrap();
    let mut input = TakeoverInput::Tick {
        now_ms: live.now(),
        safe: o.at_safe_point(),
    };
    let request = loop {
        match t.next(input) {
            TakeoverAction::Run { exec, step } => {
                eprintln!("takeover: {step:?}");
                let stdout = live.run(&exec);
                input = TakeoverInput::Ran {
                    now_ms: live.now(),
                    stdout,
                };
            }
            TakeoverAction::Wait { ms, .. } => {
                std::thread::sleep(Duration::from_millis(ms));
                step(&live, &mut o, &mut events);
                input = TakeoverInput::Tick {
                    now_ms: live.now(),
                    safe: o.at_safe_point(),
                };
            }
            TakeoverAction::Launch { request } => break request,
            TakeoverAction::Failed { reason } => panic!("takeover failed: {reason}"),
        }
    };
    assert_eq!(request.resume.as_deref(), Some(native.as_str()));
    let acc = std::env::var("ACC_BIN")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../../../acc/target/debug/acc").to_owned());
    let out = live.sh(&format!(
        "cd {} && echo 'Reply with the single word yes.' | {} run {} . --resume {} --json 2>&1",
        acc_core::shell::quote(&live.cwd()),
        acc_core::shell::quote(&acc),
        harness.program(),
        acc_core::shell::quote(&native)
    ));
    let tail: String = out.lines().rev().take(15).collect::<Vec<_>>().join("\n");
    eprintln!("acc run (last lines, newest first):\n{tail}");
    assert!(out.contains("turn_completed"), "the resumed session runs a turn");

    // And back to the pane.
    let spec_launcher = request.launcher.clone();
    let pane_label = takeover::pane_label(&pane);
    let back =
        takeover::hand_back_exec(harness, &native, &pane_label, &live.cwd(), spec_launcher.as_ref()).unwrap();
    live.run(&back);
    live.until("the TUI to come back with acc's turn", 90, || {
        let screen = live.screen(&pane);
        live.answer_dialogs(&pane, &screen);
        screen
            .lines()
            .any(|l| l.trim().trim_start_matches(['●', '•', '┃']).trim() == "yes")
            || screen.contains("Reply with the single word yes.")
    });
    live.send(&pane, &tui::exit_inputs(harness).unwrap());
}

#[test]
#[ignore = "needs cursor-agent signed in; spends model calls"]
fn cursor() {
    live_tui(Harness::Cursor);
}

#[test]
#[ignore = "needs copilot signed in; spends model calls"]
fn copilot() {
    live_tui(Harness::Copilot);
}

#[test]
#[ignore = "needs opencode 2.x signed in; spends model calls"]
fn opencode() {
    live_tui(Harness::Opencode);
}
