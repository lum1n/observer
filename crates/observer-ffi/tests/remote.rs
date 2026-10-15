//! The pane bindings against a real OpenSSH server on localhost with tmux
//! behind it, called exactly as Swift and Kotlin call them.
//!
//! Skips when `sshd` or tmux is missing, unless `ACC_REQUIRE_SSHD=1` and
//! `ACC_REQUIRE_MUX=1` (set in CI).

#![cfg(unix)]
#![allow(clippy::expect_used, clippy::unwrap_used, clippy::panic)]

#[path = "../../observer-ssh/tests/support/sshd.rs"]
mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use acc_ffi::*;
use observer_ffi::*;
use tokio::time::timeout;

fn config(sshd: &support::Sshd, host_key: HostKeyCheck) -> SshConfig {
    SshConfig {
        host: "127.0.0.1".into(),
        port: sshd.port,
        user: support::user(),
        auth: SshAuth::PrivateKey {
            key: sshd.key.clone(),
            passphrase: None,
        },
        host_key,
        connect_timeout_ms: Some(5_000),
        keepalive_interval_ms: None,
        keepalive_max: None,
    }
}

async fn events_until(
    next: impl AsyncFn() -> Result<Vec<Event>, AccError>,
    stop: impl Fn(&Event) -> bool,
) -> Vec<Event> {
    let mut seen = Vec::new();
    loop {
        let got = timeout(Duration::from_secs(20), next())
            .await
            .expect("timed out")
            .expect("events");
        let done = got.iter().any(&stop);
        seen.extend(got);
        if done {
            return seen;
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn co_drives_a_pane() {
    if !Path::new("/usr/bin/tmux").exists() && !Path::new("/opt/homebrew/bin/tmux").exists() {
        assert!(std::env::var_os("ACC_REQUIRE_MUX").is_none(), "tmux is required");
        eprintln!("skipping: tmux not installed");
        return;
    }
    let Some(sshd) = support::start_sshd("ffi-pane") else {
        return;
    };
    let ssh = Arc::new(SshHost::new(config(&sshd, HostKeyCheck::AcceptAny)).unwrap());
    let host = PaneHost::new(ssh.clone());
    let socket = sshd.dir.join("tmux.sock").display().to_string();
    let target = MuxTarget {
        multiplexer: Multiplexer::Tmux,
        server: socket.clone(),
        session: None,
    };
    // A shell in a wide pane, standing in for the user's own window.
    let started = ssh
        .exec(
            format!("tmux -S '{socket}' new-session -d -x 160 -y 50 -P -F '#{{pane_id}}' sh"),
            vec![],
        )
        .await
        .unwrap();
    assert_eq!(started.exit_code, Some(0));
    let pane_id = String::from_utf8(started.stdout).unwrap().trim().to_owned();

    let panes = host.list_panes(target.clone()).await.unwrap();
    let info = panes
        .iter()
        .find(|p| p.pane.pane == pane_id)
        .expect("pane listed");
    let pid = info.pid.expect("tmux reports the pane pid");
    let located = host.locate_pane(pid).await.unwrap().expect("located");
    assert_eq!(located.pane.pane, pane_id);

    // A Claude permission dialog shows in the pane.
    let screen = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../observer/tests/fixtures/screens/claude-permission-bash.txt");
    let pane = host.pane_session(located.pane.clone(), Harness::Claude);
    pane.send_input(vec![
        PaneInput::Text {
            text: format!(
                "clear; cat '{}'; read -r k; echo \"got $k\" > '{}/got'",
                screen.display(),
                sshd.dir.display()
            ),
        },
        PaneInput::Keys {
            keys: vec![PaneKey::Enter],
        },
    ])
    .await
    .unwrap();
    let seen = events_until(
        async || pane.next_events().await,
        |e| matches!(e, Event::PermissionRequested { .. }),
    )
    .await;
    let Some(Event::PermissionRequested {
        request_id, options, ..
    }) = seen
        .iter()
        .find(|e| matches!(e, Event::PermissionRequested { .. }))
    else {
        panic!("no dialog in {seen:?}");
    };
    assert_eq!(
        options.iter().map(|o| o.id.as_str()).collect::<Vec<_>>(),
        ["1", "2", "3", "4", "cancel"]
    );
    assert_eq!(
        pane.state().map(|s| s.state),
        Some(ProgramState::BlockedPermission)
    );

    // Answer "No": the digit, then Enter so `read` returns.
    pane.send(Command::AnswerPermission {
        request_id: request_id.clone(),
        option_id: "4".into(),
    })
    .await
    .unwrap();
    pane.send_input(vec![PaneInput::Keys {
        keys: vec![PaneKey::Enter],
    }])
    .await
    .unwrap();
    let got = sshd.dir.join("got");
    for _ in 0..100 {
        if got.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(std::fs::read_to_string(&got).unwrap(), "got 4\n");
    assert!(
        pane.capture(false)
            .await
            .unwrap()
            .contains("Do you want to proceed?"),
        "the dialog text stays on screen"
    );
    let bad = pane
        .send_input(vec![PaneInput::Keys {
            keys: vec![PaneKey::Ctrl { letter: "1".into() }],
        }])
        .await;
    assert!(matches!(bad, Err(AccError::InvalidArgument { .. })), "{bad:?}");

    pane.kill().await.unwrap();
    let exit = events_until(
        async || pane.next_events().await,
        |e| matches!(e, Event::Exited { .. }),
    )
    .await;
    assert!(matches!(exit.last(), Some(Event::Exited { .. })));
}
