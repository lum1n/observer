# observer

Harness sessions [acc](https://github.com/lum1n/acc) didn't start. acc drives the sessions it
launches; observer handles the rest: Claude Code, Codex or pi TUIs a user started themselves, in a
terminal or a tmux, zellij, screen or Rex pane. It finds them, follows them through the transcript
files the harnesses write, reads their screens, types into their panes, and takes them over into
ordinary acc sessions, which acc then drives like any other.

Everything it reports is in acc's vocabulary (events, transcript items, states, launch requests),
so a client shows a foreign session with the same code as an acc session. Like acc, it needs
nothing installed on the hosts: every action is a shell script for an SSH exec channel.

## Workspace

| Crate | What it is |
|---|---|
| `crates/observer` | Sans-IO core on acc-core: discovery, transcript readers (Claude, Codex, pi), observation, takeover and hand-back, multiplexer backends (tmux, zellij, screen, Rex) with TUI keystrokes and screen matching, OSC 7501 |
| `crates/observer-ssh` | Co-drives TUIs in panes over an `acc_ssh::Host` |
| `crates/observer-ffi` | UniFFI bindings for Swift and Kotlin, built into one library with acc-ffi and using its types |
| `crates/observer-wasm` | wasm-bindgen bindings for browsers and Node, built into one module with acc-wasm (a test keeps them level with observer-ffi) |
| `crates/observer-cli` | `observer` command-line tool |

acc is a path dependency: check it out next to this repository (`../acc`).

## Quick start

```sh
cargo test --workspace

# Multiplexers, this shell's pane and harness session stores on this machine
cargo run -q -p observer-cli -- doctor

# Swift / Kotlin bindings for acc and observer from one library
cargo build -p observer-ffi --release
cargo run -p observer-ffi --features bindgen --bin uniffi-bindgen -- \
  generate --library target/release/libobserver_ffi.so --language swift --language kotlin --out-dir bindings

# JS bindings (browser: --target web)
cargo build -p observer-wasm --release --target wasm32-unknown-unknown
wasm-bindgen --target nodejs --out-dir bindings/node target/wasm32-unknown-unknown/release/observer_wasm.wasm
```

From Swift, with acc-ffi's `SshHost`:

```swift
// A Claude Code the user started in tmux: read its dialogs, answer them.
let panes = PaneHost(host: host)
let pane = panes.paneSession(pane: info.pane, harness: .claude)   // from listPanes / locatePane
try await pane.send(command: .answerPermission(requestId: id, optionId: "1"))

// Take a TUI over into an acc session.
let out = try await host.exec(script: discoverExec().script, stdin: Data())
let agents = parseDiscover(stdout: String(decoding: out.stdout, as: UTF8.self))
let takeover = try AccTakeover(agentJson: agents[0].json, nativeId: agents[0].nativeId, force: false)
// step it with what you observe; on .launch(request), start it with AccSession.launch
```

## Try it by hand

```sh
cargo install --path crates/observer-cli   # puts `observer` in ~/.cargo/bin
observer doctor
```

**Resume a session headlessly.** `observer sessions <claude|codex|pi>` lists recent sessions on
this machine with the `acc run` command that resumes each one. Quit the harness's own TUI first
(Claude resumes per project, so run it from the session's directory):

```sh
observer sessions claude
acc run claude . --resume <session-id>
```

**Inside tmux, zellij or screen.** Leave the harness running in its pane and drive it from any
other terminal; each `observer mux` command prints a shell script, so pipe it to `sh` (or to
`ssh host sh` for a remote machine):

```sh
observer mux tmux - - list | sh                      # find the harness's pane, e.g. %3
observer mux tmux - - capture %3 | sh                # what the pane shows now
observer mux tmux - - type %3 "fix the tests" | sh   # type a prompt and press Enter
observer mux zellij - - list | sh                    # zellij: panes of every session
observer mux zellij <session> - type terminal_1 "fix the tests" | sh
```

**Read the screen and answer dialogs.** Some state only shows on screen: Claude Code's permission
and question dialogs, and pi's busy spinner before pi 1.x. `observer decode screen <claude|pi>`
reads a captured pane and prints its state and any open dialog with its numbered options; `keys`
presses the one you pick (or `escape` to deny):

```sh
observer mux tmux - - capture %3 | sh | observer decode screen claude
observer mux tmux - - keys %3 1 | sh                  # "1. Yes"
```

**Find TUIs on a host.** `observer discover` prints the discovery script;
`observer decode discover` turns its output into JSON (process, directory, pane, transcript):

```sh
observer discover | ssh devbox sh | observer decode discover
```

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) for the design.
