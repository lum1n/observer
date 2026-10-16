//! UniFFI surface of observer for Swift (iOS/macOS) and Kotlin (Android/JVM),
//! next to acc-ffi's.
//!
//! The library contains acc-ffi, so an app links one library and gets both:
//! acc's sessions, and harness TUIs the user started themselves. Records and
//! errors are acc-ffi's (`AccError`, `AccExec`, `Event`, ...), so a foreign
//! session renders like an acc session.
//!
//! - Sans-IO ([`observe`]): discover TUIs, follow them through their
//!   transcripts ([`AccObserved`], [`TranscriptObserver`]), and take them
//!   over into an acc session ([`AccTakeover`]). The app runs the scripts
//!   and feeds back their output.
//! - Over an acc-ffi `SshHost` ([`panes`]): list, spawn and co-drive TUIs in
//!   tmux, zellij, screen or Rex panes ([`PaneHost`], [`PaneSession`]).
//! - OSC 7501 status reports in terminal output ([`Osc7501Scanner`]).

use std::sync::{Mutex, MutexGuard};

use acc_ffi::{AccError, Event, Harness, ProgramState, from_run_state};
use observer::osc7501::{self, Osc7501};

pub mod observe;
pub mod panes;

pub use observe::*;
pub use panes::*;

uniffi::setup_scaffolding!();

/// Locks a mutex, recovering from poisoning: every core call leaves its
/// state consistent, so a panic in one call shouldn't brick the object.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Follows a harness's own transcript file (Claude, Codex, pi) to observe a
/// session ACC didn't start, before or instead of taking it over.
#[derive(Debug, uniffi::Object)]
pub struct TranscriptObserver {
    inner: Mutex<observer::transcript::Observer>,
}

#[uniffi::export]
impl TranscriptObserver {
    #[uniffi::constructor]
    pub fn new(harness: Harness) -> Result<Self, AccError> {
        let h: acc_core::Harness = harness.into();
        let inner = observer::transcript::Observer::new(h).ok_or_else(|| AccError::Unsupported {
            reason: format!("observer doesn't read {} transcripts", h.display_name()),
        })?;
        Ok(Self {
            inner: Mutex::new(inner),
        })
    }

    /// Feeds bytes appended to the transcript.
    pub fn feed(&self, bytes: Vec<u8>, now_ms: u64) -> Vec<Event> {
        lock(&self.inner)
            .feed(&bytes, now_ms)
            .into_iter()
            .map(Event::from)
            .collect()
    }
}

// ---------------------------------------------------------------------------
// OSC 7501

/// One OSC 7501 message seen in terminal output.
#[derive(Debug, Clone, uniffi::Record)]
pub struct ProgramStatus {
    /// True for a probe (`OSC 7501 ; ?`); the terminal should reply with
    /// [`osc7501_probe_reply`] before its DA1 response.
    pub is_probe: bool,
    pub state: Option<ProgramState>,
    pub progress: Option<u8>,
    pub id: Option<String>,
    pub app: Option<String>,
    pub title: Option<String>,
    pub message: Option<String>,
}

/// Bytes a terminal writes back to answer an OSC 7501 probe.
#[uniffi::export]
pub fn osc7501_probe_reply() -> Vec<u8> {
    osc7501::PROBE_REPLY.to_vec()
}

/// Incremental OSC 7501 extractor for a terminal byte stream.
#[derive(Debug, uniffi::Object)]
pub struct Osc7501Scanner {
    inner: Mutex<osc7501::Scanner>,
}

#[uniffi::export]
impl Osc7501Scanner {
    #[uniffi::constructor]
    pub fn new() -> Self {
        Self {
            inner: Mutex::new(osc7501::Scanner::new()),
        }
    }

    /// Feed raw terminal output; returns any status messages it completed.
    pub fn feed(&self, bytes: Vec<u8>) -> Vec<ProgramStatus> {
        lock(&self.inner)
            .feed(&bytes)
            .into_iter()
            .map(to_status)
            .collect()
    }
}

impl Default for Osc7501Scanner {
    fn default() -> Self {
        Self::new()
    }
}

fn to_status(msg: Osc7501) -> ProgramStatus {
    match msg {
        Osc7501::Probe => ProgramStatus {
            is_probe: true,
            state: None,
            progress: None,
            id: None,
            app: None,
            title: None,
            message: None,
        },
        Osc7501::Report(r) => {
            let (state, progress) = from_run_state(r.state.as_ref());
            ProgramStatus {
                is_probe: false,
                state: Some(state),
                progress,
                id: r.id,
                app: r.app,
                title: r.title,
                message: r.message,
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn scanner_reports_states() {
        let got = Osc7501Scanner::new().feed(b"\x1b]7501;state=working:progress=40\x07".to_vec());
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].state, Some(ProgramState::Working));
        assert_eq!(got[0].progress, Some(40));
    }

    #[test]
    fn transcript_observer_only_for_file_backed_harnesses() {
        assert!(TranscriptObserver::new(Harness::Claude).is_ok());
        assert!(matches!(
            TranscriptObserver::new(Harness::Opencode),
            Err(AccError::Unsupported { .. })
        ));
    }
}
