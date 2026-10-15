"""Drives a foreign Claude Code session through the real UniFFI library:
discovery, observation from its recorded transcript, takeover into an acc
session, hand-back, OSC 7501 and the pane surface.

Run after generating the Python bindings next to the built library. The
bindings import each other, so they go in a package:

    cargo build -p observer-ffi --release
    cargo run -p observer-ffi --features bindgen --bin uniffi-bindgen -- generate \
        --library target/release/libobserver_ffi.so --language python --out-dir bindings/python/ffi
    cp target/release/libobserver_ffi.so bindings/python/ffi/
    touch bindings/python/ffi/__init__.py
    python3 crates/observer-ffi/tests/smoke.py bindings/python/ffi

The Swift and Kotlin bindings are generated from the same library metadata,
so this exercises the exact FFI calls and type conversions they use.
"""

import asyncio
import importlib
import pathlib
import socket
import sys

PKG = pathlib.Path(sys.argv[1]).resolve()
sys.path.insert(0, str(PKG.parent))
acc = importlib.import_module(f"{PKG.name}.acc_ffi")
obs = importlib.import_module(f"{PKG.name}.observer_ffi")

ROOT = pathlib.Path(__file__).resolve().parents[3]
TRANSCRIPT = ROOT / "crates/observer/tests/fixtures/claude/transcript.jsonl"

# What `discover_exec` prints for a Claude TUI in tmux pane %3.
DISCOVERED = (
    "A\t4242\tclaude\t01:02:03\tnode /usr/lib/node_modules/claude/cli.js --model opus\n"
    "C\t4242\t/home/v/proj\n"
    "E\t4242\tTMUX=/tmp/tmux-1000/default,999,0\n"
    "E\t4242\tTMUX_PANE=%3\n"
    "N\t4242\t/home/v/.claude/projects/-home-v-proj/5f3c1a52-8a3e-4c1e-9d47-2b6f0e9a7c11.jsonl\t2048\n"
    "P\t%3\twork:2\t/tmp/tmux-1000/default\n"
)


def main() -> None:
    assert "ps" in obs.discover_exec().script
    (agent,) = obs.parse_discover(DISCOVERED)
    assert agent.harness == acc.AccHarness.CLAUDE and agent.pid == 4242
    assert agent.pane and agent.pane.startswith("tmux:%3")
    assert obs.takeover_blocker(agent.json, agent.native_id) is None

    transcript = TRANSCRIPT.read_bytes()
    observed = obs.AccObserved(agent.json)
    assert observed.feed_entry() is not None
    updates = observed.feed(transcript, 1_000)
    assert updates and observed.items(), "the transcript produced items"
    assert observed.state() is not None
    assert observed.alive_exec().script

    events = obs.TranscriptObserver(acc.Harness.CLAUDE).feed(transcript, 1_000)
    assert any(isinstance(e, acc.Event.ITEM_DELTA) for e in events)
    try:
        obs.TranscriptObserver(acc.Harness.COPILOT)
        raise AssertionError("expected Unsupported")
    except acc.AccError.Unsupported:
        pass

    takeover = obs.AccTakeover(agent.json, agent.native_id, False)
    close = takeover.next(obs.AccTakeoverInput.TICK(now_ms=0, safe=True))
    assert close.step == obs.AccTakeoverStep.CLOSING_TUI and "send-keys" in close.exec.script
    assert isinstance(takeover.next(obs.AccTakeoverInput.RAN(now_ms=100, stdout="")), obs.AccTakeoverAction.RUN)
    launch = takeover.next(obs.AccTakeoverInput.RAN(now_ms=200, stdout="gone\n"))
    assert isinstance(launch, obs.AccTakeoverAction.LAUNCH)
    request = launch.request
    assert request.resume == agent.native_id and request.labels["observer.pane"] == agent.pane

    session = acc.AccSession.launch(request, "s1", "py", 1_000)
    assert "nohup" in session.take_launch_exec().script
    back = obs.hand_back_exec(session.spec().json, agent.native_id)
    assert back is not None and "send-keys" in back.script

    (status,) = obs.Osc7501Scanner().feed(b"\x1b]7501;state=working:progress=40\x07")
    assert status.state == acc.ProgramState.WORKING and status.progress == 40
    assert obs.osc7501_probe_reply()

    asyncio.run(pane_calls())
    print(f"ok: {len(updates)} updates from a foreign session through the FFI")


async def pane_calls() -> None:
    """The async pane surface over an acc-ffi SshHost: a rejected input
    comes back as a typed error through the foreign future."""
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        closed_port = s.getsockname()[1]
    host = acc.SshHost(
        acc.SshConfig(
            host="127.0.0.1",
            port=closed_port,
            user="nobody",
            auth=acc.SshAuth.PASSWORD(password="x"),
            host_key=acc.HostKeyCheck.ACCEPT_ANY(),
            connect_timeout_ms=2_000,
            keepalive_interval_ms=None,
            keepalive_max=None,
        )
    )
    pane = obs.PaneHost(host).pane_session(
        obs.PaneRef(multiplexer=obs.Multiplexer.TMUX, server="", session=None, pane="%0"),
        acc.Harness.CLAUDE,
    )
    assert pane.pane().pane == "%0" and pane.state() is None
    try:
        await pane.send_input([obs.PaneInput.KEYS(keys=[obs.PaneKey.CTRL(letter="12")])])
        raise AssertionError("expected an invalid argument")
    except acc.AccError.InvalidArgument:
        pass
    await pane.stop_watching()
    await host.disconnect()


main()
