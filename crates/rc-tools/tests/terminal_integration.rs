#![cfg(unix)]

use std::thread;
use std::time::{Duration, Instant};

use rc_tools::terminal::{
    TerminalClientEngine, TerminalEvent, TerminalHostEngine, MAX_CONCURRENT_TERMINALS,
    MAX_TERMINAL_WIRE_FRAME_BYTES,
};

fn pump_once(
    client: &mut TerminalClientEngine,
    host: &mut TerminalHostEngine,
) -> Vec<TerminalEvent> {
    let mut events = Vec::new();
    for f in client.poll_outgoing(32) {
        assert!(f.len() <= MAX_TERMINAL_WIRE_FRAME_BYTES);
        host.handle_incoming(&f);
    }
    for f in host.poll_outgoing(32) {
        assert!(f.len() <= MAX_TERMINAL_WIRE_FRAME_BYTES);
        events.extend(client.handle_incoming(&f));
    }
    events
}

#[test]
fn real_unix_pty_echo_resize_and_exit_zero() {
    let mut client = TerminalClientEngine::new();
    let mut host = TerminalHostEngine::new();

    let term_id = client.open(80, 24);
    let mut opened = false;
    let mut output_acc = String::new();
    let mut exit_code = None;
    let mut sent_cmd = false;

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline && exit_code.is_none() {
        for ev in pump_once(&mut client, &mut host) {
            match ev {
                TerminalEvent::Opened {
                    term_id: tid,
                    ok,
                    title,
                    cwd,
                    error,
                } => {
                    assert_eq!(tid, term_id);
                    assert!(ok, "pty open failed: {error:?}");
                    assert!(!title.is_empty());
                    assert!(!cwd.is_empty());
                    opened = true;
                    if !sent_cmd {
                        client.resize(term_id, 120, 40);
                        client.send_input(term_id, b"echo RC_PTY_HELLO && exit 0\n");
                        sent_cmd = true;
                    }
                }
                TerminalEvent::Output {
                    term_id: tid,
                    text_utf8_lossy,
                    ..
                } => {
                    assert_eq!(tid, term_id);
                    output_acc.push_str(&text_utf8_lossy);
                }
                TerminalEvent::Exited { term_id: tid, code } => {
                    assert_eq!(tid, term_id);
                    exit_code = Some(code);
                }
            }
        }
        if exit_code.is_none() {
            thread::sleep(Duration::from_millis(10));
        }
    }

    assert!(opened, "expected Opened event");
    assert_eq!(
        exit_code,
        Some(0),
        "expected Exited(0), got output: {output_acc:?}"
    );
    assert!(
        output_acc.contains("RC_PTY_HELLO"),
        "expected RC_PTY_HELLO in terminal output, got: {output_acc:?}"
    );
    assert_eq!(host.active_count(), 0);
}

#[test]
fn terminal_host_enforces_max_concurrent_ptys_and_clean_drop() {
    let mut client = TerminalClientEngine::new();
    let mut host = TerminalHostEngine::new();

    let mut opened_ok = 0usize;
    let mut opened_err = 0usize;

    for _ in 0..=MAX_CONCURRENT_TERMINALS {
        client.open(80, 24);
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && (opened_ok + opened_err) <= MAX_CONCURRENT_TERMINALS {
        for ev in pump_once(&mut client, &mut host) {
            if let TerminalEvent::Opened { ok, .. } = ev {
                if ok {
                    opened_ok += 1;
                } else {
                    opened_err += 1;
                }
            }
        }
        thread::sleep(Duration::from_millis(10));
    }

    assert_eq!(opened_ok, MAX_CONCURRENT_TERMINALS);
    assert_eq!(opened_err, 1);
    assert_eq!(host.active_count(), MAX_CONCURRENT_TERMINALS);

    // Dropping TerminalHostEngine must terminate all 4 live shells promptly.
    let drop_start = Instant::now();
    drop(host);
    assert!(
        drop_start.elapsed() < Duration::from_secs(3),
        "TerminalHostEngine::drop took too long"
    );
}
