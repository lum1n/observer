//! Harness TUIs in tmux, zellij, screen and Rex panes on a host, driven over
//! SSH with nothing installed there.
//!
//! Builds on [`acc_ssh::Host`] for the connection and on [`observer`] for
//! the multiplexer commands and parsers. [`Pane`] co-drives a TUI the user
//! may also be looking at; [`list_panes`], [`locate`] and [`spawn`] find and
//! start panes. Everything is async on tokio; the caller owns the runtime.

mod error;
mod pane;

pub use error::Error;
pub use pane::{Pacing, Pane, list_panes, locate, run, spawn};

pub type Result<T, E = Error> = std::result::Result<T, E>;
