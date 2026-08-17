use std::fs;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use sha2::{Digest, Sha256};
use yams_embed::{
    ConstructionLease, ConstructionLockError, ConstructionWait, Embedder, JINA_ARTIFACTS_SHA256,
    JINA_DIMENSIONS, JINA_REVISION, JinaEmbedder, JinaError, build_online_with_endpoint,
};

const LOCAL_HTTP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;

#[test]
fn offline_construction_reports_the_first_missing_artifact_without_network() {
    let model_cache = tempfile::tempdir().unwrap();
    let lock_dir = tempfile::tempdir().unwrap();

    let error = match JinaEmbedder::offline(model_cache.path(), lock_dir.path()) {
        Ok(_) => panic!("an empty cache must not construct the model"),
        Err(error) => error,
    };

    assert!(matches!(
        error,
        JinaError::MissingOfflineArtifact {
            artifact: "model.onnx",
            ..
        }
    ));
    assert!(error.to_string().contains("network is off"));
    assert!(error.to_string().contains("YAMS_ALLOW_NET=1"));
}

#[test]
fn offline_construction_requires_each_named_artifact_explicitly() {
    for artifact in [
        "model.onnx",
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ] {
        let model_cache = tempfile::tempdir().unwrap();
        let snapshot = write_corrupt_cache(model_cache.path());
        fs::remove_file(snapshot.join(artifact)).unwrap();
        let lock_dir = tempfile::tempdir().unwrap();

        let error = match JinaEmbedder::offline(model_cache.path(), lock_dir.path()) {
            Ok(_) => panic!("missing {artifact} must be reported before model construction"),
            Err(error) => error,
        };
        assert!(
            matches!(
                error,
                JinaError::MissingOfflineArtifact {
                    artifact: found,
                    ..
                } if found == artifact
            ),
            "{artifact}: {error:?}"
        );
    }
}

#[test]
fn offline_construction_refuses_artifacts_that_miss_the_pinned_digest() {
    let model_cache = tempfile::tempdir().unwrap();
    let snapshot = write_corrupt_cache(model_cache.path());
    let lock_dir = tempfile::tempdir().unwrap();

    let error = match JinaEmbedder::offline(model_cache.path(), lock_dir.path()) {
        Ok(_) => panic!("artifacts that miss the pinned digest must not construct"),
        Err(error) => error,
    };

    assert!(
        matches!(error, JinaError::PinnedArtifactsMismatch { .. }),
        "{error:?}"
    );
    let message = error.to_string();
    for expected in [
        JINA_REVISION,
        JINA_ARTIFACTS_SHA256,
        &snapshot.display().to_string(),
        "YAMS_ALLOW_NET=1",
    ] {
        assert!(
            message.contains(expected),
            "{expected} missing from {message}"
        );
    }
}

#[test]
fn a_cache_holding_only_a_superseded_snapshot_names_the_pinned_one() {
    const SUPERSEDED: &str = "0123456789abcdef0123456789abcdef01234567";
    let model_cache = tempfile::tempdir().unwrap();
    write_snapshot(model_cache.path(), SUPERSEDED);
    let lock_dir = tempfile::tempdir().unwrap();

    let error = match JinaEmbedder::offline(model_cache.path(), lock_dir.path()) {
        Ok(_) => panic!("a superseded snapshot must not construct"),
        Err(error) => error,
    };

    assert!(
        matches!(error, JinaError::PinnedSnapshotMissing { .. }),
        "{error:?}"
    );
    let message = error.to_string();
    for expected in [JINA_REVISION, "YAMS_ALLOW_NET=1", "yams --index"] {
        assert!(
            message.contains(expected),
            "{expected} missing from {message}"
        );
    }
}

#[test]
fn a_hostile_cache_is_reported_without_spending_a_download() {
    let model_cache = tempfile::tempdir().unwrap();
    let snapshot = write_corrupt_cache(model_cache.path());
    // A snapshot directory another user can write is never repaired by
    // fetching: hf-hub keeps whatever entry is already there.
    fs::set_permissions(&snapshot, fs::Permissions::from_mode(0o777)).unwrap();

    // Any attempt to reach this endpoint fails loudly rather than silently
    // reaching the real hub, so a regression here cannot pass as a slow test.
    let error = build_online_with_endpoint(model_cache.path(), "http://127.0.0.1:1")
        .expect_err("a shared-writable snapshot must fail closed");

    assert!(
        matches!(error, JinaError::UnsafeOfflineCache { .. }),
        "{error:?}"
    );
}

#[test]
fn unreadable_cached_artifacts_have_a_typed_error() {
    let model_cache = tempfile::tempdir().unwrap();
    let snapshot = write_corrupt_cache(model_cache.path());
    fs::remove_file(snapshot.join("model.onnx")).unwrap();
    fs::create_dir(snapshot.join("model.onnx")).unwrap();
    let lock_dir = tempfile::tempdir().unwrap();

    let error = match JinaEmbedder::offline(model_cache.path(), lock_dir.path()) {
        Ok(_) => panic!("a directory cannot be read as ONNX model bytes"),
        Err(error) => error,
    };

    assert!(matches!(error, JinaError::UnsafeOfflineCache { .. }));
}

#[test]
#[ignore = "requires an explicit pinned Jina cache, signature, and embedding digest"]
fn cached_jina_v2_has_the_frozen_contract() {
    let model_cache = PathBuf::from(
        std::env::var_os("YAMS_TEST_JINA_MODEL_CACHE")
            .expect("set YAMS_TEST_JINA_MODEL_CACHE to an explicit populated cache"),
    );
    let expected_signature = std::env::var("YAMS_TEST_JINA_EXPECTED_SIGNATURE")
        .expect("set YAMS_TEST_JINA_EXPECTED_SIGNATURE to the exact resolved signature");
    let expected_query_sha256 = std::env::var("YAMS_TEST_JINA_EXPECTED_QUERY_SHA256")
        .expect("set YAMS_TEST_JINA_EXPECTED_QUERY_SHA256 to the pinned query digest");
    // The revision and artifact digest ship as source constants; the operator
    // value is a cross-check of the whole signature, including the runtime and
    // target components the build cannot pin for every host.
    assert!(
        expected_signature.ends_with(&format!(
            "|snapshot={JINA_REVISION}|artifacts_sha256={JINA_ARTIFACTS_SHA256}"
        )),
        "YAMS_TEST_JINA_EXPECTED_SIGNATURE disagrees with the pins compiled into this build"
    );
    let lock_dir = tempfile::tempdir().unwrap();
    let mut model = JinaEmbedder::offline(&model_cache, lock_dir.path()).unwrap();

    assert_eq!(model.signature(), expected_signature);
    assert_eq!(model.dimensions(), JINA_DIMENSIONS);
    let _first = ConstructionLease::acquire(lock_dir.path()).unwrap();
    let _second = ConstructionLease::acquire(lock_dir.path()).unwrap();
    let embedding = model.embed_query("memory search").unwrap();
    assert_eq!(embedding.dimensions(), 768);
    let mut digest = Sha256::new();
    for value in embedding.values() {
        digest.update(value.to_le_bytes());
    }
    assert_eq!(lower_hex(&digest.finalize()), expected_query_sha256);
}

#[test]
fn jina_dimensions_are_frozen() {
    assert_eq!(JINA_DIMENSIONS, 768);
}

#[test]
fn online_bootstrap_pins_the_revision_and_sends_no_authorization_header() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = thread::spawn(move || -> io::Result<Vec<u8>> {
        let mut socket = accept_with_timeout(&listener, LOCAL_HTTP_TIMEOUT)?;
        let capture = (|| {
            socket.set_read_timeout(Some(LOCAL_HTTP_TIMEOUT))?;
            socket.set_write_timeout(Some(LOCAL_HTTP_TIMEOUT))?;
            read_request_headers(&mut socket)
        })();
        let response = if capture.is_ok() {
            b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".as_slice()
        } else {
            b"HTTP/1.1 400 Bad Request\r\ncontent-length: 0\r\nconnection: close\r\n\r\n".as_slice()
        };
        let response_result = socket.write_all(response);
        match capture {
            Ok(request) => {
                response_result?;
                Ok(request)
            }
            Err(error) => {
                let _ = response_result;
                Err(error)
            }
        }
    });

    // A test-owned "ambient" token, colocated with model_cache exactly the
    // way a real `~/.cache/huggingface/{hub,token}` pair would be, but
    // entirely inside a tempdir this test controls. It exists so the
    // assertion below proves the fix wins over *any* cache-supplied token,
    // without ever touching the real ~/.cache/huggingface/token or setting
    // process environment variables.
    let root = tempfile::tempdir().unwrap();
    let model_cache = root.path().join("hub");
    fs::create_dir_all(&model_cache).unwrap();
    fs::write(root.path().join("token"), "test-only-ambient-token").unwrap();

    let (bootstrap_tx, bootstrap_rx) = mpsc::channel();
    let bootstrap = thread::spawn(move || {
        let result = build_online_with_endpoint(&model_cache, &endpoint);
        bootstrap_tx.send(result).unwrap();
    });

    let bootstrap_result = match bootstrap_rx.recv_timeout(LOCAL_HTTP_TIMEOUT) {
        Ok(result) => result,
        Err(error) => {
            let server_result = server.join();
            panic!(
                "online bootstrap must complete within the test timeout ({error}); local HTTP server outcome: {server_result:?}"
            );
        }
    };
    bootstrap.join().expect("online bootstrap thread panicked");
    let request = server
        .join()
        .expect("local HTTP server thread panicked")
        .expect("local HTTP server must capture complete request headers");

    let error = bootstrap_result.expect_err("the local HTTP server always returns 404");
    assert!(
        matches!(&error, JinaError::ModelDownload(message) if message.contains("404")),
        "local 404 must surface as a typed model-download failure: {error:?}"
    );
    assert!(request.ends_with(b"\r\n\r\n"));
    let request = String::from_utf8_lossy(&request);
    assert!(
        !request.to_ascii_lowercase().contains("authorization:"),
        "ambient credential must never be sent: {request}"
    );
    assert!(
        request.contains(&format!(
            "/jinaai/jina-embeddings-v2-base-en/resolve/{JINA_REVISION}/"
        )),
        "the download must be qualified by the pinned revision, never a moving branch: {request}"
    );
}

fn accept_with_timeout(listener: &TcpListener, timeout: Duration) -> io::Result<TcpStream> {
    listener.set_nonblocking(true)?;
    let deadline = Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false)?;
                return Ok(socket);
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for local HTTP connection",
                    ));
                }
                thread::sleep(Duration::from_millis(5));
            }
            Err(error) => return Err(error),
        }
    }
}

#[test]
fn local_http_accept_returns_a_blocking_socket() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let client = thread::spawn(move || TcpStream::connect(address).unwrap());

    let socket = accept_with_timeout(&listener, LOCAL_HTTP_TIMEOUT).unwrap();

    assert!(
        !rustix::fs::fcntl_getfl(&socket)
            .unwrap()
            .contains(rustix::fs::OFlags::NONBLOCK),
        "accepted local HTTP sockets must not inherit the listener's polling mode"
    );
    drop(client.join().unwrap());
}

fn read_request_headers(socket: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut request = Vec::new();
    loop {
        let remaining = MAX_REQUEST_HEADER_BYTES - request.len();
        if remaining == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP request headers exceeded the test byte cap",
            ));
        }

        let mut buffer = [0u8; 1024];
        let read_len = remaining.min(buffer.len());
        let n = socket.read(&mut buffer[..read_len])?;
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "HTTP request ended before the header terminator",
            ));
        }
        request.extend_from_slice(&buffer[..n]);

        if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
            request.truncate(end + 4);
            return Ok(request);
        }
    }
}

#[test]
fn two_construction_slots_are_available_and_a_third_times_out() {
    let lock_dir = tempfile::tempdir().unwrap();
    let first = ConstructionLease::acquire(lock_dir.path()).unwrap();
    let second = ConstructionLease::acquire(lock_dir.path()).unwrap();

    assert_ne!(first.slot(), second.slot());
    let error = ConstructionLease::acquire_with_wait(
        lock_dir.path(),
        ConstructionWait::new(
            Duration::from_millis(5),
            Duration::from_secs(1),
            Duration::from_millis(30),
        ),
        |_| {},
    )
    .unwrap_err();
    assert!(matches!(error, ConstructionLockError::Busy { .. }));
}

#[test]
fn a_waiter_announces_once_and_progresses_after_a_slot_is_released() {
    let lock_dir = tempfile::tempdir().unwrap();
    let first = ConstructionLease::acquire(lock_dir.path()).unwrap();
    let _second = ConstructionLease::acquire(lock_dir.path()).unwrap();
    let (note_sender, note_receiver) = mpsc::channel();
    let waiter_dir = lock_dir.path().to_path_buf();
    let waiter = thread::spawn(move || {
        ConstructionLease::acquire_with_wait(
            waiter_dir,
            ConstructionWait::new(
                Duration::from_millis(5),
                Duration::from_millis(15),
                Duration::from_secs(2),
            ),
            |note| note_sender.send(note.to_string()).unwrap(),
        )
    });

    let note = note_receiver.recv_timeout(Duration::from_secs(1)).unwrap();
    drop(first);
    let lease = waiter.join().unwrap().unwrap();

    assert!(lease.slot() < 2);
    assert!(note.contains("waiting for a model-construction slot"));
    assert!(matches!(
        note_receiver.try_recv(),
        Err(mpsc::TryRecvError::Disconnected)
    ));
}

#[test]
fn construction_slots_are_private_zero_byte_regular_files() {
    let lock_dir = tempfile::tempdir().unwrap();
    let first = ConstructionLease::acquire(lock_dir.path()).unwrap();
    let second = ConstructionLease::acquire(lock_dir.path()).unwrap();

    for lease in [&first, &second] {
        let metadata = fs::symlink_metadata(lease.path()).unwrap();
        assert!(metadata.is_file());
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o600);
        assert_eq!(metadata.len(), 0);
    }
}

#[test]
fn unsafe_slot_objects_fail_closed() {
    for kind in [
        "symlink",
        "directory",
        "fifo",
        "hardlink",
        "wrong-mode",
        "nonzero",
    ] {
        let lock_dir = tempfile::tempdir().unwrap();
        let slot = lock_dir.path().join(".yams-model-load-0.lock");
        let victim = lock_dir.path().join("victim");
        fs::write(&victim, b"protected").unwrap();
        match kind {
            "symlink" => symlink(&victim, &slot).unwrap(),
            "directory" => fs::create_dir(&slot).unwrap(),
            "fifo" => assert!(
                Command::new("mkfifo")
                    .arg(&slot)
                    .status()
                    .unwrap()
                    .success()
            ),
            "hardlink" => {
                fs::File::create(&slot).unwrap();
                fs::set_permissions(&slot, fs::Permissions::from_mode(0o600)).unwrap();
                fs::hard_link(&slot, lock_dir.path().join("other-link")).unwrap();
            }
            "wrong-mode" => {
                fs::File::create(&slot).unwrap();
                fs::set_permissions(&slot, fs::Permissions::from_mode(0o644)).unwrap();
            }
            "nonzero" => {
                fs::write(&slot, b"not empty").unwrap();
                fs::set_permissions(&slot, fs::Permissions::from_mode(0o600)).unwrap();
            }
            _ => unreachable!(),
        }

        let error = ConstructionLease::acquire(lock_dir.path()).unwrap_err();
        assert!(
            matches!(error, ConstructionLockError::Unsafe { .. }),
            "{kind}: {error:?}"
        );
        assert_eq!(fs::read(&victim).unwrap(), b"protected");
    }
}

#[test]
fn replacing_a_held_slot_is_detected_by_descriptor_and_name_revalidation() {
    let lock_dir = tempfile::tempdir().unwrap();
    let lease = ConstructionLease::acquire(lock_dir.path()).unwrap();
    let stale = lock_dir.path().join("stale.lock");
    fs::rename(lease.path(), &stale).unwrap();
    fs::File::create(lease.path()).unwrap();
    fs::set_permissions(lease.path(), fs::Permissions::from_mode(0o600)).unwrap();

    assert!(matches!(
        lease.revalidate(),
        Err(ConstructionLockError::Rebound { .. })
    ));
}

#[test]
fn replacing_the_lock_directory_is_detected_by_confinement_revalidation() {
    let parent = tempfile::tempdir().unwrap();
    let lock_dir = parent.path().join("locks");
    fs::create_dir(&lock_dir).unwrap();
    fs::set_permissions(&lock_dir, fs::Permissions::from_mode(0o700)).unwrap();
    let lease = ConstructionLease::acquire(&lock_dir).unwrap();
    fs::rename(&lock_dir, parent.path().join("old-locks")).unwrap();
    fs::create_dir(&lock_dir).unwrap();
    fs::set_permissions(&lock_dir, fs::Permissions::from_mode(0o700)).unwrap();

    assert!(matches!(
        lease.revalidate(),
        Err(ConstructionLockError::Rebound { .. })
    ));
}

#[test]
fn failed_construction_releases_its_slot() {
    let model_cache = tempfile::tempdir().unwrap();
    write_corrupt_cache(model_cache.path());
    let lock_dir = tempfile::tempdir().unwrap();

    let error = match JinaEmbedder::offline(model_cache.path(), lock_dir.path()) {
        Ok(_) => panic!("corrupt model bytes must not construct"),
        Err(error) => error,
    };
    assert!(matches!(error, JinaError::PinnedArtifactsMismatch { .. }));
    for slot in 0..2 {
        assert!(
            lock_dir
                .path()
                .join(format!(".yams-model-load-{slot}.lock"))
                .is_file(),
            "construction must acquire through the persistent two-slot lease"
        );
    }

    let _first = ConstructionLease::acquire(lock_dir.path()).unwrap();
    let _second = ConstructionLease::acquire(lock_dir.path()).unwrap();
}

#[test]
fn a_missing_lock_directory_and_its_missing_parent_are_provisioned_privately() {
    let base = tempfile::tempdir().unwrap();
    let store_dir = base.path().join("rust-v1");
    let lock_dir = store_dir.join("locks");

    let lease = ConstructionLease::acquire(&lock_dir).unwrap();

    assert_eq!(
        lease.path().parent().unwrap(),
        fs::canonicalize(&lock_dir).unwrap()
    );
    for provisioned in [&store_dir, &lock_dir] {
        let metadata = fs::symlink_metadata(provisioned).unwrap();
        assert!(
            metadata.is_dir(),
            "{provisioned:?} must be a real directory"
        );
        assert_eq!(
            metadata.permissions().mode() & 0o7777,
            0o700,
            "{provisioned:?} must be provisioned private"
        );
    }
}

#[test]
fn construction_provisions_its_missing_lock_directory_before_loading_the_model() {
    let model_cache = tempfile::tempdir().unwrap();
    write_corrupt_cache(model_cache.path());
    let base = tempfile::tempdir().unwrap();
    let lock_dir = base.path().join("rust-v1").join("locks");

    let error = match JinaEmbedder::offline(model_cache.path(), &lock_dir) {
        Ok(_) => panic!("corrupt model bytes must not construct"),
        Err(error) => error,
    };

    assert!(
        matches!(error, JinaError::PinnedArtifactsMismatch { .. }),
        "construction must reach artifact verification, not the lock stage: {error:?}"
    );
    let metadata = fs::symlink_metadata(&lock_dir).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
    for slot in 0..2 {
        assert!(
            lock_dir
                .join(format!(".yams-model-load-{slot}.lock"))
                .is_file()
        );
    }
}

#[test]
fn a_symlinked_lock_directory_fails_closed_without_touching_its_target() {
    let base = tempfile::tempdir().unwrap();
    let target = base.path().join("target");
    fs::create_dir(&target).unwrap();
    fs::set_permissions(&target, fs::Permissions::from_mode(0o700)).unwrap();
    let victim = target.join("victim");
    fs::write(&victim, b"protected").unwrap();
    let lock_dir = base.path().join("locks");
    symlink(&target, &lock_dir).unwrap();

    let error = ConstructionLease::acquire(&lock_dir).unwrap_err();

    assert!(
        matches!(error, ConstructionLockError::Unsafe { .. }),
        "{error:?}"
    );
    assert_eq!(fs::read(&victim).unwrap(), b"protected");
    assert_eq!(fs::read_dir(&target).unwrap().count(), 1);
    assert!(fs::symlink_metadata(&lock_dir).unwrap().is_symlink());
}

#[test]
fn an_existing_group_writable_lock_directory_is_never_repaired() {
    for mode in [0o775, 0o707] {
        let base = tempfile::tempdir().unwrap();
        let lock_dir = base.path().join("locks");
        fs::create_dir(&lock_dir).unwrap();
        fs::set_permissions(&lock_dir, fs::Permissions::from_mode(mode)).unwrap();

        let error = ConstructionLease::acquire(&lock_dir).unwrap_err();

        assert!(
            matches!(error, ConstructionLockError::Unsafe { .. }),
            "{mode:04o}: {error:?}"
        );
        assert_eq!(
            fs::symlink_metadata(&lock_dir)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            mode,
            "an unsafe lock directory must never be repaired"
        );
        assert_eq!(fs::read_dir(&lock_dir).unwrap().count(), 0);
    }
}

#[test]
fn an_existing_lock_directory_is_used_exactly_as_before() {
    let base = tempfile::tempdir().unwrap();
    let lock_dir = base.path().join("locks");
    fs::create_dir(&lock_dir).unwrap();
    fs::set_permissions(&lock_dir, fs::Permissions::from_mode(0o755)).unwrap();

    let lease = ConstructionLease::acquire(&lock_dir).unwrap();

    assert!(lease.path().is_file());
    assert_eq!(
        fs::symlink_metadata(&lock_dir)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755,
        "an existing lock directory is never re-permissioned"
    );
}

#[test]
fn a_non_directory_at_the_lock_path_fails_closed() {
    let base = tempfile::tempdir().unwrap();
    let lock_dir = base.path().join("locks");
    fs::write(&lock_dir, b"not a directory").unwrap();

    let error = ConstructionLease::acquire(&lock_dir).unwrap_err();

    assert!(
        matches!(error, ConstructionLockError::Unsafe { .. }),
        "{error:?}"
    );
    assert_eq!(fs::read(&lock_dir).unwrap(), b"not a directory");
}

#[test]
fn concurrent_provisioning_of_one_missing_lock_directory_resolves_to_one_directory() {
    let base = tempfile::tempdir().unwrap();
    let lock_dir = base.path().join("rust-v1").join("locks");
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
    let racers: Vec<_> = (0..2)
        .map(|_| {
            let lock_dir = lock_dir.clone();
            let barrier = std::sync::Arc::clone(&barrier);
            thread::spawn(move || {
                barrier.wait();
                ConstructionLease::acquire(&lock_dir)
            })
        })
        .collect();

    let leases: Vec<_> = racers
        .into_iter()
        .map(|racer| racer.join().unwrap().unwrap())
        .collect();

    assert_ne!(leases[0].slot(), leases[1].slot());
    let metadata = fs::symlink_metadata(&lock_dir).unwrap();
    assert!(metadata.is_dir());
    assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
}

#[test]
fn a_lock_directory_purged_mid_wait_fails_closed_without_reprovisioning() {
    let base = tempfile::tempdir().unwrap();
    let lock_dir = base.path().join("rust-v1").join("locks");
    let first = ConstructionLease::acquire(&lock_dir).unwrap();
    let second = ConstructionLease::acquire(&lock_dir).unwrap();
    let (note_sender, note_receiver) = mpsc::channel();
    let waiter_dir = lock_dir.clone();
    // The one-shot notice fires from inside the poll loop, so receiving it
    // proves the waiter is past any entry-time provisioning.
    let waiter = thread::spawn(move || {
        ConstructionLease::acquire_with_wait(
            waiter_dir,
            ConstructionWait::new(
                Duration::from_millis(5),
                Duration::ZERO,
                Duration::from_secs(5),
            ),
            move |note| note_sender.send(note.waited).unwrap(),
        )
    });
    note_receiver.recv_timeout(Duration::from_secs(2)).unwrap();

    // Rename rather than unlink: the directory stops being reachable at the
    // waited-on path in one atomic step, with no window where a poll could
    // legitimately re-acquire through a half-removed tree.
    fs::rename(&lock_dir, base.path().join("purged")).unwrap();
    let error = waiter.join().unwrap().unwrap_err();

    assert!(
        matches!(
            error,
            ConstructionLockError::Unsafe { .. } | ConstructionLockError::Rebound { .. }
        ),
        "a mid-wait disappearance must fail closed: {error:?}"
    );
    assert!(
        fs::symlink_metadata(&lock_dir).is_err(),
        "a waiter must never re-provision the lock directory it is waiting on"
    );
    drop((first, second));
}

#[test]
fn a_symlinked_intermediate_component_fails_closed_without_touching_its_target() {
    for depth in 0..2 {
        let base = tempfile::tempdir().unwrap();
        let target = base.path().join("target");
        fs::create_dir(&target).unwrap();
        let intermediate = base.path().join("state");
        let lock_dir = if depth == 0 {
            symlink(&target, &intermediate).unwrap();
            intermediate.join("rust-v1").join("locks")
        } else {
            fs::create_dir(&intermediate).unwrap();
            symlink(&target, intermediate.join("rust-v1")).unwrap();
            intermediate.join("rust-v1").join("locks")
        };

        let error = ConstructionLease::acquire(&lock_dir).unwrap_err();

        assert!(
            matches!(error, ConstructionLockError::Unprovisionable { .. }),
            "depth {depth}: {error:?}"
        );
        assert_eq!(
            fs::read_dir(&target).unwrap().count(),
            0,
            "depth {depth}: a symlinked component's target must stay untouched"
        );
    }
}

#[test]
fn a_regular_file_at_an_intermediate_component_fails_closed() {
    let base = tempfile::tempdir().unwrap();
    let state = base.path().join("state");
    fs::create_dir(&state).unwrap();
    let occupied = state.join("rust-v1");
    fs::write(&occupied, b"not a directory").unwrap();

    let error = ConstructionLease::acquire(occupied.join("locks")).unwrap_err();

    assert!(
        matches!(error, ConstructionLockError::Unprovisionable { .. }),
        "{error:?}"
    );
    assert_eq!(fs::read(&occupied).unwrap(), b"not a directory");
}

#[test]
fn a_group_writable_attachment_point_refuses_to_provision_anything() {
    for mode in [0o775, 0o707] {
        let base = tempfile::tempdir().unwrap();
        let attachment = base.path().join("shared");
        fs::create_dir(&attachment).unwrap();
        fs::set_permissions(&attachment, fs::Permissions::from_mode(mode)).unwrap();

        let error =
            ConstructionLease::acquire(attachment.join("rust-v1").join("locks")).unwrap_err();

        assert!(
            matches!(error, ConstructionLockError::Unprovisionable { .. }),
            "{mode:04o}: {error:?}"
        );
        assert_eq!(
            fs::read_dir(&attachment).unwrap().count(),
            0,
            "{mode:04o}: nothing may be created under a shared-writable attachment point"
        );
        assert_eq!(
            fs::symlink_metadata(&attachment)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            mode
        );
    }
}

#[test]
fn a_conventional_attachment_point_provisions_the_whole_missing_chain() {
    let base = tempfile::tempdir().unwrap();
    let attachment = base.path().join("state");
    fs::create_dir(&attachment).unwrap();
    fs::set_permissions(&attachment, fs::Permissions::from_mode(0o755)).unwrap();
    let store_dir = attachment.join("rust-v1");
    let lock_dir = store_dir.join("locks");

    let lease = ConstructionLease::acquire(&lock_dir).unwrap();

    assert!(lease.path().is_file());
    for provisioned in [&store_dir, &lock_dir] {
        let metadata = fs::symlink_metadata(provisioned).unwrap();
        assert!(metadata.is_dir());
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
    }
    assert_eq!(
        fs::symlink_metadata(&attachment)
            .unwrap()
            .permissions()
            .mode()
            & 0o7777,
        0o755,
        "an existing attachment point is never re-permissioned"
    );
}

/// A structurally valid cache for the pinned revision whose artifact bytes are
/// deliberately not the release-verified ones.
fn write_corrupt_cache(cache: &Path) -> PathBuf {
    write_snapshot(cache, JINA_REVISION)
}

fn write_snapshot(cache: &Path, revision: &str) -> PathBuf {
    let repository = cache.join("models--jinaai--jina-embeddings-v2-base-en");
    let snapshot = repository.join("snapshots").join(revision);
    fs::create_dir_all(repository.join("refs")).unwrap();
    fs::create_dir_all(repository.join("blobs")).unwrap();
    fs::create_dir_all(&snapshot).unwrap();
    fs::write(repository.join("refs").join(revision), revision).unwrap();
    for artifact in [
        "model.onnx",
        "tokenizer.json",
        "config.json",
        "special_tokens_map.json",
        "tokenizer_config.json",
    ] {
        fs::write(snapshot.join(artifact), b"not a valid model artifact").unwrap();
    }
    snapshot
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}
