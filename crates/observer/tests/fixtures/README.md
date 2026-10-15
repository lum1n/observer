# Recorded harness files

Session files and terminal captures recorded from real harnesses (2026-10-10, 2026-10-12 for the
subagent recordings, 2026-10-14 for the Codex subagent), replayed by the tests. They were recorded
alongside the driver fixtures in acc's `crates/acc-core/tests/fixtures`.

| File | Harness | What it covers |
|---|---|---|
| `claude/transcript.jsonl` | Claude Code 2.1.296 | the `~/.claude/projects` transcript of both sessions |
| `claude/subagents/<session>.jsonl` and `<session>/subagents/agent-*.{jsonl,meta.json}` | Claude Code 2.1.280 | the `~/.claude/projects` transcript of that session, with its two subagents' own transcripts and meta files |
| `codex/rollout.jsonl` | codex-cli 0.162.1 | the rollout file of that interrupted turn |
| `screens/claude-*.txt` | Claude Code 2.1.296 | `tmux capture-pane -p` of the TUI: Bash and Write permission dialogs (also at 50 columns), an AskUserQuestion dialog, the folder trust dialog, working and idle |
| `screens/pi-*.txt` | pi 0.82.1, 1.1.0 | the TUI in tmux, working and idle (0.82.1 sends no OSC 7501) |
| `codex/subagent-0.162.1-{parent,child}.rollout.jsonl` | codex-cli 0.162.1 | the two rollout files of that session: the spawn's encrypted task, `SubAgentActivity` records naming the thread, the child's `FINAL_ANSWER` as an inter-agent message |
| `pi/session.jsonl` | pi 1.1.0 | a pi session file: one turn with a tool call |

Edits made when recording: home paths and the Claude session id were replaced, and
environment-specific bookkeeping lines were dropped from the transcript files. The Codex rollouts
keep only the session metadata, conversation and events (no instructions, developer messages or
environment).
