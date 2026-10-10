//! Terminals in shims, started through this package's own binary as the
//! runner starts them: a shell outlives the session that started it, a new
//! session finds it again with its screen, and the replay a shim keeps is
//! bounded.
#![cfg(unix)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use zeughaus_mux::{Dimensions, TerminalCommand, TerminalHead, TerminalId};
use zeughaus_terminal::shim::ShimConn;
use zeughaus_terminal::shim::proto::{REPLAY_BYTES, ToShim};
use zeughaus_terminal::wal;
use zeughaus_terminal::wal::Wal;
use zeughaus_terminal::{Profile, Session, ShimHost, TerminalHost};

const SIZE: Dimensions = Dimensions { cols: 80, rows: 24 };

fn host(name: &str) -> ShimHost {
    let root = std::env::temp_dir().join(format!("zh-shim-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    ShimHost {
        program: PathBuf::from(env!("CARGO_BIN_EXE_zeughaus-runner")),
        args: vec!["shim".into()],
        root,
    }
}

fn sh(args: &[&str]) -> Profile {
    Profile {
        label: "sh".into(),
        program: Some("/bin/sh".into()),
        args: args.iter().map(|a| a.to_string()).collect(),
        cwd: None,
        env: vec![("PS1".into(), "$ ".into())],
        scrollback_rows: 64,
    }
}

fn text_of(head: &TerminalHead) -> String {
    head.rows
        .iter()
        .flat_map(|row| row.spans.iter().map(|span| span.text.as_str()))
        .collect()
}

/// Polls `check` until it answers, or gives up after `within`.
fn wait_for<T>(within: Duration, mut check: impl FnMut() -> Option<T>) -> Option<T> {
    let deadline = Instant::now() + within;
    loop {
        if let Some(found) = check() {
            return Some(found);
        }
        if Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The digits after `marker-` in the screen, once a whole number is there.
fn marker_pid(session: &Session) -> Option<String> {
    let text = text_of(&session.head(64));
    // The echoed command line holds `marker-$$`; the output holds digits.
    text.match_indices("marker-").find_map(|(at, m)| {
        let digits: String = text[at + m.len()..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect();
        (!digits.is_empty()).then_some(digits)
    })
}

#[test]
fn a_shell_outlives_its_session_and_is_found_again() {
    let host = host("reattach");
    let id = TerminalId(3);
    let session = Session::spawn(
        id,
        &sh(&[]),
        SIZE,
        Wal::Off,
        &TerminalHost::Shim(host.clone()),
    )
    .expect("start a shell in a shim");
    session
        .apply(&TerminalCommand::Text {
            serial: 1,
            text: "echo marker-$$\n".into(),
        })
        .expect("type into the shell");
    let pid = wait_for(Duration::from_secs(10), || marker_pid(&session))
        .unwrap_or_else(|| panic!("no marker: {:?}", text_of(&session.head(64))));

    // The runner that held the session is gone.
    drop(session);

    let session = Session::reattach(id, &host, "sh", 64, SIZE).expect("reattach to the shim");
    assert_eq!(marker_pid(&session).as_deref(), Some(pid.as_str()));
    assert_eq!(session.exit(), None);
    let alive = std::process::Command::new("kill")
        .args(["-0", &pid])
        .status()
        .expect("run kill");
    assert!(alive.success(), "the shell {pid} did not survive");

    // Still a working terminal, not a recording.
    session
        .apply(&TerminalCommand::Text {
            serial: 2,
            text: "echo again-$((40+2))\n".into(),
        })
        .expect("type after reattaching");
    assert!(
        wait_for(Duration::from_secs(10), || text_of(&session.head(64))
            .contains("again-42")
            .then_some(()))
        .is_some(),
        "no answer after reattaching: {:?}",
        text_of(&session.head(64))
    );

    let dir = host.dir(id);
    session.kill();
    assert!(
        wait_for(Duration::from_secs(2), || (!dir.exists()).then_some(())).is_some(),
        "{} outlived the close",
        dir.display()
    );
    let _ = std::fs::remove_dir_all(&host.root);
}

#[test]
fn a_shim_replays_at_most_its_ring_and_always_the_newest_bytes() {
    let host = host("ring");
    let id = TerminalId(4);
    let script = "head -c 5000000 /dev/zero | tr '\\0' a; printf END-OF-FLOOD; sleep 30";
    let session = Session::spawn(
        id,
        &sh(&["-c", script]),
        SIZE,
        Wal::Capped,
        &TerminalHost::Shim(host.clone()),
    )
    .expect("start the flood in a shim");
    assert!(
        wait_for(Duration::from_secs(30), || text_of(&session.head(0))
            .contains("END-OF-FLOOD")
            .then_some(()))
        .is_some(),
        "the flood never ended"
    );

    let (conn, welcome, replay) = ShimConn::connect(&host.dir(id)).expect("connect");
    assert_eq!(welcome.exit, None);
    assert!(
        replay.len() <= REPLAY_BYTES,
        "replay of {} bytes",
        replay.len()
    );
    assert!(
        replay.len() > REPLAY_BYTES / 2,
        "replay of {} bytes",
        replay.len()
    );
    assert!(replay.ends_with(b"END-OF-FLOOD"));

    // The shim's own WAL holds the whole stream, not just the ring, with
    // times that never run backwards.
    let records = wal::read(&host.dir(id).join("wal")).expect("the shim's WAL");
    let stream: Vec<u8> = records
        .iter()
        .flat_map(|r| r.bytes.iter().copied())
        .collect();
    assert_eq!(stream.len(), 5_000_000 + "END-OF-FLOOD".len());
    assert!(stream.ends_with(b"END-OF-FLOOD"));
    assert!(records.windows(2).all(|w| w[0].at_micros <= w[1].at_micros));

    conn.send(&ToShim::Close).expect("close");
    let dir = host.dir(id);
    assert!(wait_for(Duration::from_secs(2), || (!dir.exists()).then_some(())).is_some());
    drop(session);
    let _ = std::fs::remove_dir_all(&host.root);
}
