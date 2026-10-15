//! Harness sessions acc didn't start.
//!
//! acc drives the sessions it launches. This crate covers everything else:
//! harness TUIs the user started themselves, in a terminal or a multiplexer
//! pane. It finds them, follows them through the transcript files the
//! harnesses write, reads their screens, types into their panes, and takes
//! them over into ordinary acc sessions. What it produces is acc's own
//! vocabulary ([`acc_core::Event`], [`acc_core::session::Update`],
//! [`acc_core::session::LaunchRequest`]), so a client renders an observed
//! session the same way as one acc started.
//!
//! Like acc-core it is **sans-IO**: every host action is an
//! [`acc_core::Exec`] for the embedder to run (over SSH, say), and every
//! parser takes the bytes it printed.
//!
//! Layout:
//! - [`discover`]: harness processes on a host, their panes and transcripts.
//! - [`transcript`]: readers for Claude's, Codex's and pi's transcript files.
//! - [`observe`]: one foreign session followed through its transcript.
//! - [`multiplexer`]: tmux, zellij, screen and Rex panes: list, capture,
//!   type, watch; reading TUI screens and typing harness commands.
//! - [`osc7501`]: the OSC 7501 terminal status protocol.
//! - [`takeover`]: closing a TUI safely and resuming it as an acc session,
//!   and handing it back.

pub mod discover;
pub mod multiplexer;
pub mod observe;
pub mod osc7501;
pub mod takeover;
pub mod transcript;
mod wire;
