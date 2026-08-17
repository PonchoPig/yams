#![cfg(feature = "test-support")]

use std::ffi::OsString;
use std::io::{Read, Write};
use std::os::unix::ffi::OsStringExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use yams_cli::client::{ConnectOutcome, Connector, try_service, try_service_with};
use yams_cli::{DirectOperation, DirectRequest, ResolvedDirsOverride, RuntimeLayout};
use yams_protocol::peer::{PeerCredentialProvider, PeerError, SystemPeerCredentials};
use yams_protocol::{
    Accepted, Completed, Message, OperationKind, Rejected, Request, encode, receive_request,
    send_message,
};

fn search_request(query: &str) -> DirectRequest {
    DirectRequest {
        operation: DirectOperation::Search,
        project: None,
        query: Some(query.to_owned()),
        k: 5,
        requested_k: "5".into(),
        json: false,
        full: false,
        no_gate: false,
        explain: false,
        min_score: None,
        max_gap: None,
    }
}

fn index_request(project: &Path) -> DirectRequest {
    DirectRequest {
        operation: DirectOperation::Index,
        project: Some(project.to_path_buf()),
        query: None,
        k: 5,
        requested_k: "5".into(),
        json: false,
        full: false,
        no_gate: false,
        explain: false,
        min_score: None,
        max_gap: None,
    }
}

fn layout(cwd: PathBuf, socket: PathBuf) -> RuntimeLayout {
    let base = socket.parent().unwrap().join("state");
    let store = base.join("rust-v1");
    RuntimeLayout {
        cwd,
        application_support_dir: base.clone(),
        query_log: base.join("queries.jsonl"),
        cache_dir: base.clone(),
        store_dir: store.clone(),
        indexes_dir: store.join("indexes"),
        vectors_path: store.join("vectors.sqlite3"),
        model_cache_dir: store.join("models"),
        model_lock_dir: store.join("locks"),
        runtime_dir: base,
        service_socket: socket,
        corpus_dirs: ResolvedDirsOverride::Absent,
    }
}

fn service<F>(handler: F) -> (tempfile::TempDir, RuntimeLayout, thread::JoinHandle<()>)
where
    F: FnOnce(UnixStream) + Send + 'static,
{
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let socket = temporary.path().join("service.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let handle = thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        handler(stream);
    });
    let runtime = layout(project.canonicalize().unwrap(), socket);
    (temporary, runtime, handle)
}

fn accept_and_complete(mut stream: UnixStream, request_id: &str, output: &str) -> Request {
    let request = match receive_request(&mut stream).unwrap() {
        Message::Request(request) => request,
        other => panic!("expected request, got {other:?}"),
    };
    send_message(
        &mut stream,
        &Message::Accepted(Accepted {
            request_id: request_id.into(),
        }),
    )
    .unwrap();
    send_message(
        &mut stream,
        &Message::Completed(Completed {
            request_id: request_id.into(),
            exit_code: 0,
            stdout: output.into(),
            stderr: String::new(),
        }),
    )
    .unwrap();
    request
}

#[test]
fn client_waits_past_two_seconds_after_accepted() {
    let (_temporary, runtime, handle) = service(|mut stream| {
        let request = receive_request(&mut stream).unwrap();
        assert!(matches!(request, Message::Request(_)));
        let request_id = "aa".repeat(8);
        send_message(
            &mut stream,
            &Message::Accepted(Accepted {
                request_id: request_id.clone(),
            }),
        )
        .unwrap();
        thread::sleep(Duration::from_millis(2600));
        send_message(
            &mut stream,
            &Message::Completed(Completed {
                request_id,
                exit_code: 0,
                stdout: "late but valid\n".into(),
                stderr: String::new(),
            }),
        )
        .unwrap();
    });

    let completion = try_service(&search_request("alpha"), &runtime)
        .unwrap()
        .unwrap();
    assert_eq!(completion.stdout, "late but valid\n");
    handle.join().unwrap();
}

#[test]
fn immediate_completion_survives_peer_close_repeatedly() {
    const EXCHANGES: usize = 64;

    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let socket = temporary.path().join("service.sock");
    let listener = UnixListener::bind(&socket).unwrap();
    let runtime = layout(project.canonicalize().unwrap(), socket);
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
                    stdout: format!("fast-{index}\n"),
                    stderr: String::new(),
                }),
            )
            .unwrap();
        }
    });

    for index in 0..EXCHANGES {
        let completion = try_service(&search_request("alpha"), &runtime)
            .unwrap()
            .unwrap();
        assert_eq!(completion.stdout, format!("fast-{index}\n"));
    }
    server.join().unwrap();
}

#[test]
fn option_shaped_queries_round_trip_as_one_positional_after_a_separator() {
    for query in [
        "--gc",
        "--json -k 999",
        "alpha  double  space",
        "füß --index",
    ] {
        let (sender, receiver) = mpsc::channel();
        let (_temporary, runtime, handle) = service(move |stream| {
            let request = accept_and_complete(stream, "request-id", "");
            sender.send(request.argv).unwrap();
        });
        let completion = try_service(&search_request(query), &runtime)
            .unwrap()
            .unwrap();
        assert_eq!(completion.exit_code, yams_core::ExitCode::Ok);
        let argv = receiver.recv().unwrap();
        let separator = argv
            .iter()
            .position(|argument| argument == "--")
            .expect("-- always precedes the query");
        assert_eq!(argv[separator + 1], query, "exact owned query");
        assert_eq!(argv.len(), separator + 2, "nothing after the query");
        handle.join().unwrap();
    }
}

struct OneShotConnector(Mutex<Option<UnixStream>>);

impl Connector for OneShotConnector {
    fn connect(&self, _: &Path, _: Instant) -> ConnectOutcome {
        ConnectOutcome::Connected(self.0.lock().unwrap().take().unwrap())
    }
}

struct DelayedOneShotConnector {
    stream: Mutex<Option<UnixStream>>,
    delay: Duration,
}

struct ExpiredConnectedConnector(Mutex<Option<UnixStream>>);

impl Connector for ExpiredConnectedConnector {
    fn connect(&self, _: &Path, deadline: Instant) -> ConnectOutcome {
        thread::sleep(
            deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(5),
        );
        ConnectOutcome::Connected(self.0.lock().unwrap().take().unwrap())
    }
}

impl Connector for DelayedOneShotConnector {
    fn connect(&self, _: &Path, _: Instant) -> ConnectOutcome {
        thread::sleep(self.delay);
        ConnectOutcome::Connected(self.stream.lock().unwrap().take().unwrap())
    }
}

struct MismatchedPeer;

impl PeerCredentialProvider for MismatchedPeer {
    fn peer_uid(&self, _: &UnixStream) -> Result<u32, PeerError> {
        Ok((rustix::process::geteuid().as_raw() as u32) ^ 1)
    }
}

#[test]
fn uid_mismatch_fails_closed_before_any_request_bytes() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let connector = OneShotConnector(Mutex::new(Some(client)));
    let runtime = layout(PathBuf::from("/tmp"), PathBuf::from("/unused"));

    let result = try_service_with(
        &search_request("alpha"),
        &runtime,
        &connector,
        &MismatchedPeer,
    );

    assert!(matches!(result, Some(Err(_))), "never direct fallback");
    let mut byte = [0_u8; 1];
    assert_eq!(server.read(&mut byte).unwrap(), 0, "no request bytes");
}

#[test]
fn delayed_connect_does_not_restore_timeout_after_fast_acceptance() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let connector = DelayedOneShotConnector {
        stream: Mutex::new(Some(client)),
        delay: Duration::from_millis(20),
    };
    let runtime = layout(PathBuf::from("/tmp"), PathBuf::from("/unused"));
    let handle = thread::spawn(move || {
        assert!(matches!(
            receive_request(&mut server).unwrap(),
            Message::Request(_)
        ));
        send_message(
            &mut server,
            &Message::Accepted(Accepted {
                request_id: "delayed".into(),
            }),
        )
        .unwrap();
        send_message(
            &mut server,
            &Message::Completed(Completed {
                request_id: "delayed".into(),
                exit_code: 0,
                stdout: "complete\n".into(),
                stderr: String::new(),
            }),
        )
        .unwrap();
    });

    let completion = try_service_with(
        &search_request("alpha"),
        &runtime,
        &connector,
        &SystemPeerCredentials,
    )
    .unwrap()
    .unwrap();

    assert_eq!(completion.stdout, "complete\n");
    handle.join().unwrap();
}

#[test]
fn admission_deadline_before_request_delivery_falls_back() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let connector = ExpiredConnectedConnector(Mutex::new(Some(client)));
    let runtime = layout(PathBuf::from("/tmp"), PathBuf::from("/unused"));

    let result = try_service_with(
        &search_request("alpha"),
        &runtime,
        &connector,
        &SystemPeerCredentials,
    );

    assert!(result.is_none(), "an undelivered request may run directly");
    let mut byte = [0_u8; 1];
    assert_eq!(server.read(&mut byte).unwrap(), 0, "no request bytes");
}

#[test]
fn post_connect_encoding_failures_are_operational_not_absence() {
    let (client, mut server) = UnixStream::pair().unwrap();
    let connector = OneShotConnector(Mutex::new(Some(client)));
    let cwd = PathBuf::from(OsString::from_vec(b"/tmp/f\xff".to_vec()));
    let runtime = layout(cwd, PathBuf::from("/unused"));

    let result = try_service_with(
        &search_request("alpha"),
        &runtime,
        &connector,
        &SystemPeerCredentials,
    );

    assert!(matches!(result, Some(Err(_))), "never direct fallback");
    let mut byte = [0_u8; 1];
    assert_eq!(server.read(&mut byte).unwrap(), 0, "no request bytes");
}

struct DeadlineConnector;

impl Connector for DeadlineConnector {
    fn connect(&self, _: &Path, deadline: Instant) -> ConnectOutcome {
        thread::sleep(deadline.saturating_duration_since(Instant::now()));
        ConnectOutcome::Absent
    }
}

struct FailedConnector;

impl Connector for FailedConnector {
    fn connect(&self, _: &Path, _: Instant) -> ConnectOutcome {
        ConnectOutcome::Failed(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "refused by policy",
        ))
    }
}

#[test]
fn pre_connect_nonabsence_failures_are_operational() {
    let runtime = layout(PathBuf::from("/tmp"), PathBuf::from("/unused"));
    let outcome = try_service_with(
        &search_request("alpha"),
        &runtime,
        &FailedConnector,
        &SystemPeerCredentials,
    );
    assert!(matches!(outcome, Some(Err(_))), "failure is not absence");
}

#[test]
fn pending_connect_respects_the_absolute_bound() {
    let runtime = layout(PathBuf::from("/tmp"), PathBuf::from("/unused"));
    let started = Instant::now();
    let outcome = try_service_with(
        &search_request("alpha"),
        &runtime,
        &DeadlineConnector,
        &SystemPeerCredentials,
    );
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(
        outcome.is_none(),
        "pre-connect expiry allows direct execution"
    );
}

struct CountingConnector(Arc<AtomicUsize>);

impl Connector for CountingConnector {
    fn connect(&self, _: &Path, _: Instant) -> ConnectOutcome {
        self.0.fetch_add(1, Ordering::SeqCst);
        ConnectOutcome::Absent
    }
}

#[test]
fn oversize_query_is_service_ineligible_before_connecting() {
    let connections = Arc::new(AtomicUsize::new(0));
    let connector = CountingConnector(Arc::clone(&connections));
    let runtime = layout(PathBuf::from("/tmp"), PathBuf::from("/unused"));
    let query = "x".repeat(yams_protocol::MAX_ARGUMENT_BYTES + 1);

    let outcome = try_service_with(
        &search_request(&query),
        &runtime,
        &connector,
        &SystemPeerCredentials,
    );

    assert!(outcome.is_none(), "oversize query runs directly");
    assert_eq!(connections.load(Ordering::SeqCst), 0, "never connected");

    let exact = "x".repeat(yams_protocol::MAX_ARGUMENT_BYTES);
    let exact_outcome = try_service_with(
        &search_request(&exact),
        &runtime,
        &connector,
        &SystemPeerCredentials,
    );
    assert!(exact_outcome.is_none(), "absent connector still falls back");
    assert_eq!(
        connections.load(Ordering::SeqCst),
        1,
        "the exact argument limit remains service-eligible"
    );
}

#[test]
fn pre_accept_read_times_out_while_the_server_is_still_open() {
    let (request_seen_tx, request_seen_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server_closed = Arc::new(AtomicBool::new(false));
    let closed_by_server = Arc::clone(&server_closed);
    let (_temporary, runtime, silent) = service(move |mut stream| {
        let request = match receive_request(&mut stream).unwrap() {
            Message::Request(request) => request,
            other => panic!("expected request, got {other:?}"),
        };
        assert_eq!(request.operation.kind, OperationKind::Index);
        request_seen_tx.send(()).unwrap();
        let _ = release_rx.recv_timeout(Duration::from_secs(10));
        closed_by_server.store(true, Ordering::SeqCst);
    });
    let request = index_request(&runtime.cwd);

    let (client_tx, client_rx) = mpsc::channel();
    let client = thread::spawn(move || {
        client_tx.send(try_service(&request, &runtime)).unwrap();
    });
    request_seen_rx
        .recv_timeout(Duration::from_secs(3))
        .expect("server received the request");
    let result = match client_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(result) => result,
        Err(error) => {
            let _ = release_tx.send(());
            let _ = client.join();
            let _ = silent.join();
            panic!("client did not honor its pre-Accept deadline: {error}");
        }
    };
    assert!(
        !server_closed.load(Ordering::SeqCst),
        "client returned only after the server closed"
    );
    let completion = result
        .expect("a delivered index cannot fall back to direct")
        .unwrap_err();
    assert!(
        completion.stderr.contains("after request delivery"),
        "{}",
        completion.stderr
    );
    release_tx.send(()).unwrap();
    client.join().unwrap();
    silent.join().unwrap();
}

#[test]
fn service_rejection_is_operational() {
    let (_temporary, runtime, rejected) = service(|mut stream| {
        let request = receive_request(&mut stream).unwrap();
        assert!(matches!(request, Message::Request(_)));
        send_message(
            &mut stream,
            &Message::Rejected(Rejected {
                code: "busy".into(),
                message: "try later".into(),
            }),
        )
        .unwrap();
    });
    let completion = try_service(&search_request("alpha"), &runtime)
        .expect("rejection never falls back")
        .unwrap_err();
    assert!(completion.stderr.contains("service rejected request"));
    rejected.join().unwrap();
}

#[test]
fn mismatched_completion_request_id_is_operational() {
    let (_temporary, runtime, handle) = service(|mut stream| {
        let request = receive_request(&mut stream).unwrap();
        assert!(matches!(request, Message::Request(_)));
        send_message(
            &mut stream,
            &Message::Accepted(Accepted {
                request_id: "accepted".into(),
            }),
        )
        .unwrap();
        send_message(
            &mut stream,
            &Message::Completed(Completed {
                request_id: "different".into(),
                exit_code: 0,
                stdout: String::new(),
                stderr: String::new(),
            }),
        )
        .unwrap();
    });
    let completion = try_service(&search_request("alpha"), &runtime)
        .expect("connected failure remains operational")
        .unwrap_err();
    assert!(
        completion.stderr.contains("request ID did not match"),
        "{}",
        completion.stderr
    );
    handle.join().unwrap();
}

#[test]
fn fast_closed_truncated_completion_is_rejected() {
    let (_temporary, runtime, handle) = service(|mut stream| {
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
        let body = encode(&Message::Completed(Completed {
            request_id: "expected".into(),
            exit_code: 0,
            stdout: String::new(),
            stderr: String::new(),
        }))
        .unwrap();
        stream
            .write_all(&(body.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(&body[..body.len() - 1]).unwrap();
    });

    let completion = try_service(&search_request("alpha"), &runtime)
        .expect("connected failure remains operational")
        .unwrap_err();
    assert!(
        completion.stderr.contains("truncated frame"),
        "{}",
        completion.stderr
    );
    handle.join().unwrap();
}
