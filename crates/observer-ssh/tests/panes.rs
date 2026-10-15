//! Co-drives a TUI in a tmux pane on a real OpenSSH server on localhost,
//! through a TCP proxy that can cut every connection to simulate a phone
//! losing its network.
//!
//! Skips when `sshd`, `ssh-keygen` or tmux is missing, unless
//! `ACC_REQUIRE_SSHD=1` and `ACC_REQUIRE_MUX=1` (set in CI).

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use acc_core::Event;
use acc_core::command::Command as AccCommand;
use acc_core::harness::Harness;
use acc_ssh::{Auth, Host, HostKeyCheck, SshConfig};
use observer::multiplexer::{Multiplexer, Program, Target};
use observer_ssh::{Error, Pane};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::watch;
use tokio::time::timeout;

#[path = "support/sshd.rs"]
mod support;

use support::{Sshd, start_sshd};

/// A TCP proxy to sshd. New connections are always accepted; existing ones
/// can be cut (like a phone switching networks).
struct Proxy {
    port: u16,
    cuts: watch::Sender<u32>,
}

impl Proxy {
    async fn start(target: u16) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let (cuts, rx) = watch::channel(0);
        tokio::spawn(async move {
            while let Ok((mut inbound, _)) = listener.accept().await {
                let mut rx = rx.clone();
                rx.mark_unchanged();
                tokio::spawn(async move {
                    let Ok(mut outbound) = TcpStream::connect(("127.0.0.1", target)).await else {
                        return;
                    };
                    tokio::select! {
                        _ = copy_bidirectional(&mut inbound, &mut outbound) => {}
                        _ = rx.changed() => {}
                    }
                });
            }
        });
        Self { port, cuts }
    }

    fn cut(&self) {
        self.cuts.send_modify(|n| *n += 1);
    }
}

fn host(sshd: &Sshd, port: u16, check: HostKeyCheck) -> Arc<Host> {
    host_with(port, check, Auth::key(&sshd.key, None).expect("key"))
}

fn host_with(port: u16, check: HostKeyCheck, auth: Auth) -> Arc<Host> {
    let mut config = SshConfig::new("127.0.0.1", support::user(), auth, check);
    config.port = port;
    config.connect_timeout = Duration::from_secs(5);
    Arc::new(Host::new(config))
}

/// Stands in for Claude Code's TUI: once the pane is wide enough, shows a
/// recorded permission dialog and waits for one key, which it saves to
/// `answer`. A moment later it reports working over OSC 7501 and shows the
/// recorded working screen.
fn fake_claude_tui(answer: &Path) -> Program {
    let screens = Path::new(env!("CARGO_MANIFEST_DIR")).join("../observer/tests/fixtures/screens");
    let script = r#"while [ "$(stty size | cut -d' ' -f2)" -lt 120 ]; do sleep 0.1; done
stty -icanon -echo min 1
cat "$1/claude-permission-bash.txt"
dd bs=1 count=1 of="$2" 2>/dev/null
sleep 1
printf '\033[2J\033[H\033]7501;state=working\033\\'
cat "$1/claude-working.txt"
exec cat >/dev/null"#;
    Program {
        program: "sh".into(),
        args: vec![
            "-c".into(),
            script.into(),
            "sh".into(),
            screens.display().to_string(),
            answer.display().to_string(),
        ],
        env: vec![],
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_drives_a_tui_in_a_tmux_pane() {
    if !Path::new("/usr/bin/tmux").exists() && !Path::new("/opt/homebrew/bin/tmux").exists() {
        assert!(std::env::var_os("ACC_REQUIRE_MUX").is_none(), "tmux is required");
        eprintln!("skipping: tmux not installed");
        return;
    }
    let Some(sshd) = start_sshd("tmux") else {
        return;
    };
    let proxy = Proxy::start(sshd.port).await;
    let h = host(
        &sshd,
        proxy.port,
        HostKeyCheck::Fingerprints(vec![sshd.fingerprint.clone()]),
    );
    let target = Target {
        multiplexer: Multiplexer::Tmux,
        server: sshd.dir.join("tmux.sock").display().to_string(),
        session: None,
    };
    let answer = sshd.dir.join("answer");
    let pane_ref = observer_ssh::spawn(&h, &target, &fake_claude_tui(&answer), "/tmp", "acc-test")
        .await
        .expect("spawn");
    h.exec(
        &format!(
            "tmux -S {} resize-window -t {} -x 160 -y 50",
            target.server, pane_ref.pane
        ),
        b"",
    )
    .await
    .expect("resize")
    .ok()
    .expect("resize ok");
    let listed = observer_ssh::list_panes(&h, &target).await.expect("list");
    assert!(listed.iter().any(|p| p.pane == pane_ref), "{listed:?}");

    let pane = Pane::new(h.clone(), pane_ref.clone(), Harness::Claude).with_pacing(observer_ssh::Pacing {
        min_gap: Duration::from_millis(100),
        idle: Duration::from_millis(500),
    });
    let next = || async {
        timeout(Duration::from_secs(20), pane.next_events())
            .await
            .expect("timed out")
            .expect("events")
    };
    let mut seen = Vec::new();
    let request = loop {
        let got = next().await;
        if let Some(Event::PermissionRequested { request_id, .. }) = got
            .iter()
            .find(|e| matches!(e, Event::PermissionRequested { .. }))
        {
            break request_id.clone();
        }
        seen.extend(got);
    };
    assert!(matches!(
        pane.state().map(|s| s.state),
        Some(acc_core::RunState::Blocked { .. })
    ));

    // The phone drops off the network; the answer still lands.
    proxy.cut();
    pane.send(AccCommand::AnswerPermission {
        request_id: request.clone(),
        option_id: "1".into(),
    })
    .await
    .expect("answer");
    let mut osc = false;
    let mut resolved = false;
    while !(osc && resolved) {
        for e in next().await {
            match e {
                Event::StateChanged(s)
                    if s.source == acc_core::StateSource::Osc7501
                        && matches!(s.state, acc_core::RunState::Working { .. }) =>
                {
                    osc = true;
                }
                Event::PermissionResolved { request_id } if request_id == request => resolved = true,
                _ => {}
            }
        }
    }
    assert_eq!(std::fs::read_to_string(&answer).expect("answer"), "1");

    // A dialog that's gone can't be answered.
    let stale = pane
        .send(AccCommand::AnswerPermission {
            request_id: request,
            option_id: "1".into(),
        })
        .await;
    assert!(matches!(stale, Err(Error::Driver(_))), "{stale:?}");

    pane.kill().await.expect("kill");
    let exit = next().await;
    assert!(matches!(exit.as_slice(), [.., Event::Exited { .. }]), "{exit:?}");
    let _ = h
        .exec(&format!("tmux -S {} kill-server", target.server), b"")
        .await;
}
