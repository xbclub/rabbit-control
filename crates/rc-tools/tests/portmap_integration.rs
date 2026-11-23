use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use rc_tools::portmap::{
    PortMapClientEngine, PortMapHostEngine, PortMapStatus, MAX_PORTMAP_WIRE_FRAME_BYTES,
};

fn reserve_free_port() -> u16 {
    let l = TcpListener::bind("127.0.0.1:0").expect("bind ephemeral port");
    l.local_addr().unwrap().port()
}

fn spawn_echo_server() -> (u16, Arc<AtomicBool>, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind echo server");
    let port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(true).unwrap();
    let stop = Arc::new(AtomicBool::new(false));
    let s_stop = Arc::clone(&stop);

    let handle = thread::spawn(move || {
        let mut workers = Vec::new();
        while !s_stop.load(Ordering::Acquire) {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    let _ = stream.set_nonblocking(false);
                    let _ = stream.set_nodelay(true);
                    let _ = stream.set_read_timeout(Some(Duration::from_millis(50)));
                    let w_stop = Arc::clone(&s_stop);
                    workers.push(thread::spawn(move || {
                        let mut buf = [0u8; 16 * 1024];
                        while !w_stop.load(Ordering::Acquire) {
                            match stream.read(&mut buf) {
                                Ok(0) => break,
                                Ok(n) => {
                                    if stream.write_all(&buf[..n]).is_err() {
                                        break;
                                    }
                                }
                                Err(e)
                                    if matches!(
                                        e.kind(),
                                        ErrorKind::WouldBlock
                                            | ErrorKind::TimedOut
                                            | ErrorKind::Interrupted
                                    ) => {}
                                Err(_) => break,
                            }
                        }
                    }));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(_) => break,
            }
        }
        for w in workers {
            let _ = w.join();
        }
    });

    (port, stop, handle)
}

fn pump_step(client: &mut PortMapClientEngine, host: &mut PortMapHostEngine) -> bool {
    let mut activity = false;
    for f in client.poll_outgoing(64) {
        assert!(
            f.len() <= MAX_PORTMAP_WIRE_FRAME_BYTES,
            "client portmap frame exceeded 64 KiB: {}",
            f.len()
        );
        host.handle_incoming(&f);
        activity = true;
    }
    for f in host.poll_outgoing(64) {
        assert!(
            f.len() <= MAX_PORTMAP_WIRE_FRAME_BYTES,
            "host portmap frame exceeded 64 KiB: {}",
            f.len()
        );
        let _ = client.handle_incoming(&f);
        activity = true;
    }
    activity
}

#[test]
fn portmap_echo_128kib_stats_and_toggle_delete_teardown() {
    let (echo_port, echo_stop, echo_handle) = spawn_echo_server();
    let local_port = reserve_free_port();

    let mut client = PortMapClientEngine::new();
    let mut host = PortMapHostEngine::new();

    let snap = client
        .add_rule(
            "echo-test".to_string(),
            local_port,
            "127.0.0.1".to_string(),
            echo_port,
        )
        .unwrap();
    assert_eq!(snap.status, PortMapStatus::Ok);
    assert!(snap.enabled);
    assert_eq!(snap.error, None);
    let rule_id = snap.id;

    // 128 KiB deterministic payload
    let payload: Vec<u8> = (0..(128 * 1024))
        .map(|i| ((i * 31 + 17) & 0xFF) as u8)
        .collect();
    let payload_for_sender = payload.clone();

    // Connect a real TCP client in a worker thread to perform full-duplex write + read_exact
    let io_done = Arc::new(AtomicBool::new(false));
    let io_done_flag = Arc::clone(&io_done);
    let tcp_client_handle = thread::spawn(move || {
        let mut stream =
            TcpStream::connect(("127.0.0.1", local_port)).expect("connect mapped port");
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let mut writer = stream.try_clone().unwrap();
        let writer_handle = thread::spawn(move || {
            writer
                .write_all(&payload_for_sender)
                .expect("write 128 KiB");
            writer.flush().unwrap();
        });

        let mut echoed = vec![0u8; payload.len()];
        stream.read_exact(&mut echoed).expect("read 128 KiB echo");
        writer_handle.join().unwrap();
        assert_eq!(echoed, payload, "echoed 128 KiB payload mismatch");
        drop(stream);
        io_done_flag.store(true, Ordering::Release);
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let moved = pump_step(&mut client, &mut host);
        if io_done.load(Ordering::Acquire) {
            let rules = client.list_rules();
            if rules[0].active_conns == 0 && host.active_conn_count() == 0 {
                break;
            }
        }
        if !moved {
            thread::sleep(Duration::from_millis(2));
        }
    }

    tcp_client_handle
        .join()
        .expect("tcp client thread panicked");
    assert!(io_done.load(Ordering::Acquire));

    let rules = client.list_rules();
    assert_eq!(rules.len(), 1);
    assert_eq!(rules[0].status, PortMapStatus::Ok);
    assert_eq!(rules[0].bytes_tx, 128 * 1024);
    assert_eq!(rules[0].bytes_rx, 128 * 1024);
    assert_eq!(rules[0].active_conns, 0);
    assert_eq!(host.active_conn_count(), 0);

    // Disable rule -> local listener must stop immediately
    let disabled = client.set_rule_enabled(rule_id, false).unwrap();
    assert!(!disabled.enabled);
    assert_eq!(disabled.status, PortMapStatus::Pending);
    assert!(
        TcpStream::connect_timeout(
            &format!("127.0.0.1:{local_port}").parse().unwrap(),
            Duration::from_millis(200),
        )
        .is_err(),
        "disabled rule must refuse connections"
    );

    // Re-enable rule -> listener binds cleanly again
    let reenabled = client.set_rule_enabled(rule_id, true).unwrap();
    assert!(reenabled.enabled);
    assert_eq!(reenabled.status, PortMapStatus::Ok);
    let probe = TcpStream::connect(("127.0.0.1", local_port)).expect("re-enabled port binds");
    drop(probe);

    // Pump briefly then delete rule
    let _ = pump_step(&mut client, &mut host);
    client.delete_rule(rule_id).unwrap();
    assert!(client.list_rules().is_empty());
    let _ = pump_step(&mut client, &mut host);

    assert!(
        TcpStream::connect_timeout(
            &format!("127.0.0.1:{local_port}").parse().unwrap(),
            Duration::from_millis(200),
        )
        .is_err(),
        "deleted rule must release local port"
    );

    drop(client);
    drop(host);
    echo_stop.store(true, Ordering::Release);
    let _ = echo_handle.join();
}

#[test]
fn portmap_reports_target_connect_error_cleanly() {
    let local_port = reserve_free_port();
    let unused_target_port = reserve_free_port();

    let mut client = PortMapClientEngine::new();
    let mut host = PortMapHostEngine::new();

    let snap = client
        .add_rule(
            "dead-target".to_string(),
            local_port,
            "127.0.0.1".to_string(),
            unused_target_port,
        )
        .unwrap();
    assert_eq!(snap.status, PortMapStatus::Ok);

    let stream = TcpStream::connect(("127.0.0.1", local_port)).expect("connect local listener");

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut saw_error = false;
    while Instant::now() < deadline {
        let _ = pump_step(&mut client, &mut host);
        let rules = client.list_rules();
        if rules[0].error.is_some() && rules[0].active_conns == 0 {
            saw_error = true;
            break;
        }
        thread::sleep(Duration::from_millis(10));
    }

    drop(stream);
    assert!(saw_error, "expected ConnError to populate rule.error");
    assert_eq!(host.active_conn_count(), 0);
}

#[test]
fn portmap_tcp_half_close_delivers_full_response_after_shutdown_write() {
    // Target server reads until client sends TCP FIN (EOF), then responds with a 64 KiB payload.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind half-close server");
    let target_port = listener.local_addr().unwrap().port();
    let expected_req = b"GET /half-close HTTP/1.0\r\nHost: test\r\n\r\n".to_vec();
    let expected_resp: Vec<u8> = (0..(64 * 1024))
        .map(|i| ((i * 19 + 7) & 0xFF) as u8)
        .collect();
    let req_for_server = expected_req.clone();
    let resp_for_server = expected_resp.clone();

    let server_handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept target");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let mut got = Vec::new();
        stream.read_to_end(&mut got).expect("server read_to_end");
        assert_eq!(got, req_for_server, "server received truncated request");
        stream
            .write_all(&resp_for_server)
            .expect("server write full response");
        let _ = stream.shutdown(std::net::Shutdown::Write);
    });

    let local_port = reserve_free_port();
    let mut client = PortMapClientEngine::new();
    let mut host = PortMapHostEngine::new();

    let snap = client
        .add_rule(
            "half-close".to_string(),
            local_port,
            "127.0.0.1".to_string(),
            target_port,
        )
        .unwrap();
    assert_eq!(snap.status, PortMapStatus::Ok);

    let done = Arc::new(AtomicBool::new(false));
    let done_flag = Arc::clone(&done);
    let resp_expected = expected_resp.clone();
    let client_handle = thread::spawn(move || {
        let mut stream =
            TcpStream::connect(("127.0.0.1", local_port)).expect("connect mapped port");
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(&expected_req).expect("write request");
        // Half-close write direction immediately after sending request!
        stream
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown write");

        let mut received = Vec::new();
        stream
            .read_to_end(&mut received)
            .expect("read full response after half-close");
        assert_eq!(
            received, resp_expected,
            "client response after half-close mismatch"
        );
        done_flag.store(true, Ordering::Release);
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let moved = pump_step(&mut client, &mut host);
        if done.load(Ordering::Acquire) {
            let rules = client.list_rules();
            if rules[0].active_conns == 0 && host.active_conn_count() == 0 {
                break;
            }
        }
        if !moved {
            thread::sleep(Duration::from_millis(2));
        }
    }

    client_handle.join().expect("tcp client thread panicked");
    server_handle.join().expect("server thread panicked");
    assert!(done.load(Ordering::Acquire));
    assert_eq!(client.list_rules()[0].active_conns, 0);
    assert_eq!(host.active_conn_count(), 0);
}

#[test]
fn portmap_write_then_immediate_close_loses_zero_tail_bytes() {
    // Target server drains everything until EOF and verifies 100% of bytes arrived.
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind sink server");
    let target_port = listener.local_addr().unwrap().port();
    let payload: Vec<u8> = (0..(192 * 1024 + 333))
        .map(|i| ((i * 53 + 11) & 0xFF) as u8)
        .collect();
    let expected = payload.clone();

    let received_ok = Arc::new(AtomicBool::new(false));
    let received_ok_flag = Arc::clone(&received_ok);
    let server_handle = thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept sink");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
        let mut got = Vec::new();
        stream.read_to_end(&mut got).expect("sink read_to_end");
        assert_eq!(
            got.len(),
            expected.len(),
            "lost tail bytes on write-then-immediate-close"
        );
        assert_eq!(got, expected);
        received_ok_flag.store(true, Ordering::Release);
    });

    let local_port = reserve_free_port();
    let mut client = PortMapClientEngine::new();
    let mut host = PortMapHostEngine::new();

    client
        .add_rule(
            "tail-zero-loss".to_string(),
            local_port,
            "127.0.0.1".to_string(),
            target_port,
        )
        .unwrap();

    let writer_handle = thread::spawn(move || {
        let mut stream =
            TcpStream::connect(("127.0.0.1", local_port)).expect("connect mapped port");
        stream.set_nodelay(true).unwrap();
        stream.write_all(&payload).expect("write payload");
        // Immediately drop (close) without waiting for any response!
        drop(stream);
    });

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        let moved = pump_step(&mut client, &mut host);
        if received_ok.load(Ordering::Acquire)
            && client.list_rules()[0].active_conns == 0
            && host.active_conn_count() == 0
        {
            break;
        }
        if !moved {
            thread::sleep(Duration::from_millis(2));
        }
    }

    writer_handle.join().expect("writer thread panicked");
    server_handle.join().expect("sink server thread panicked");
    assert!(received_ok.load(Ordering::Acquire));
    assert_eq!(client.list_rules()[0].active_conns, 0);
    assert_eq!(host.active_conn_count(), 0);
}

#[test]
fn portmap_window_flow_control_sustains_over_3mib_transfer() {
    // Exceeds INITIAL_CONN_WINDOW_BYTES (2 MiB) to exercise ConnWindow replenishment end-to-end.
    let (echo_port, echo_stop, echo_handle) = spawn_echo_server();
    let local_port = reserve_free_port();

    let mut client = PortMapClientEngine::new();
    let mut host = PortMapHostEngine::new();
    client
        .add_rule(
            "window-3m".to_string(),
            local_port,
            "127.0.0.1".to_string(),
            echo_port,
        )
        .unwrap();

    let payload: Vec<u8> = (0..(3 * 1024 * 1024 + 123))
        .map(|i| ((i * 29 + 3) & 0xFF) as u8)
        .collect();
    let payload_send = payload.clone();

    let io_done = Arc::new(AtomicBool::new(false));
    let io_done_flag = Arc::clone(&io_done);
    let tcp_handle = thread::spawn(move || {
        let mut stream = TcpStream::connect(("127.0.0.1", local_port)).unwrap();
        stream.set_nodelay(true).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(15)))
            .unwrap();
        let mut writer = stream.try_clone().unwrap();
        let w = thread::spawn(move || {
            writer.write_all(&payload_send).unwrap();
            let _ = writer.shutdown(std::net::Shutdown::Write);
        });
        let mut echoed = Vec::with_capacity(payload.len());
        stream.read_to_end(&mut echoed).unwrap();
        w.join().unwrap();
        assert_eq!(echoed.len(), payload.len());
        assert_eq!(echoed, payload);
        io_done_flag.store(true, Ordering::Release);
    });

    let deadline = Instant::now() + Duration::from_secs(15);
    while Instant::now() < deadline {
        let moved = pump_step(&mut client, &mut host);
        if io_done.load(Ordering::Acquire)
            && client.list_rules()[0].active_conns == 0
            && host.active_conn_count() == 0
        {
            break;
        }
        if !moved {
            thread::sleep(Duration::from_millis(1));
        }
    }

    tcp_handle.join().unwrap();
    assert!(io_done.load(Ordering::Acquire));
    assert_eq!(client.list_rules()[0].bytes_tx, 3 * 1024 * 1024 + 123);
    assert_eq!(client.list_rules()[0].bytes_rx, 3 * 1024 * 1024 + 123);

    drop(client);
    drop(host);
    echo_stop.store(true, Ordering::Release);
    let _ = echo_handle.join();
}
