use std::io::{Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use std::thread;
use std::time::Duration;

use yams_protocol::{
    Accepted, Completed, Message, ProtocolError, Rejected, Request, encode, receive_request,
    receive_response, send_message,
};
use yams_service::{
    ExecutionOutput, MAX_ACTIVE_REQUESTS, MAX_PENDING_ADMISSIONS, MAX_STREAM_BYTES,
    REQUEST_FRAME_DEADLINE, ServiceError, ShutdownToken, connect, serve_once, serve_until,
    validate_peer,
};

/// Smoke test proving `yams_service::validate_peer` still resolves through
/// the re-export of the shared `yams-protocol` peer validation primitive.
#[test]
fn peer_validation_accepts_the_current_process_peer() {
    let (left, right) = UnixStream::pair().unwrap();
    drop(right);
    validate_peer(&left).unwrap();
}

#[test]
fn service_exchange_acknowledges_and_completes_one_request() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let server = thread::spawn(move || {
        serve_once(&listener, Duration::from_secs(1), |request| {
            assert_eq!(request.argv, vec!["search", "needle"]);
            ExecutionOutput::new(0, "result\n", "")
        })
        .unwrap()
    });
    let result = connect(
        Path::new(&owned.path),
        Request::from_argv(vec!["search".into(), "needle".into()], String::from("/tmp"))
            .expect("service request is not --write"),
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(
        result,
        Completed {
            request_id: result.request_id.clone(),
            exit_code: 0,
            stdout: "result\n".into(),
            stderr: String::new(),
        }
    );
    server.join().unwrap();
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn completion_may_arrive_after_the_handshake_timeout() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        serve_once(&listener, Duration::from_secs(1), |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(2)).unwrap();
            ExecutionOutput::new(0, "delayed\n", "")
        })
    });
    let socket = owned.path.clone();
    let client = thread::spawn(move || {
        connect(
            &socket,
            Request::from_argv(vec!["search".into()], String::from("/tmp"))
                .expect("service request is not --write"),
            Duration::from_millis(100),
        )
    });

    entered_rx
        .recv_timeout(Duration::from_secs(1))
        .expect("Accepted was sent before handler entry");
    thread::sleep(Duration::from_millis(250));
    let finished_before_release = client.is_finished();
    release_tx.send(()).unwrap();
    let result = client.join().unwrap();
    server.join().unwrap().unwrap();
    yams_service::cleanup_owned_socket(&owned).unwrap();

    assert!(
        !finished_before_release,
        "completion wait remains live after the handshake timeout"
    );
    assert_eq!(result.unwrap().stdout, "delayed\n");
}

#[test]
fn fast_completion_survives_immediate_peer_close_repeatedly() {
    const EXCHANGES: usize = 32;

    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let server = thread::spawn(move || {
        for index in 0..EXCHANGES {
            let (mut stream, _) = listener.accept().unwrap();
            assert!(matches!(
                receive_request(&mut stream).unwrap(),
                Message::Request(_)
            ));
            let request_id = format!("fast-{index}");
            send_message(
                &mut stream,
                &Message::Accepted(Accepted {
                    request_id: request_id.clone(),
                }),
            )
            .unwrap();
            send_message(
                &mut stream,
                &Message::Completed(Completed {
                    request_id,
                    exit_code: 0,
                    stdout: "fast\n".into(),
                    stderr: String::new(),
                }),
            )
            .unwrap();
        }
    });

    for _ in 0..EXCHANGES {
        let completed = connect(&owned.path, request(), Duration::from_secs(1)).unwrap();
        assert_eq!(completed.stdout, "fast\n");
    }
    server.join().unwrap();
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

fn fast_raw_exchange(
    completion: Message,
    truncate_completion: bool,
) -> Result<Completed, ServiceError> {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        assert!(matches!(
            receive_request(&mut stream).unwrap(),
            Message::Request(_)
        ));
        send_message(
            &mut stream,
            &Message::Accepted(Accepted {
                request_id: "expected".into(),
            }),
        )
        .unwrap();
        if truncate_completion {
            let body = encode(&completion).unwrap();
            stream
                .write_all(&(body.len() as u32).to_be_bytes())
                .unwrap();
            stream.write_all(&body[..body.len() - 1]).unwrap();
        } else {
            send_message(&mut stream, &completion).unwrap();
        }
    });
    let result = connect(&owned.path, request(), Duration::from_secs(1));
    server.join().unwrap();
    yams_service::cleanup_owned_socket(&owned).unwrap();
    result
}

#[test]
fn fast_closed_completion_still_validates_request_id() {
    let result = fast_raw_exchange(
        Message::Completed(Completed {
            request_id: "wrong".into(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }),
        false,
    );
    assert!(matches!(result, Err(ServiceError::RequestIdMismatch)));
}

#[test]
fn fast_closed_truncated_completion_is_rejected() {
    let result = fast_raw_exchange(
        Message::Completed(Completed {
            request_id: "expected".into(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }),
        true,
    );
    assert!(matches!(
        result,
        Err(ServiceError::Protocol(ProtocolError::TruncatedFrame))
    ));
}

/// Serve one request whose handler returns the given output and return the
/// completion the client received.
fn exchange_with_output(stdout: String, stderr: String) -> Completed {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let server = thread::spawn(move || {
        serve_once(&listener, Duration::from_secs(5), move |_| {
            ExecutionOutput::new(0, stdout.clone(), stderr.clone())
        })
        .unwrap()
    });
    let result = connect(
        &owned.path,
        Request::from_argv(vec!["x".into()], String::from("/tmp"))
            .expect("service request is not --write"),
        Duration::from_secs(5),
    )
    .unwrap();
    server.join().unwrap();
    yams_service::cleanup_owned_socket(&owned).unwrap();
    result
}

/// Assert a completion is the exact oracle-pinned output-limit completion:
/// exit 4, empty stdout, and the fixed stderr message -- not merely a
/// matching exit code, so a regression that leaks partially-truncated output
/// on a passing stream cannot slip through.
fn assert_output_limit(completed: &Completed) {
    assert_eq!(
        (
            completed.exit_code,
            completed.stdout.as_str(),
            completed.stderr.as_str()
        ),
        (4, "", "yams: output limit\n")
    );
}

#[test]
fn either_stream_over_four_mib_returns_the_output_limit_completion() {
    for (out, err) in [
        ("x".repeat(MAX_STREAM_BYTES + 1), String::new()),
        (String::new(), "x".repeat(MAX_STREAM_BYTES + 1)),
    ] {
        assert_output_limit(&exchange_with_output(out, err));
    }
}

#[test]
fn multibyte_output_crossing_the_limit_never_panics_or_leaks() {
    for pad in 0..3usize {
        // '€' is 3 bytes: the limit falls on every byte inside the code point.
        let body = "x".repeat(MAX_STREAM_BYTES - 2 + pad) + "€";
        // Old code panicked in String::truncate; confirm both the panic-free
        // completion AND that no truncated remainder of `body` leaked out.
        assert_output_limit(&exchange_with_output(body, String::new()));
    }
}

#[test]
fn output_at_the_limit_is_preserved_when_the_frame_fits() {
    let completed = exchange_with_output("x".repeat(MAX_STREAM_BYTES), String::new());
    assert_eq!(completed.exit_code, 0);
    assert_eq!(completed.stdout.len(), MAX_STREAM_BYTES);
}

#[test]
fn encoded_frame_overflow_still_returns_the_output_limit_completion() {
    // Both streams sit exactly at their independent per-stream limit, so the
    // combined raw content is exactly MAX_RESPONSE_BYTES; JSON structural
    // overhead (field names, quotes, the request ID, exit code) then pushes
    // the strictly encoded frame past the protocol's response limit even
    // though neither stream individually exceeds MAX_STREAM_BYTES.
    let completed =
        exchange_with_output("x".repeat(MAX_STREAM_BYTES), "y".repeat(MAX_STREAM_BYTES));
    assert_output_limit(&completed);
}

#[test]
fn bounded_loop_drains_workers_after_cooperative_shutdown() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let stop = ShutdownToken::new();
    let caller_stop = stop.clone();
    let server = thread::spawn(move || {
        serve_until(listener, Duration::from_secs(1), None, stop, |_| {
            ExecutionOutput::new(0, "ok\n", "")
        })
        .unwrap()
    });
    let result = connect(
        &owned.path,
        Request::from_argv(vec!["search".into()], String::from("/tmp"))
            .expect("service request is not --write"),
        Duration::from_secs(1),
    )
    .unwrap();
    assert_eq!(result.stdout, "ok\n");
    caller_stop.request();
    let stats = server.join().unwrap();
    assert_eq!(stats.accepted, 1);
    assert_eq!(stats.completed, 1);
    assert!(!stats.workers_stuck);
    assert_eq!(MAX_ACTIVE_REQUESTS, 8);
    assert_eq!(MAX_PENDING_ADMISSIONS, 64);
    assert_eq!(REQUEST_FRAME_DEADLINE, Duration::from_secs(2));
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn worker_loop_never_executes_more_than_eight_requests_at_once() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let stop = ShutdownToken::new();
    let server_stop = stop.clone();
    let active = Arc::new(AtomicUsize::new(0));
    let maximum = Arc::new(AtomicUsize::new(0));
    let active_for_server = Arc::clone(&active);
    let maximum_for_server = Arc::clone(&maximum);
    let server = thread::spawn(move || {
        serve_until(
            listener,
            Duration::from_secs(2),
            None,
            server_stop,
            move |_| {
                let now = active_for_server.fetch_add(1, Ordering::AcqRel) + 1;
                maximum_for_server.fetch_max(now, Ordering::AcqRel);
                thread::sleep(Duration::from_millis(40));
                active_for_server.fetch_sub(1, Ordering::AcqRel);
                ExecutionOutput::new(0, "ok\n", "")
            },
        )
        .unwrap()
    });
    let callers = (0..72)
        .map(|_| {
            let socket = owned.path.clone();
            thread::spawn(move || {
                connect(
                    &socket,
                    Request::from_argv(vec!["search".into()], String::from("/tmp"))
                        .expect("service request is not --write"),
                    Duration::from_secs(2),
                )
            })
        })
        .collect::<Vec<_>>();
    for caller in callers {
        let _ = caller.join();
    }
    stop.request();
    let stats = server.join().unwrap();
    assert!(maximum.load(Ordering::Acquire) <= MAX_ACTIVE_REQUESTS);
    assert_eq!(stats.accepted, 72);
    assert_eq!(stats.completed + stats.rejected, stats.accepted);
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

fn request() -> Request {
    Request::from_argv(vec!["search".into()], String::from("/tmp"))
        .expect("service request is not --write")
}

fn send_request(stream: &mut UnixStream) {
    send_message(stream, &Message::Request(request())).unwrap();
}

fn request_frame() -> Vec<u8> {
    let body = encode(&Message::Request(request())).unwrap();
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    frame
}

fn connect_with_timeout(path: &Path, timeout: Duration) -> UnixStream {
    let stream = UnixStream::connect(path).unwrap();
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream
}

fn receive_with_timeout(stream: &mut UnixStream) -> Message {
    receive_response(stream).unwrap()
}

fn assert_rejected(message: Message, code: &str, detail: &str) {
    assert_eq!(
        message,
        Message::Rejected(Rejected {
            code: code.into(),
            message: detail.into(),
        })
    );
}

#[test]
fn ninth_complete_request_receives_the_pinned_busy_rejection() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let stop = ShutdownToken::new();
    let server_stop = stop.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let release_rx = Arc::new(std::sync::Mutex::new(release_rx));
    let server = thread::spawn(move || {
        serve_until(
            listener,
            Duration::from_secs(3),
            None,
            server_stop,
            move |_| {
                entered_tx.send(()).unwrap();
                release_rx
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(3))
                    .unwrap();
                ExecutionOutput::new(0, "ok\n", "")
            },
        )
        .unwrap()
    });

    let mut active_clients = Vec::new();
    for _ in 0..8 {
        let mut stream = connect_with_timeout(&owned.path, Duration::from_secs(2));
        send_request(&mut stream);
        active_clients.push(stream);
    }
    for _ in 0..8 {
        entered_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    }

    let mut ninth = connect_with_timeout(&owned.path, Duration::from_secs(2));
    send_request(&mut ninth);
    assert_rejected(
        receive_with_timeout(&mut ninth),
        "busy",
        "service has eight active requests",
    );

    for _ in 0..8 {
        release_tx.send(()).unwrap();
    }
    for client in &mut active_clients {
        assert!(matches!(receive_with_timeout(client), Message::Accepted(_)));
        assert!(matches!(
            receive_with_timeout(client),
            Message::Completed(_)
        ));
    }
    stop.request();
    let stats = server.join().unwrap();
    assert_eq!(stats.accepted, 9);
    assert_eq!(stats.rejected, 1);
    assert_eq!(stats.completed, 8);
    assert!(!stats.workers_stuck);
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn slow_peers_expire_after_admission_deadline_without_consuming_execution_slots() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let stop = ShutdownToken::new();
    let server_stop = stop.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        serve_until(
            listener,
            Duration::from_secs(3),
            None,
            server_stop,
            move |_| {
                entered_tx.send(()).unwrap();
                ExecutionOutput::new(0, "ok\n", "")
            },
        )
        .unwrap()
    });

    let mut slow_peers = Vec::new();
    let rejection_timeout = REQUEST_FRAME_DEADLINE + Duration::from_secs(1);
    for _ in 0..MAX_ACTIVE_REQUESTS {
        let mut stream = connect_with_timeout(&owned.path, rejection_timeout);
        stream.write_all(&[0]).unwrap();
        slow_peers.push(stream);
    }

    let mut complete = connect_with_timeout(&owned.path, Duration::from_secs(2));
    send_request(&mut complete);
    entered_rx
        .recv_timeout(Duration::from_millis(500))
        .expect("a complete request executes while partial frames remain pending");
    assert!(matches!(
        receive_with_timeout(&mut complete),
        Message::Accepted(_)
    ));
    assert!(matches!(
        receive_with_timeout(&mut complete),
        Message::Completed(_)
    ));

    for stream in &mut slow_peers {
        assert_rejected(
            receive_with_timeout(stream),
            "invalid_request",
            "request frame did not arrive before the timeout",
        );
    }
    stop.request();
    let stats = server.join().unwrap();
    assert_eq!(stats.accepted, MAX_ACTIVE_REQUESTS + 1);
    assert_eq!(stats.rejected, MAX_ACTIVE_REQUESTS);
    assert_eq!(stats.completed, 1);
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn malformed_request_is_rejected_without_becoming_a_completed_execution() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let stop = ShutdownToken::new();
    let server_stop = stop.clone();
    let server = thread::spawn(move || {
        serve_until(listener, Duration::from_secs(3), None, server_stop, |_| {
            panic!("malformed frames must never reach execution")
        })
        .unwrap()
    });

    let mut malformed = connect_with_timeout(&owned.path, Duration::from_secs(2));
    malformed.write_all(&2_u32.to_be_bytes()).unwrap();
    malformed.write_all(b"{}").unwrap();
    assert_rejected(
        receive_with_timeout(&mut malformed),
        "invalid_request",
        "request rejected",
    );

    stop.request();
    let stats = server.join().unwrap();
    assert_eq!(stats.accepted, 1);
    assert_eq!(stats.rejected, 1);
    assert_eq!(stats.completed, 0);
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

fn assert_remains_pending(stream: &mut UnixStream) {
    stream.set_nonblocking(true).unwrap();
    let mut byte = [0_u8; 1];
    let error = stream
        .read(&mut byte)
        .expect_err("admission remains pending without a response");
    assert!(matches!(
        error.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    ));
}

#[test]
fn complete_sixty_fifth_admission_executes_without_perturbing_pending_fifo() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let mut pending = Vec::new();
    for _ in 0..MAX_PENDING_ADMISSIONS {
        let mut stream = connect_with_timeout(&owned.path, Duration::from_secs(3));
        stream.write_all(&[0]).unwrap();
        pending.push(stream);
    }
    let mut complete = connect_with_timeout(&owned.path, Duration::from_secs(3));
    send_request(&mut complete);
    let stop = ShutdownToken::new();
    let server_stop = stop.clone();
    let (entered_tx, entered_rx) = mpsc::channel();
    let release = Arc::new(std::sync::Barrier::new(2));
    let server_release = Arc::clone(&release);
    let server = thread::spawn(move || {
        serve_until(
            listener,
            Duration::from_secs(3),
            None,
            server_stop,
            move |_| {
                entered_tx.send(()).unwrap();
                server_release.wait();
                ExecutionOutput::new(0, "ok\n", "")
            },
        )
        .unwrap()
    });

    entered_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("the complete request bypasses the full pending FIFO");
    assert!(matches!(
        receive_with_timeout(&mut complete),
        Message::Accepted(_)
    ));
    assert_remains_pending(&mut pending[0]);
    release.wait();
    assert!(matches!(
        receive_with_timeout(&mut complete),
        Message::Completed(_)
    ));

    stop.request();
    let stats = server.join().unwrap();
    assert_eq!(stats.accepted, MAX_PENDING_ADMISSIONS + 1);
    assert_eq!(stats.rejected, MAX_PENDING_ADMISSIONS);
    assert_eq!(stats.completed, 1);
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn pending_request_completing_after_shutdown_never_invokes_handler() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let stop = ShutdownToken::new();
    let server_stop = stop.clone();
    let (invoked_tx, invoked_rx) = mpsc::channel();
    let server = thread::spawn(move || {
        serve_until(
            listener,
            Duration::from_secs(3),
            None,
            server_stop,
            move |_| {
                invoked_tx.send(()).unwrap();
                ExecutionOutput::new(0, "unexpected\n", "")
            },
        )
        .unwrap()
    });

    let mut pending = Vec::new();
    for _ in 0..MAX_PENDING_ADMISSIONS {
        let mut stream = connect_with_timeout(&owned.path, Duration::from_secs(3));
        stream.write_all(&[0]).unwrap();
        pending.push(stream);
    }
    let frame = request_frame();
    let mut retained = connect_with_timeout(&owned.path, Duration::from_secs(3));
    retained.write_all(&frame[..1]).unwrap();
    assert_rejected(
        receive_with_timeout(&mut pending[0]),
        "busy",
        "service has too many unfinished requests",
    );

    stop.request();
    retained.write_all(&frame[1..]).unwrap();
    let stats = server.join().unwrap();
    assert!(matches!(
        invoked_rx.try_recv(),
        Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected)
    ));
    assert_eq!(stats.accepted, MAX_PENDING_ADMISSIONS + 1);
    assert_eq!(stats.rejected, MAX_PENDING_ADMISSIONS + 1);
    assert_eq!(stats.completed, 0);
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn malformed_sixty_fifth_admission_does_not_perturb_pending_fifo() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let mut pending = Vec::new();
    for _ in 0..MAX_PENDING_ADMISSIONS {
        let mut stream = connect_with_timeout(&owned.path, Duration::from_secs(3));
        stream.write_all(&[0]).unwrap();
        pending.push(stream);
    }
    let mut malformed = connect_with_timeout(&owned.path, Duration::from_secs(3));
    malformed.write_all(&2_u32.to_be_bytes()).unwrap();
    malformed.write_all(b"{}").unwrap();
    let stop = ShutdownToken::new();
    let server_stop = stop.clone();
    let server = thread::spawn(move || {
        serve_until(listener, Duration::from_secs(3), None, server_stop, |_| {
            panic!("malformed frames must never reach execution")
        })
        .unwrap()
    });

    assert_rejected(
        receive_with_timeout(&mut malformed),
        "invalid_request",
        "request rejected",
    );
    assert_remains_pending(&mut pending[0]);

    stop.request();
    let stats = server.join().unwrap();
    assert_eq!(stats.accepted, MAX_PENDING_ADMISSIONS + 1);
    assert_eq!(stats.rejected, MAX_PENDING_ADMISSIONS + 1);
    assert_eq!(stats.completed, 0);
    yams_service::cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn pending_overflow_rejects_the_oldest_admission_with_the_pinned_message() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.path().canonicalize().unwrap().join("service.sock");
    let (listener, owned) = yams_service::bind_listener(&path).unwrap();
    let stop = ShutdownToken::new();
    let server_stop = stop.clone();
    let server = thread::spawn(move || {
        serve_until(listener, Duration::from_secs(3), None, server_stop, |_| {
            panic!("partial frames must never reach execution")
        })
        .unwrap()
    });

    let mut pending = Vec::new();
    for marker in 0..64_u8 {
        let mut stream = connect_with_timeout(&owned.path, Duration::from_secs(3));
        stream.write_all(&[marker]).unwrap();
        pending.push(stream);
    }
    let mut newest = connect_with_timeout(&owned.path, Duration::from_secs(3));
    newest.write_all(&[255]).unwrap();

    assert_rejected(
        receive_with_timeout(&mut pending[0]),
        "busy",
        "service has too many unfinished requests",
    );
    assert_remains_pending(&mut newest);

    stop.request();
    let stats = server.join().unwrap();
    assert_eq!(stats.accepted, 65);
    assert_eq!(stats.rejected, 65);
    assert_eq!(stats.completed, 0);
    yams_service::cleanup_owned_socket(&owned).unwrap();
}
