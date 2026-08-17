use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

use yams_service::{
    SocketProvenance, bind_listener, bind_with_provenance, cleanup_owned_socket,
    computed_default_socket, parse_service_args_in, prepare_default_runtime_dir,
};

fn private_dir() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    dir
}

fn socket_path(dir: &tempfile::TempDir, name: &str) -> std::path::PathBuf {
    dir.path().canonicalize().unwrap().join(name)
}

#[test]
fn omitted_socket_selects_the_computed_default_and_prepares_its_parent() {
    let temp = private_dir();
    let (socket, _idle, provenance) =
        parse_service_args_in(&[], &[("TMPDIR", temp.path())]).unwrap();
    let expected_dir = std::fs::canonicalize(temp.path())
        .unwrap()
        .join(format!("yams-{}", rustix::process::getuid().as_raw()));
    assert_eq!(socket, expected_dir.join("service.sock"));
    assert_eq!(provenance, SocketProvenance::ComputedDefault);

    prepare_default_runtime_dir(&expected_dir).unwrap();

    let mode = std::fs::metadata(&expected_dir)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o700);
}

#[test]
fn computed_default_resolves_a_symlinked_temporary_directory_before_binding() {
    let temp = private_dir();
    let real_temp = temp.path().join("real-tmp");
    let alias_temp = temp.path().join("tmp-alias");
    std::fs::create_dir(&real_temp).unwrap();
    std::fs::set_permissions(&real_temp, std::fs::Permissions::from_mode(0o1777)).unwrap();
    std::os::unix::fs::symlink(&real_temp, &alias_temp).unwrap();

    let (socket, _idle, provenance) =
        parse_service_args_in(&[], &[("TMPDIR", alias_temp.as_path())]).unwrap();
    let expected = computed_default_socket(&std::fs::canonicalize(&real_temp).unwrap());
    assert_eq!(socket, expected);

    let (listener, owned) = bind_with_provenance(&socket, provenance).unwrap();
    drop(listener);
    cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn explicit_socket_equal_to_the_default_path_does_not_create_its_parent() {
    let temp = private_dir();
    let default_path = computed_default_socket(&std::fs::canonicalize(temp.path()).unwrap());
    let arguments = [
        std::ffi::OsString::from("--socket"),
        default_path.clone().into_os_string(),
    ];
    let (socket, _idle, provenance) =
        parse_service_args_in(&arguments, &[("TMPDIR", temp.path())]).unwrap();
    assert_eq!(socket, default_path);
    assert_eq!(provenance, SocketProvenance::Explicit);

    bind_with_provenance(&socket, provenance).unwrap_err();

    assert!(!socket.parent().unwrap().exists());
}

#[test]
fn computed_default_binds_beneath_a_sticky_shared_temporary_directory() {
    let temp = private_dir();
    let shared_temp = temp.path().join("shared-tmp");
    std::fs::create_dir(&shared_temp).unwrap();
    std::fs::set_permissions(&shared_temp, std::fs::Permissions::from_mode(0o1777)).unwrap();
    let (socket, _idle, provenance) =
        parse_service_args_in(&[], &[("TMPDIR", shared_temp.as_path())]).unwrap();

    let (listener, owned) = bind_with_provenance(&socket, provenance).unwrap();

    assert!(socket.exists());
    drop(listener);
    cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn computed_default_rejects_a_nonsticky_shared_ancestor() {
    let temp = private_dir();
    let shared_temp = temp.path().join("shared-tmp");
    std::fs::create_dir(&shared_temp).unwrap();
    std::fs::set_permissions(&shared_temp, std::fs::Permissions::from_mode(0o777)).unwrap();
    let (socket, _idle, provenance) =
        parse_service_args_in(&[], &[("TMPDIR", shared_temp.as_path())]).unwrap();

    bind_with_provenance(&socket, provenance)
        .expect_err("a writable shared ancestor needs the sticky bit");

    assert!(!socket.exists());
}

#[test]
fn explicit_socket_beneath_a_sticky_shared_ancestor_keeps_the_full_policy() {
    let temp = private_dir();
    let shared_temp = temp.path().join("shared-tmp");
    let socket_parent = shared_temp.join("explicit-parent");
    std::fs::create_dir(&shared_temp).unwrap();
    std::fs::set_permissions(&shared_temp, std::fs::Permissions::from_mode(0o1777)).unwrap();
    std::fs::create_dir(&socket_parent).unwrap();
    std::fs::set_permissions(&socket_parent, std::fs::Permissions::from_mode(0o700)).unwrap();
    let socket = socket_parent.join("service.sock");

    bind_with_provenance(&socket, SocketProvenance::Explicit)
        .expect_err("explicit sockets must validate every ancestor");

    assert!(!socket.exists());
}

#[test]
fn prepare_default_runtime_dir_rejects_a_symlink() {
    let temp = private_dir();
    let real = temp.path().join("real");
    std::fs::create_dir(&real).unwrap();
    let link = temp
        .path()
        .join(format!("yams-{}", rustix::process::getuid().as_raw()));
    std::os::unix::fs::symlink(&real, &link).unwrap();

    prepare_default_runtime_dir(&link).expect_err("symlink must be refused");
}

#[test]
fn prepare_default_runtime_dir_rejects_an_existing_non_private_directory() {
    let temp = private_dir();
    let runtime = temp
        .path()
        .join(format!("yams-{}", rustix::process::getuid().as_raw()));
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o755)).unwrap();

    prepare_default_runtime_dir(&runtime).expect_err("non-private directory must be refused");
}

#[test]
fn prepare_default_runtime_dir_rejects_special_permission_bits_without_chmodding() {
    let temp = private_dir();
    let runtime = temp
        .path()
        .join(format!("yams-{}", rustix::process::getuid().as_raw()));
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o1700)).unwrap();

    prepare_default_runtime_dir(&runtime).expect_err("special permission bits must be refused");

    let mode = std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o7777;
    assert_eq!(mode, 0o1700, "an existing directory must not be chmodded");
}

#[test]
fn prepare_default_runtime_dir_rejects_non_normalized_paths_without_creating_them() {
    let temp = private_dir();
    std::fs::create_dir(temp.path().join("intermediate")).unwrap();
    let runtime = temp.path().join("intermediate/../runtime");

    prepare_default_runtime_dir(&runtime).expect_err("dotdot must be refused");

    assert!(!temp.path().join("runtime").exists());
}

#[test]
fn listener_is_private_and_cleanup_requires_owned_identity() {
    let fixture = private_dir();
    let path = socket_path(&fixture, "service.sock");
    let (listener, owned) = bind_listener(&path).unwrap();
    let mode = std::fs::symlink_metadata(&path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    drop(listener);
    cleanup_owned_socket(&owned).unwrap();
    assert!(!path.exists());
}

#[test]
fn stale_socket_is_replaced() {
    let fixture = private_dir();
    let path = socket_path(&fixture, "service.sock");
    let old = UnixListener::bind(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    drop(old);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    while UnixStream::connect(&path).is_ok() {
        assert!(
            std::time::Instant::now() < deadline,
            "closed listener remained connectable"
        );
        std::thread::yield_now();
    }
    let (_, owned) = bind_listener(&path).unwrap();
    cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn live_private_socket_is_not_replaced() {
    let fixture = private_dir();
    let path = socket_path(&fixture, "service.sock");
    let (listener, _) = bind_listener(&path).unwrap();
    let error = bind_listener(&path).unwrap_err();
    assert!(matches!(
        error,
        yams_service::SocketError::AlreadyRunning(_)
    ));
    drop(listener);
}

#[test]
fn relative_and_dotdot_paths_are_rejected() {
    assert!(bind_listener(Path::new("service.sock")).is_err());
    assert!(bind_listener(Path::new("/tmp/../tmp/service.sock")).is_err());
}

#[test]
fn hostile_collisions_are_preserved() {
    let fixture = private_dir();
    for kind in ["file", "dir", "fifo"] {
        let path = socket_path(&fixture, kind);
        match kind {
            "file" => std::fs::write(&path, b"keep").unwrap(),
            "dir" => std::fs::create_dir(&path).unwrap(),
            "fifo" => {
                std::process::Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert!(bind_listener(&path).is_err());
        assert!(Path::new(&path).exists());
    }
}

#[cfg(unix)]
#[test]
fn symlink_collision_is_preserved() {
    let fixture = private_dir();
    let target = socket_path(&fixture, "target");
    let path = socket_path(&fixture, "service.sock");
    std::fs::write(&target, b"keep").unwrap();
    std::os::unix::fs::symlink(&target, &path).unwrap();
    assert!(bind_listener(&path).is_err());
    assert!(
        std::fs::symlink_metadata(&path)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn bind_after_does_not_publish_the_socket_until_prepare_succeeds() {
    let fixture = private_dir();
    let path = socket_path(&fixture, "service.sock");
    let (entered_tx, entered_rx) = std::sync::mpsc::sync_channel(0);
    let (release_tx, release_rx) = std::sync::mpsc::sync_channel(0);
    let socket = path.clone();
    let worker = std::thread::spawn(move || {
        yams_service::bind_after(&socket, SocketProvenance::Explicit, || {
            entered_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok::<_, String>(())
        })
    });

    entered_rx.recv().unwrap();
    assert!(
        UnixStream::connect(&path).is_err(),
        "no listener may exist before prepare finishes"
    );
    release_tx.send(()).unwrap();
    let (listener, owned, ()) = worker.join().unwrap().unwrap();
    UnixStream::connect(&path).expect("listener is published only after prepare");
    drop(listener);
    cleanup_owned_socket(&owned).unwrap();
}

#[test]
fn bind_after_prepare_failure_leaves_no_socket() {
    let fixture = private_dir();
    let path = socket_path(&fixture, "service.sock");
    let error = yams_service::bind_after(&path, SocketProvenance::Explicit, || {
        Err::<(), _>("model still loading")
    })
    .unwrap_err();
    assert_eq!(error, "model still loading");
    assert!(!path.exists());
}

#[test]
fn replacement_is_not_removed_by_cleanup() {
    let fixture = private_dir();
    let path = socket_path(&fixture, "service.sock");
    let (listener, owned) = bind_listener(&path).unwrap();
    drop(listener);
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, b"replacement").unwrap();
    assert!(cleanup_owned_socket(&owned).is_err());
    assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
}
