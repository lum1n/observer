// Drives a foreign Claude Code session through the real wasm build:
// discovery, observation from its recorded transcript, takeover into an acc
// session, hand-back and OSC 7501. The module contains acc-wasm too.
//
//   cargo build -p observer-wasm --release --target wasm32-unknown-unknown
//   wasm-bindgen --target nodejs --out-dir bindings/node \
//     target/wasm32-unknown-unknown/release/observer_wasm.wasm
//   node crates/observer-wasm/tests/smoke.mjs bindings/node

import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import { resolve } from "node:path";

const require = createRequire(import.meta.url);
const obs = require(resolve(process.argv[2], "observer_wasm.js"));
const root = new URL("../../../", import.meta.url);
const transcript = readFileSync(new URL("crates/observer/tests/fixtures/claude/transcript.jsonl", root));

// What `discoverExec` prints for a Claude TUI in tmux pane %3.
const discovered = [
  "A\t4242\tclaude\t01:02:03\tnode /usr/lib/node_modules/claude/cli.js --model opus",
  "C\t4242\t/home/v/proj",
  "E\t4242\tTMUX=/tmp/tmux-1000/default,999,0",
  "E\t4242\tTMUX_PANE=%3",
  "N\t4242\t/home/v/.claude/projects/-home-v-proj/5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11.jsonl\t2048",
  "P\t%3\twork:2\t/tmp/tmux-1000/default",
  "",
].join("\n");

assert.ok(obs.discoverExec().script.includes("ps"));
const [agent] = obs.parseDiscover(discovered);
assert.equal(agent.harness, "claude");
assert.ok(agent.pane_id.startsWith("tmux:%3"));
assert.equal(obs.takeoverBlocker(agent, agent.native_id), undefined);

const observed = new obs.AccObserved(agent);
assert.ok(observed.feedEntry());
const updates = observed.feed(transcript, 1000);
assert.ok(updates.length > 0 && observed.items().length > 0);
assert.ok(observed.state());

const events = new obs.TranscriptObserver("claude").feed(transcript, 1000);
assert.ok(events.some((e) => e.type === "item_delta"));
assert.throws(() => new obs.TranscriptObserver("copilot"), /doesn't read/);

const takeover = new obs.AccTakeover(agent, agent.native_id, false);
const close = takeover.next({ type: "tick", now_ms: 0, safe: true });
assert.equal(close.kind, "run");
assert.equal(close.step, "closing_tui");
assert.ok(close.exec.script.includes("send-keys"));
assert.equal(takeover.next({ type: "ran", now_ms: 100, stdout: "" }).kind, "run");
const launch = takeover.next({ type: "ran", now_ms: 200, stdout: "gone\n" });
assert.equal(launch.kind, "launch");
assert.equal(launch.request.resume, agent.native_id);
assert.equal(launch.request.labels["observer.pane"], agent.pane_id);

// The acc session the takeover launches, from acc-wasm in the same module.
const session = obs.AccSession.launch(launch.request, "s1", "web", 1000);
assert.ok(session.takeLaunchExec().script.includes("nohup"));
const back = obs.handBackExec(session.spec(), agent.native_id);
assert.ok(back.script.includes("send-keys"));

const [status] = new obs.Osc7501Scanner().feed(new TextEncoder().encode("\x1b]7501;state=working:progress=40\x07"));
assert.deepEqual(status.state, { state: "working", progress: 40 });
assert.ok(obs.osc7501ProbeReply().length > 0);

console.log(`ok: ${updates.length} updates from a foreign session through wasm`);
