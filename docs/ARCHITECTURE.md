# Architecture

## Goals and constraints

- **Sessions acc didn't start.** acc drives the sessions it launches and knows nothing else.
  observer covers the TUIs of every harness acc drives (Claude Code, Codex, Copilot CLI, Cursor
  Agent, opencode, pi) that a user started themselves, in a terminal or a tmux, zellij, screen or Rex pane: find them, follow them,
  co-drive their panes, and take them over into ordinary acc sessions.
- **acc's vocabulary.** Everything observer reports is acc's: `acc_core::Event`s,
  `session::Update`s with acc's `Transcript`, `SessionState` with a ranked source, and a
  `LaunchRequest` at the end of a takeover. A client renders a foreign session with the code it
  already has for acc sessions. Dependencies go one way: observer → acc.
- **Install nothing on hosts**, as with acc: every host action is a POSIX shell script for an SSH
  exec channel, using the harnesses' own files and the host's own multiplexer binary.
- **Sans-IO** like acc-core: scripts out, their output in. Observation lives in the client; nothing
  is written to the host until a takeover launches an acc session.

## Shape

```
            observer (Rust, sans-IO, on acc-core)
 ┌──────────────────────────────────────────────────────────────────┐
 │ discover   transcript (6 harnesses)   observe   takeover          │
 │ multiplexer (tmux, zellij, screen, Rex; tui keys, screen_match)   │
 │ osc7501                                                           │
 └──────────────┬──────────────────────────────┬────────────────────┘
                │ Exec scripts / bytes          │ acc Events, LaunchRequest
   observer-ssh: panes over an acc-ssh Host     acc-core / acc-ssh sessions
                │
   bindings: observer-ffi (on acc-ffi) · observer-wasm (on acc-wasm) · observer-cli
```

## Foreign sessions

`discover` lists harness TUIs acc didn't start: process, working directory, multiplexer pane (from
its environment), the wrapper it runs in (a `bwrap` sandbox, as an `acc_core::harness::Launcher`),
and its transcript (an open file, else the newest in the harness's directory).
`observe::Observed` follows that transcript (the readers in `transcript` emit full conversation
items, not just state), infers a waiting approval from a quiet outstanding tool call where the
transcript doesn't record approvals, and can type prompts into the pane. Its updates are acc's
`session::Update`s over acc's `Transcript`, so items, subagents and usage look the same as an acc
session's. The Copilot, Cursor and opencode readers shape tool calls like those harnesses' ACP
servers report them, so acc's item details render them as they do a driven session's.

| Harness | Where | Followed by | Approvals and questions |
|---|---|---|---|
| Claude Code | `~/.claude/projects/<cwd>/<id>.jsonl` (`sessions/<pid>.json` names it) | tail | `sessions/<pid>.json` status, else a quiet call checked on screen |
| Codex | `~/.codex/sessions/.../rollout-*-<thread>.jsonl` | tail | recorded in the rollout |
| Copilot CLI | `~/.copilot/session-state/<id>/events.jsonl` (the TUI holds `inuse.<pid>.lock` there) | tail | `permission.requested`, `ask_user` calls |
| Cursor Agent | `~/.cursor/projects/<cwd>/agent-transcripts/<id>/<id>.jsonl` | poll (`probe_exec`) | screen only: a quiet turn checked for the dialog |
| opencode 2.x | its server, through `opencode api` | poll (`probe_exec`) | `/permission`, `/form` |
| pi | `~/.pi/agent/sessions/--<cwd>--/*.jsonl` | tail | none |

Two transcripts can't be tailed. Cursor rewrites its file: a new turn truncates it back to the start
of the trailing `turn_ended` line, and a cancelled turn's lines are removed again, so a byte
offset would resume mid-line. opencode keeps one SQLite database behind a background service its
TUIs talk to. For both, `Observed::feed_entry` is `None` and `Observed::probe_exec` polls along with
the liveness check: Cursor's from the last offset nothing rewrites, opencode's session, pending
permissions and questions, and messages (only when the session moved or may be busy, plus running
subagents' sessions). `Observed::tick` reads the result. Cursor's transcript is lossy too: no tool
ids, results or usage, and a step is written once its tools have run.

`takeover::Takeover` is the state machine for moving it into an acc session: wait for a safe point
(or interrupt when forced), close the TUI the way a user would, confirm the process is gone, then
hand the client a `LaunchRequest` resuming the harness's session id inside the TUI's own wrapper.
Codex, Copilot and opencode resume over their headless protocol from the same store the TUI
writes. Cursor's ACP server keeps its own (`~/.cursor/acp-sessions/<id>`, the same SQLite store as
the TUI's `~/.cursor/chats/<md5 of cwd>/<id>`), so a Cursor takeover copies the chat there first
(`TakeoverStep::Preparing`) and the hand-back copies it back before `agent --resume`.
The client launches it as any acc session. The request's labels record where the TUI ran:
`observer.pane` (the pane, as `tmux:%3`) and `observer.transcript`. acc stores them in `spec.json`
and never reads them; `hand_back_session_exec` reads the pane label back and types the resume
command into that pane, after the client stopped the headless session.

**Subagents.** `Observed::children_exec` finds subagent transcripts not followed yet — Claude's
`subagents_script` next to the session, Codex's rollout `locate_script` for a spawned thread id —
and `Observed::on_children` matches what it finds to the spawning item.
`Observed::child_feed_entries` joins the session's own entry in the client's fan-in (acc's
`detached::fan_in_script`); `Observed::feed_child` routes each child's bytes through its own reader
(`transcript::claude::Observer::for_subagent`, `transcript::codex::Observer::for_child`), whose
items carry the spawning item as `parent` and whose state and usage are the parent's, not reported
again. Codex rollouts name a child's thread in `SubAgentActivity` records; the spawn's task is
encrypted (shown as no prompt) and the child's `FINAL_ANSWER` inter-agent message is its report.

## Multiplexers

`observer::multiplexer` drives harnesses running inside tmux, zellij, screen and Rex with the
host's own binary over SSH exec channels. A `Backend` per multiplexer builds the commands (list
panes, capture, send text/paste/keys, spawn, kill, watch) and parses their output; nothing is
installed.

| | Watch stream | OSC 7501 |
|---|---|---|
| tmux 3.2+ | control mode (`tmux -C attach -f ignore-size`), raw `%output` bytes | reports sent without the probe (pi with `PI_PROGRAM_STATUS=1`) |
| zellij 0.44+ | `zellij subscribe -f json`, rendered lines | no |
| screen 4.01+ | window log tailed with `tail -F` (best effort) | as tmux |
| Rex | `rex events --json`, native program-status records | yes, for every harness that emits it |

`multiplexer::tui` turns acc commands into keystrokes for co-driving a harness TUI in its pane (pi
fully; prompts, interrupt, exit, resume and common slash commands for the others; never images,
which only the headless protocols carry). Interrupt is Escape, but Ctrl+C for Copilot (Escape
edits its queued prompt) and Escape twice for opencode; Cursor puts an interrupted prompt back in
the input, so its exit clears the line before `/quit`. `multiplexer::screen_match` reads what only shows on screen:
Claude's permission, question and trust dialogs with their numbered options, and pi's busy state
before pi 1.x; `tui::shows_approval` spots Claude's and Cursor's approval dialogs. States from the screen carry `StateSource::ScreenMatch`, from OSC 7501
`StateSource::Osc7501`, from transcripts `StateSource::Transcript`; acc's `StateTracker` ranks
them. The Rex backend follows Rex's docs and is unverified until Rex ships a server outside the
macOS app. Tests drive real tmux, zellij and screen in CI.
`tests/live_tuis.rs` (ignored: signed-in harnesses, a few model calls) runs the Cursor, Copilot
and opencode TUIs in tmux through discovery, observation, takeover, `acc run --resume` and
hand-back.

### Panes over SSH (`observer-ssh`)

`observer-ssh` runs the multiplexer commands over an `acc_ssh::Host`: list, locate (from a harness
PID's environment), spawn and kill panes, and `Pane`, which co-drives a harness TUI. `Pane` turns
commands into keystrokes (permission answers keyed from the dialog on screen at that moment), and
its `next_events` streams the multiplexer's watch output for OSC 7501 states, reads the screen when
the pane draws (at most every `Pacing::min_gap`, at least every `Pacing::idle`), reopens the watch
after a dropped connection, and reports `Exited` once the pane is gone. A channel that fails to
open on a connection that died unnoticed is retried once on a fresh one (by `Host`), since the
script never started; anything that may have run (a keystroke) is never repeated.

CI drives a real OpenSSH server through a proxy that cuts connections mid-session, with tmux behind
it for `Pane`.

## Bindings

`observer-ffi` (UniFFI) depends on acc-ffi and is built into one library with it, so an app links
a single library and gets both APIs. Its records and errors are acc-ffi's (`AccError`, `AccExec`,
`AccUpdate`, `Event`, ...): `discover_exec`/`parse_discover`, `AccObserved`, `AccTakeover`,
`hand_back_exec`, `TranscriptObserver`, `Osc7501Scanner`, and over an acc-ffi `SshHost`,
`PaneHost` (`listPanes`, `locatePane`, `spawnPane`, `paneSession`) → `PaneSession` (`nextEvents`,
`send`, `sendInput`, `capture`, `exit`, `kill`). Library-mode bindgen on `libobserver_ffi` writes
`acc_ffi` and `observer_ffi` sources side by side.

`observer-wasm` (wasm-bindgen) depends on acc-wasm the same way: one module exports both, and
`tests/parity.rs` keeps it in step with observer-ffi except for the pane objects, which need raw
sockets. Values are plain JS objects in the core's serde shapes, as in acc-wasm.

## Research

The multiplexer, takeover and screen-reading decisions come from acc's research notes:
daemonless SSH control and takeover, state monitoring, and multiplexer control.
