# Recorded harness files

Session files and terminal captures recorded from real harnesses (2026-10-10, 2026-10-12 for the
subagent recordings, 2026-10-14 for the Codex subagent, 2026-10-16 for Copilot, Cursor and
opencode), replayed by the tests. The earlier ones were recorded
alongside the driver fixtures in acc's `crates/acc-core/tests/fixtures`; the Copilot, Cursor and
opencode ones in tmux, the way `tests/live_tuis.rs` runs those TUIs.

| File | Harness | What it covers |
|---|---|---|
| `claude/transcript.jsonl` | Claude Code 2.1.296 | the `~/.claude/projects` transcript of both sessions |
| `claude/subagents/<session>.jsonl` and `<session>/subagents/agent-*.{jsonl,meta.json}` | Claude Code 2.1.280 | the `~/.claude/projects` transcript of that session, with its two subagents' own transcripts and meta files |
| `codex/rollout.jsonl` | codex-cli 0.162.1 | the rollout file of that interrupted turn |
| `screens/claude-*.txt` | Claude Code 2.1.296 | `tmux capture-pane -p` of the TUI: Bash and Write permission dialogs (also at 50 columns), an AskUserQuestion dialog, the folder trust dialog, working and idle |
| `screens/pi-*.txt` | pi 0.82.1, 1.1.0 | the TUI in tmux, working and idle (0.82.1 sends no OSC 7501) |
| `codex/subagent-0.162.1-{parent,child}.rollout.jsonl` | codex-cli 0.162.1 | the two rollout files of that session: the spawn's encrypted task, `SubAgentActivity` records naming the thread, the child's `FINAL_ANSWER` as an inter-agent message |
| `pi/session.jsonl` | pi 1.1.0 | a pi session file: one turn with a tool call |
| `copilot/events.jsonl` | Copilot CLI 1.0.98 | a TUI session's event log: a shell call, an approved write (`permission.requested`), an `ask_user` question, a sync `explore` subagent; then resumed and a slow command cancelled with Ctrl+C (`abort`) |
| `cursor/transcript.jsonl` | Cursor Agent 2026.10.01 | the TUI's transcript after an approved shell command, an answer cancelled with Escape, and a short turn |
| `opencode/{main,child}.messages.json`, `opencode/sessions.json` | opencode 2.0.14 | `opencode api` responses for a TUI session (a question answered, a foreground `explore` subagent) and its subagent's session, and the session list |
| `screens/cursor-*.txt` | Cursor Agent 2026.10.01 | the TUI in tmux: the shell approval dialog, and working |

Edits made when recording: home paths and the Claude session id were replaced, and
environment-specific bookkeeping lines were dropped from the transcript files. The Codex rollouts
keep only the session metadata, conversation and events (no instructions, developer messages or
environment). The Copilot log drops Copilot's own `model.*` and `hook.*` telemetry, system prompts,
encrypted reasoning and usage accounting; the opencode messages drop their snapshot hashes.
