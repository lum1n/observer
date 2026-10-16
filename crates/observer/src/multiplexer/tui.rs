//! Co-driving a harness TUI in a pane: ACC commands as keystrokes.
//!
//! When a harness runs interactively in a multiplexer, ACC types into its
//! pane like the user would. This is per harness, and only covers what each
//! TUI does reliably from its prompt: submitting text, interrupting, built-in
//! slash commands and exiting. Answering approval dialogs needs the rendered
//! screen: see [`super::screen_match`].
//!
//! pi is covered fully (checked against pi 1.1.0's keybindings and slash
//! commands). Other harnesses get prompts and the commands whose keys are
//! well known; the rest return [`DriverError::Unsupported`].

use acc_core::command::{Command, OptionId};
use acc_core::driver::DriverError;
use acc_core::harness::Harness;

use super::{Input, Key, Program};

/// Inputs that submit `text` at the TUI's prompt: typed when it is one line,
/// a bracketed paste otherwise, then Enter.
pub fn submit(text: &str) -> Vec<Input> {
    let body = if text.contains(['\n', '\r']) {
        Input::Paste(text.to_owned())
    } else {
        Input::Text(text.to_owned())
    };
    vec![body, Input::Keys(vec![Key::Enter])]
}

fn unsupported(harness: Harness, what: &'static str) -> DriverError {
    DriverError::Unsupported { harness, what }
}

/// Keystrokes that carry out `cmd` in `harness`'s TUI.
pub fn inputs(harness: Harness, cmd: &Command) -> Result<Vec<Input>, DriverError> {
    let slash = |s: String| Ok(submit(&s));
    let with_arg = |name: &str, arg: &str| {
        if arg.is_empty() {
            format!("/{name}")
        } else {
            format!("/{name} {arg}")
        }
    };
    match (harness, cmd) {
        (_, Command::Prompt { text, attachments }) if attachments.is_empty() => Ok(submit(text)),
        // Images only travel over the headless protocols.
        (h, Command::Prompt { .. }) => Err(unsupported(h, "images from a pane")),
        (_, Command::Invoke { name, args }) => slash(with_arg(name.trim_start_matches('/'), args)),
        (Harness::Pi | Harness::Claude | Harness::Codex, Command::Interrupt) => {
            Ok(vec![Input::Keys(vec![Key::Escape])])
        }
        // Escape only edits Copilot's queued prompt; Ctrl+C cancels the turn.
        (Harness::Copilot, Command::Interrupt) => Ok(vec![Input::Keys(vec![Key::Ctrl('c')])]),
        // Cursor puts the cancelled prompt back in the input.
        (Harness::Cursor, Command::Interrupt) => Ok(vec![Input::Keys(vec![Key::Escape])]),
        // opencode asks for a second Escape ("esc again to interrupt").
        (Harness::Opencode, Command::Interrupt) => Ok(vec![
            Input::Keys(vec![Key::Escape]),
            Input::Keys(vec![Key::Escape]),
        ]),
        (Harness::Opencode, Command::Compact { .. }) => slash("/compact".into()),
        (Harness::Pi | Harness::Claude | Harness::Codex | Harness::Copilot, Command::Compact { hint }) => {
            slash(with_arg("compact", hint.as_deref().unwrap_or("")))
        }
        (Harness::Pi | Harness::Codex | Harness::Opencode, Command::New) => slash("/new".into()),
        (Harness::Claude | Harness::Copilot, Command::New) => slash("/clear".into()),
        (Harness::Copilot, Command::Rename { name }) => slash(with_arg("rename", name)),
        (Harness::Pi, Command::Fork) => slash("/clone".into()),
        (Harness::Pi, Command::Rename { name }) => slash(with_arg("name", name)),
        (Harness::Pi | Harness::Claude, Command::Shell { command }) => slash(format!("!{command}")),
        (
            Harness::Pi,
            Command::SetOption {
                id: OptionId::Model,
                value,
            },
        ) => slash(with_arg("model", value)),
        (
            Harness::Pi,
            Command::SetOption {
                id: OptionId::Effort,
                value,
            },
        ) => slash(with_arg("thinking", value)),
        (
            Harness::Claude | Harness::Copilot,
            Command::SetOption {
                id: OptionId::Model,
                value,
            },
        ) => slash(with_arg("model", value)),
        // Picking an option depends on the dialog on screen right now.
        (_, Command::AnswerPermission { .. }) => Err(unsupported(
            harness,
            "answering dialogs without the screen (use screen_match::answer)",
        )),
        (h, _) => Err(unsupported(h, "this command from a pane")),
    }
}

/// Keystrokes that close the TUI cleanly (for takeover: close it, then
/// resume the session headless). Interrupt a running turn first.
pub fn exit_inputs(harness: Harness) -> Result<Vec<Input>, DriverError> {
    match harness {
        Harness::Pi | Harness::Codex => Ok(submit("/quit")),
        Harness::Claude | Harness::Copilot | Harness::Opencode => Ok(submit("/exit")),
        // Clear what an interrupt put back in the input first.
        Harness::Cursor => {
            let mut keys = vec![Input::Keys(vec![Key::Ctrl('u')])];
            keys.extend(submit("/quit"));
            Ok(keys)
        }
    }
}

/// The interactive TUI for `harness`, resuming `resume` when given. pi gets
/// `PI_PROGRAM_STATUS=1`, so it reports OSC 7501 through tmux and screen,
/// which never answer the probe.
pub fn launch(harness: Harness, resume: Option<&str>) -> Result<Program, DriverError> {
    let mut program = Program {
        program: harness.program().to_owned(),
        args: Vec::new(),
        env: Vec::new(),
    };
    match (harness, resume) {
        (Harness::Pi, r) => {
            program.env.push(("PI_PROGRAM_STATUS".into(), "1".into()));
            if let Some(path) = r {
                program.args = vec!["--session".into(), path.into()];
            }
        }
        (Harness::Claude | Harness::Copilot | Harness::Cursor, Some(id)) => {
            program.args = vec!["--resume".into(), id.into()];
        }
        (Harness::Codex, Some(id)) => program.args = vec!["resume".into(), id.into()],
        (Harness::Opencode, Some(id)) => program.args = vec!["--session".into(), id.into()],
        (_, None) => {}
    }
    Ok(program)
}

/// Keystrokes that hand a session back to its TUI in the user's own pane,
/// after ACC closed the TUI there: types the resume command into the shell
/// left in the pane.
pub fn resume_inputs(harness: Harness, resume: &str) -> Result<Vec<Input>, DriverError> {
    let argv = resume_argv(harness, resume, None)?;
    Ok(submit(&acc_core::shell::join(argv.iter().map(String::as_str))))
}

/// The TUI command resuming `resume`, run the way `launcher` describes
/// (inside the user's sandbox, with their executable and flags).
pub fn resume_argv(
    harness: Harness,
    resume: &str,
    launcher: Option<&acc_core::harness::Launcher>,
) -> Result<Vec<String>, DriverError> {
    let mut program = launch(harness, Some(resume))?;
    let Some(l) = launcher else {
        return Ok(program.argv());
    };
    if let Some(p) = &l.program {
        program.program = p.clone();
    }
    program.args.extend(l.args.iter().cloned());
    let mut argv = l.prefix.clone();
    argv.extend(program.argv());
    Ok(argv)
}

/// Whether a captured pane shows `harness`'s approval dialog, which its
/// transcript doesn't record. Only the bottom of the screen counts: the
/// dialog replaces the prompt there, while the same words higher up are
/// conversation.
pub fn shows_approval(harness: Harness, screen: &str) -> bool {
    match harness {
        // "Do you want to proceed?" (or "make this edit to", "allow Claude
        // to fetch", ...) over numbered choices starting with "1. Yes".
        Harness::Claude => {
            let tail: Vec<&str> = screen
                .lines()
                .map(|l| l.trim_matches(|c: char| c.is_whitespace() || "│|❯>›".contains(c)))
                .filter(|l| !l.is_empty())
                .collect();
            let tail = &tail[tail.len().saturating_sub(25)..];
            let question = tail.iter().position(|l| l.starts_with("Do you want to"));
            question.is_some_and(|q| tail[q..].iter().any(|l| l.starts_with("1. Yes")))
        }
        // "Run this command?" over "→ Run (once) (y)" ... "Skip & tell the
        // agent what to do instead (esc or n)".
        Harness::Cursor => {
            let tail: Vec<&str> = screen.lines().map(str::trim).filter(|l| !l.is_empty()).collect();
            let tail = &tail[tail.len().saturating_sub(15)..];
            tail.iter().any(|l| l.ends_with("(y)")) && tail.iter().any(|l| l.ends_with("(esc or n)"))
        }
        _ => false,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn prompts_type_or_paste() {
        assert_eq!(
            inputs(Harness::Cursor, &Command::prompt("hi")).unwrap(),
            [Input::Text("hi".into()), Input::Keys(vec![Key::Enter])]
        );
        assert!(matches!(
            inputs(Harness::Pi, &Command::prompt("a\nb")).unwrap()[0],
            Input::Paste(_)
        ));
        // A pane takes keystrokes, never an image.
        let with_image = Command::Prompt {
            text: "look".into(),
            attachments: vec![acc_core::command::Attachment::from_bytes("image/png", b"PNG")],
        };
        assert!(matches!(
            inputs(Harness::Pi, &with_image),
            Err(DriverError::Unsupported { .. })
        ));
    }

    #[test]
    fn pi_commands_are_slash_commands() {
        let first = |c: Command| inputs(Harness::Pi, &c).unwrap().remove(0);
        assert_eq!(
            first(Command::Compact { hint: None }),
            Input::Text("/compact".into())
        );
        assert_eq!(
            first(Command::Rename { name: "x y".into() }),
            Input::Text("/name x y".into())
        );
        assert_eq!(first(Command::Fork), Input::Text("/clone".into()));
        assert_eq!(
            first(Command::Shell { command: "ls".into() }),
            Input::Text("!ls".into())
        );
        assert_eq!(first(Command::Interrupt), Input::Keys(vec![Key::Escape]));
        assert!(inputs(Harness::Pi, &Command::Review).is_err());
    }

    #[test]
    fn launch_and_hand_back() {
        let p = launch(Harness::Pi, Some("/s/x.jsonl")).unwrap();
        assert_eq!(
            p.argv(),
            ["env", "PI_PROGRAM_STATUS=1", "pi", "--session", "/s/x.jsonl"]
        );
        assert_eq!(
            resume_inputs(Harness::Claude, "abc").unwrap()[0],
            Input::Text("claude --resume abc".into())
        );
        assert_eq!(exit_inputs(Harness::Pi).unwrap()[0], Input::Text("/quit".into()));
        assert_eq!(
            launch(Harness::Cursor, Some("c1")).unwrap().argv(),
            ["agent", "--resume", "c1"]
        );
        assert_eq!(
            launch(Harness::Opencode, Some("ses_1")).unwrap().argv(),
            ["opencode", "--session", "ses_1"]
        );
    }

    #[test]
    fn interrupt_and_exit_per_tui() {
        let interrupt = |h| inputs(h, &Command::Interrupt).unwrap();
        assert_eq!(interrupt(Harness::Copilot), [Input::Keys(vec![Key::Ctrl('c')])]);
        assert_eq!(interrupt(Harness::Opencode).len(), 2, "esc twice");
        // Cursor's interrupt leaves the prompt in the input: clear it first.
        assert_eq!(
            exit_inputs(Harness::Cursor).unwrap()[0],
            Input::Keys(vec![Key::Ctrl('u')])
        );
        assert_eq!(
            exit_inputs(Harness::Opencode).unwrap()[0],
            Input::Text("/exit".into())
        );
    }

    #[test]
    fn cursor_approval_dialog_on_screen() {
        let dialog = include_str!("../../tests/fixtures/screens/cursor-permission-shell.txt");
        assert!(shows_approval(Harness::Cursor, dialog));
        let working = include_str!("../../tests/fixtures/screens/cursor-working.txt");
        assert!(!shows_approval(Harness::Cursor, working));
    }

    #[test]
    fn claude_approval_dialog_on_screen() {
        let dialog = "● Running the build\n\n Bash command\n\n   cargo build\n   Build it\n\n \
            Do you want to proceed?\n ❯ 1. Yes\n   2. Yes, and don't ask again for cargo build commands\n   \
            3. No, and tell Claude what to do differently (esc)\n\n";
        assert!(shows_approval(Harness::Claude, dialog));
        let boxed = "│ Do you want to make this edit to main.rs?\n│ > 1. Yes\n│   2. No\n";
        assert!(shows_approval(Harness::Claude, boxed));
        let working = "● Bash(cargo build)\n  ⎿  Running…\n\n✻ Compiling… (12s · esc to interrupt)\n\n> \n  ? for shortcuts\n";
        assert!(!shows_approval(Harness::Claude, working));
        let quoted = "> Do you want to proceed? is what it asks\n● Sure.\n> \n";
        assert!(!shows_approval(Harness::Claude, quoted));
        assert!(!shows_approval(Harness::Pi, dialog));
    }
}
