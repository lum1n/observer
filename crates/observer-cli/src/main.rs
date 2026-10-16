//! `observer`: harness TUIs acc didn't start, from the command line. Scripts
//! it prints can be piped to `ssh host sh`, which is exactly what a client
//! does over an SSH exec channel.

mod doctor;

use std::io::{Read as _, Write as _};
use std::process::ExitCode;

use acc_core::{Exec, Harness};
use observer::multiplexer::{self, Input, Key, Multiplexer, PaneRef, Target, screen_match, tui};
use observer::osc7501::Scanner;
use observer::{discover, transcript};

const USAGE: &str = "\
usage:
  observer doctor                       multiplexers, this shell's pane and harness session stores
  observer sessions <claude|codex|copilot|cursor|pi> [N]
                                        recent sessions on this machine, with acc resume commands
  observer discover                     print a script listing harness TUIs on a host
                                        (pipe to `ssh host sh`, then to `observer decode discover`)
  observer mux <tmux|zellij|screen|rex> <server|-> <session|-> list
  observer mux <mux> <server> <session|-> capture|watch <pane>
  observer mux <mux> <server> <session|-> type <pane> <text>
  observer mux <mux> <server> <session|-> keys <pane> <key>...
                                        multiplexer commands (`type` submits text with Enter;
                                        keys: enter, escape, tab, up, down, left, right,
                                        backspace, c-<letter>, or one character such as 1)
  observer osc7501                      read terminal bytes on stdin, print OSC 7501 messages as JSON
  observer decode pi-transcript|claude-transcript|codex-rollout|copilot-events|cursor-transcript
                                        read a session file on stdin, print events
  observer decode screen <claude|pi>    read a captured pane on stdin, print its state and any
                                        open dialog as JSON (`null` when not recognized)
  observer decode discover              read `observer discover` output on stdin, print the TUIs
";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    match args.as_slice() {
        ["doctor"] => doctor::doctor(),
        ["sessions", harness, rest @ ..] => match (parse_harness(harness), rest) {
            (Ok(h), []) => doctor::sessions(h, 20),
            (Ok(h), [n]) if n.parse::<usize>().is_ok() => doctor::sessions(h, n.parse().unwrap_or(20)),
            (Err(e), _) => or_usage(Err(e)),
            _ => or_usage(Err(USAGE.to_owned())),
        },
        ["discover"] => {
            print!("{}", discover::discover_script());
            ExitCode::SUCCESS
        }
        ["mux", rest @ ..] => mux(rest),
        ["osc7501"] => osc(),
        ["decode", "screen", harness] => or_usage(parse_harness(harness).and_then(decode_screen)),
        ["decode", "discover"] => or_usage(decode_discover()),
        ["decode", kind] => decode(kind),
        _ => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

fn or_usage(r: Result<ExitCode, String>) -> ExitCode {
    r.unwrap_or_else(|e| {
        eprintln!("observer: {e}");
        ExitCode::from(2)
    })
}

fn parse_harness(s: &str) -> Result<Harness, String> {
    Harness::ALL
        .into_iter()
        .find(|h| h.id() == s || h.program() == s || format!("{h:?}").eq_ignore_ascii_case(s))
        .ok_or_else(|| format!("unknown harness {s:?}"))
}

fn osc() -> ExitCode {
    let mut scanner = Scanner::new();
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 8192];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(n) => n,
            Err(e) => {
                eprintln!("observer: {e}");
                return ExitCode::FAILURE;
            }
        };
        for msg in scanner.feed(&buf[..n]) {
            let line = serde_json::to_string(&msg).unwrap_or_default();
            if writeln!(stdout, "{line}").is_err() {
                return ExitCode::FAILURE;
            }
        }
    }
}

fn mux(args: &[&str]) -> ExitCode {
    let [m, server, session, rest @ ..] = args else {
        eprint!("{USAGE}");
        return ExitCode::from(2);
    };
    let m = match *m {
        "tmux" => Multiplexer::Tmux,
        "zellij" => Multiplexer::Zellij,
        "screen" => Multiplexer::Screen,
        "rex" => Multiplexer::Rex,
        other => {
            eprintln!("observer: unknown multiplexer {other:?}");
            return ExitCode::from(2);
        }
    };
    let b = multiplexer::backend(m);
    let target = Target {
        multiplexer: m,
        server: Some((*server).to_owned())
            .filter(|s| s != "-")
            .unwrap_or_default(),
        session: Some((*session).to_owned()).filter(|s| s != "-"),
    };
    let pane = |p: &str| PaneRef {
        multiplexer: m,
        server: target.server.clone(),
        session: target.session.clone(),
        pane: p.to_owned(),
    };
    // Inputs carried on stdin are shown as a heredoc so the output stays one script.
    let print = |e: Exec| {
        if e.stdin.is_empty() {
            println!("{}", e.script);
        } else {
            println!("{} <<'ACC_EOF'\n{}\nACC_EOF", e.script, heredoc_body(&e.stdin));
        }
    };
    match rest {
        ["list"] => print(b.list_panes(&target)),
        ["capture", p] => print(b.capture(&pane(p), true)),
        ["watch", p] => {
            let e = b.watch(&pane(p)).exec;
            if e.stdin.is_empty() {
                print(e);
            } else {
                // Watches that read stdin (tmux control mode) end when it
                // closes, so feed the setup through a FIFO this script holds
                // open until the watch exits.
                println!(
                    "d=$(mktemp -d) && mkfifo \"$d/in\" || exit 1\n{} <\"$d/in\" &\nexec 3>\"$d/in\"\nrm -rf \"$d\"\ncat >&3 <<'ACC_EOF'\n{}\nACC_EOF\nwait $!",
                    e.script,
                    heredoc_body(&e.stdin)
                );
            }
        }
        ["type", p, text] => {
            for input in tui::submit(text) {
                print(b.send(&pane(p), &input));
            }
        }
        ["keys", p, keys @ ..] if !keys.is_empty() => {
            let keys: Result<Vec<Key>, String> = keys.iter().map(|k| parse_key(k)).collect();
            match keys {
                Ok(keys) => print(b.send(&pane(p), &Input::Keys(keys))),
                Err(e) => return or_usage(Err(e)),
            }
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    }
    ExitCode::SUCCESS
}

fn parse_key(name: &str) -> Result<Key, String> {
    let mut chars = name.chars();
    Ok(
        match (name.to_ascii_lowercase().as_str(), chars.next(), chars.next()) {
            ("enter", ..) => Key::Enter,
            ("escape" | "esc", ..) => Key::Escape,
            ("tab", ..) => Key::Tab,
            ("backspace", ..) => Key::Backspace,
            ("up", ..) => Key::Up,
            ("down", ..) => Key::Down,
            ("left", ..) => Key::Left,
            ("right", ..) => Key::Right,
            (n, ..) if n.len() == 3 && n.starts_with("c-") && n.as_bytes()[2].is_ascii_lowercase() => {
                Key::Ctrl(char::from(n.as_bytes()[2]))
            }
            (_, Some(c), None) if !c.is_control() => Key::Char(c),
            _ => return Err(format!("unknown key {name:?}")),
        },
    )
}

fn decode_screen(harness: Harness) -> Result<ExitCode, String> {
    if !screen_match::supported(harness) {
        return Err(format!("can't read {}'s screen yet", harness.display_name()));
    }
    let mut screen = String::new();
    std::io::stdin()
        .read_to_string(&mut screen)
        .map_err(|e| e.to_string())?;
    let read = screen_match::read(harness, &screen);
    println!(
        "{}",
        serde_json::to_string_pretty(&read).map_err(|e| e.to_string())?
    );
    Ok(ExitCode::SUCCESS)
}

fn decode_discover() -> Result<ExitCode, String> {
    let mut stdout = String::new();
    std::io::stdin()
        .read_to_string(&mut stdout)
        .map_err(|e| e.to_string())?;
    let agents = discover::parse_discover(&stdout);
    println!(
        "{}",
        serde_json::to_string_pretty(&agents).map_err(|e| e.to_string())?
    );
    Ok(ExitCode::SUCCESS)
}

/// stdin bytes as a heredoc body. The heredoc adds the final newline itself;
/// a doubled one would be an empty line, which detaches a tmux control client.
fn heredoc_body(stdin: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(stdin.strip_suffix(b"\n").unwrap_or(stdin))
}

type Decoder = dyn FnMut(&[u8]) -> Vec<acc_core::Event>;

fn decode(kind: &str) -> ExitCode {
    let mut feed: Box<Decoder> = match kind {
        "pi-transcript" => {
            let mut t = transcript::pi::Transcript::new();
            Box::new(move |b| t.feed(b))
        }
        "claude-transcript" => {
            let mut t = transcript::claude::Observer::new();
            Box::new(move |b| t.feed(b))
        }
        "codex-rollout" => {
            let mut t = transcript::codex::Observer::new();
            Box::new(move |b| t.feed(b))
        }
        "copilot-events" => {
            let mut t = transcript::copilot::Observer::new();
            Box::new(move |b| t.feed(b))
        }
        // Read whole: Cursor's transcript is a snapshot, not a stream.
        "cursor-transcript" => {
            let mut text = String::new();
            if let Err(e) = std::io::stdin().read_to_string(&mut text) {
                eprintln!("observer: {e}");
                return ExitCode::FAILURE;
            }
            let mut stdout = std::io::stdout().lock();
            for ev in transcript::cursor::Transcript::new().feed_whole(&text) {
                let line = serde_json::to_string(&ev).unwrap_or_default();
                if writeln!(stdout, "{line}").is_err() {
                    return ExitCode::FAILURE;
                }
            }
            return ExitCode::SUCCESS;
        }
        _ => {
            eprint!("{USAGE}");
            return ExitCode::from(2);
        }
    };
    let mut stdin = std::io::stdin().lock();
    let mut stdout = std::io::stdout().lock();
    let mut buf = [0u8; 65536];
    loop {
        let n = match stdin.read(&mut buf) {
            Ok(0) => return ExitCode::SUCCESS,
            Ok(n) => n,
            Err(e) => {
                eprintln!("observer: {e}");
                return ExitCode::FAILURE;
            }
        };
        for ev in feed(&buf[..n]) {
            let line = serde_json::to_string(&ev).unwrap_or_default();
            if writeln!(stdout, "{line}").is_err() {
                return ExitCode::FAILURE;
            }
        }
    }
}
