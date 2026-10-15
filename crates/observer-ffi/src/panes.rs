//! Harness TUIs in multiplexer panes on a host, co-driven over an acc-ffi
//! [`SshHost`] connection.
//!
//! These calls do I/O, so they are `async` in Swift and `suspend` in Kotlin,
//! on the tokio runtime acc-ffi starts. Every object is safe to share: one
//! task can wait on `next_events` while another sends commands.

use std::sync::Arc;

use acc_ffi::{AccError, Command, Event, Harness, SessionState, SshHost};
use observer::multiplexer as core_mux;

fn ssh_error(e: observer_ssh::Error) -> AccError {
    match e {
        observer_ssh::Error::Ssh(e) => e.into(),
        observer_ssh::Error::Driver(e) => e.into(),
        observer_ssh::Error::Mux(e @ core_mux::MuxError::Unsupported(..)) => AccError::Unsupported {
            reason: e.to_string(),
        },
        observer_ssh::Error::Mux(e @ core_mux::MuxError::Parse(..)) => AccError::Remote {
            exit_code: Some(0),
            stderr: e.to_string(),
        },
    }
}

/// Panes on the host an [`SshHost`] connects to.
#[derive(Debug, uniffi::Object)]
pub struct PaneHost {
    host: Arc<acc_ssh::Host>,
}

#[uniffi::export(async_runtime = "tokio")]
impl PaneHost {
    #[uniffi::constructor]
    pub fn new(host: Arc<SshHost>) -> Self {
        Self { host: host.host() }
    }

    /// The panes of `target` (every session when it names none).
    pub async fn list_panes(&self, target: MuxTarget) -> Result<Vec<PaneInfo>, AccError> {
        let panes = observer_ssh::list_panes(&self.host, &target.into())
            .await
            .map_err(ssh_error)?;
        Ok(panes.into_iter().map(PaneInfo::from).collect())
    }

    /// The pane process `pid` runs in, if it runs in a multiplexer: for taking
    /// over a harness the user started in tmux, zellij, screen or Rex.
    pub async fn locate_pane(&self, pid: u32) -> Result<Option<PaneInfo>, AccError> {
        Ok(observer_ssh::locate(&self.host, pid)
            .await
            .map_err(ssh_error)?
            .map(PaneInfo::from))
    }

    /// Co-drives `harness`'s TUI running in `pane`.
    pub fn pane_session(&self, pane: PaneRef, harness: Harness) -> PaneSession {
        PaneSession {
            inner: observer_ssh::Pane::new(self.host.clone(), pane.into(), harness.into()),
        }
    }

    /// Starts `harness`'s TUI in a new background pane of `target` in `cwd`,
    /// resuming the harness's session id where its TUI can. `name` labels the
    /// pane (a window, tab or block) and should be unique.
    pub async fn spawn_pane(
        &self,
        target: MuxTarget,
        harness: Harness,
        cwd: String,
        name: String,
        resume: Option<String>,
    ) -> Result<PaneSession, AccError> {
        let pane = observer_ssh::Pane::spawn(
            self.host.clone(),
            &target.into(),
            harness.into(),
            &cwd,
            &name,
            resume.as_deref(),
        )
        .await
        .map_err(ssh_error)?;
        Ok(PaneSession { inner: pane })
    }
}

// ---------------------------------------------------------------------------
// Multiplexers

#[derive(Debug, Clone, Copy, PartialEq, Eq, uniffi::Enum)]
pub enum Multiplexer {
    Tmux,
    Zellij,
    Screen,
    Rex,
}

impl From<Multiplexer> for core_mux::Multiplexer {
    fn from(m: Multiplexer) -> Self {
        match m {
            Multiplexer::Tmux => Self::Tmux,
            Multiplexer::Zellij => Self::Zellij,
            Multiplexer::Screen => Self::Screen,
            Multiplexer::Rex => Self::Rex,
        }
    }
}

impl From<core_mux::Multiplexer> for Multiplexer {
    fn from(m: core_mux::Multiplexer) -> Self {
        match m {
            core_mux::Multiplexer::Tmux => Self::Tmux,
            core_mux::Multiplexer::Zellij => Self::Zellij,
            core_mux::Multiplexer::Screen => Self::Screen,
            core_mux::Multiplexer::Rex => Self::Rex,
        }
    }
}

/// A multiplexer server, optionally narrowed to one session.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct MuxTarget {
    pub multiplexer: Multiplexer,
    /// tmux socket path (empty: the default server), zellij or screen
    /// session name, or Rex server address (empty: the local server).
    pub server: String,
    #[uniffi(default = None)]
    pub session: Option<String>,
}

impl From<MuxTarget> for core_mux::Target {
    fn from(t: MuxTarget) -> Self {
        Self {
            multiplexer: t.multiplexer.into(),
            server: t.server,
            session: t.session,
        }
    }
}

/// Where a harness runs inside a multiplexer.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PaneRef {
    pub multiplexer: Multiplexer,
    pub server: String,
    pub session: Option<String>,
    /// tmux `%N`, zellij `terminal_N`, screen window number, Rex block id.
    pub pane: String,
}

impl From<PaneRef> for core_mux::PaneRef {
    fn from(p: PaneRef) -> Self {
        Self {
            multiplexer: p.multiplexer.into(),
            server: p.server,
            session: p.session,
            pane: p.pane,
        }
    }
}

impl From<core_mux::PaneRef> for PaneRef {
    fn from(p: core_mux::PaneRef) -> Self {
        Self {
            multiplexer: p.multiplexer.into(),
            server: p.server,
            session: p.session,
            pane: p.pane,
        }
    }
}

/// One pane as the multiplexer lists it.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Record)]
pub struct PaneInfo {
    pub pane: PaneRef,
    /// The process the pane started (tmux only).
    pub pid: Option<u32>,
    pub tty: Option<String>,
    /// The foreground process name, where reported (`claude`, `node`...).
    pub command: Option<String>,
    pub cwd: Option<String>,
    pub title: Option<String>,
    pub active: bool,
    /// The program exited but the pane is kept.
    pub exited: bool,
}

impl From<core_mux::PaneInfo> for PaneInfo {
    fn from(p: core_mux::PaneInfo) -> Self {
        Self {
            pane: p.pane.into(),
            pid: p.pid,
            tty: p.tty,
            command: p.command,
            cwd: p.cwd,
            title: p.title,
            active: p.active,
            exited: p.exited,
        }
    }
}

/// A key press.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PaneKey {
    Enter,
    Escape,
    Tab,
    Backspace,
    Up,
    Down,
    Left,
    Right,
    /// Ctrl plus one ASCII letter.
    Ctrl {
        letter: String,
    },
    /// One printable character, such as a menu digit.
    Char {
        character: String,
    },
}

/// Input for a pane.
#[derive(Debug, Clone, PartialEq, Eq, uniffi::Enum)]
pub enum PaneInput {
    /// Typed literally; no newlines (use `Paste`).
    Text {
        text: String,
    },
    /// One bracketed paste, so newlines don't submit early.
    Paste {
        text: String,
    },
    Keys {
        keys: Vec<PaneKey>,
    },
}

fn one_char(s: &str, what: &str) -> Result<char, AccError> {
    let mut chars = s.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => Ok(c),
        _ => Err(AccError::InvalidArgument {
            reason: format!("{what} must be one character, got {s:?}"),
        }),
    }
}

impl TryFrom<PaneInput> for core_mux::Input {
    type Error = AccError;

    fn try_from(i: PaneInput) -> Result<Self, AccError> {
        use core_mux::Key as K;
        Ok(match i {
            PaneInput::Text { text } => Self::Text(text),
            PaneInput::Paste { text } => Self::Paste(text),
            PaneInput::Keys { keys } => Self::Keys(
                keys.into_iter()
                    .map(|k| {
                        Ok(match k {
                            PaneKey::Enter => K::Enter,
                            PaneKey::Escape => K::Escape,
                            PaneKey::Tab => K::Tab,
                            PaneKey::Backspace => K::Backspace,
                            PaneKey::Up => K::Up,
                            PaneKey::Down => K::Down,
                            PaneKey::Left => K::Left,
                            PaneKey::Right => K::Right,
                            PaneKey::Ctrl { letter } => {
                                let c = one_char(&letter, "a Ctrl key")?;
                                if !c.is_ascii_alphabetic() {
                                    return Err(AccError::InvalidArgument {
                                        reason: format!("Ctrl takes an ASCII letter, got {letter:?}"),
                                    });
                                }
                                K::Ctrl(c)
                            }
                            PaneKey::Char { character } => K::Char(one_char(&character, "a key")?),
                        })
                    })
                    .collect::<Result<_, AccError>>()?,
            ),
        })
    }
}

/// A harness TUI in a multiplexer pane, co-driven over SSH: the user can keep
/// using the pane, and the app types into it like they would.
#[derive(Debug, uniffi::Object)]
pub struct PaneSession {
    inner: observer_ssh::Pane,
}

#[uniffi::export(async_runtime = "tokio")]
impl PaneSession {
    pub fn pane(&self) -> PaneRef {
        self.inner.pane().clone().into()
    }

    pub fn harness(&self) -> Harness {
        self.inner.harness().into()
    }

    /// The current state, merged across OSC 7501 and the screen.
    pub fn state(&self) -> Option<SessionState> {
        self.inner.state().as_ref().map(SessionState::from)
    }

    /// Waits for the next events: state changes, and permission dialogs
    /// opening and closing (read off the screen for Claude and pi). Returns
    /// `[Exited]` once the pane is gone.
    pub async fn next_events(&self) -> Result<Vec<Event>, AccError> {
        let events = self.inner.next_events().await.map_err(ssh_error)?;
        Ok(events.into_iter().map(Event::from).collect())
    }

    /// Reads the screen once and returns what changed.
    pub async fn poll(&self) -> Result<Vec<Event>, AccError> {
        let events = self.inner.poll().await.map_err(ssh_error)?;
        Ok(events.into_iter().map(Event::from).collect())
    }

    /// Carries out a command by typing into the TUI. Answers to permission
    /// requests are keyed from the dialog on screen now.
    pub async fn send(&self, command: Command) -> Result<(), AccError> {
        self.inner.send(command.into()).await.map_err(ssh_error)
    }

    /// Delivers raw input, in order.
    pub async fn send_input(&self, inputs: Vec<PaneInput>) -> Result<(), AccError> {
        let inputs = inputs
            .into_iter()
            .map(core_mux::Input::try_from)
            .collect::<Result<Vec<_>, _>>()?;
        self.inner.send_input(&inputs).await.map_err(ssh_error)
    }

    /// The pane's rendered text.
    pub async fn capture(&self, scrollback: bool) -> Result<String, AccError> {
        self.inner.capture(scrollback).await.map_err(ssh_error)
    }

    /// Asks the TUI to exit the way a user would.
    pub async fn exit(&self) -> Result<(), AccError> {
        self.inner.exit().await.map_err(ssh_error)
    }

    /// Closes the pane, ending its program.
    pub async fn kill(&self) -> Result<(), AccError> {
        self.inner.kill().await.map_err(ssh_error)
    }

    /// Stops watching the pane and undoes what watching set up on the host;
    /// the pane keeps running.
    pub async fn stop_watching(&self) -> Result<(), AccError> {
        self.inner.close().await.map_err(ssh_error)
    }
}
