//! Following the transcript files harnesses write for every session,
//! whoever started it: the conversation, state and usage, with nothing
//! installed on the host.

pub mod claude;
pub mod codex;
pub mod pi;

use acc_core::{Event, Harness};

/// Reads a harness's own transcript file to follow a session ACC didn't
/// start (state, usage), before or instead of taking it over.
#[derive(Debug)]
pub enum Observer {
    Claude(claude::Observer),
    Codex(codex::Observer),
    Pi(pi::Transcript),
}

impl Observer {
    /// `None` for harnesses whose transcripts ACC doesn't read yet.
    pub fn new(harness: Harness) -> Option<Self> {
        match harness {
            Harness::Claude => Some(Self::Claude(claude::Observer::new())),
            Harness::Codex => Some(Self::Codex(codex::Observer::new())),
            Harness::Pi => Some(Self::Pi(pi::Transcript::new())),
            Harness::Cursor | Harness::Copilot | Harness::Opencode => None,
        }
    }

    /// Feeds bytes appended to the transcript; stamps state changes with `now_ms`.
    pub fn feed(&mut self, bytes: &[u8], now_ms: u64) -> Vec<Event> {
        let mut events = match self {
            Self::Claude(o) => o.feed(bytes),
            Self::Codex(o) => o.feed(bytes),
            Self::Pi(o) => o.feed(bytes),
        };
        for event in &mut events {
            if let Event::StateChanged(s) = event
                && s.observed_at_ms == 0
            {
                s.observed_at_ms = now_ms;
            }
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn observer_exists_for_file_backed_harnesses() {
        assert!(Observer::new(Harness::Claude).is_some());
        assert!(Observer::new(Harness::Copilot).is_none());
    }
}
