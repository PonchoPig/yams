use assert_cmd::{Command, cargo::cargo_bin_cmd};
use std::os::unix::fs::PermissionsExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::thread;
use std::time::Duration;

use yams_protocol::{Accepted, Completed, Message, receive_request, send_message};

const OWNED_ENVIRONMENT: &[&str] = &[
    "YAMS_HOME",
    "YAMS_DIRS",
    "YAMS_ALLOW_NET",
    "YAMS_NO_SERVICE",
    "YAMS_SERVICE_SOCKET",
];

fn isolated(mut command: Command) -> Command {
    for name in OWNED_ENVIRONMENT {
        command.env_remove(name);
    }
    command
}

fn management_command(args: &[&str]) -> std::process::Output {
    let home = tempfile::tempdir().unwrap();
    std::process::Command::new(env!("CARGO_BIN_EXE_yams"))
        .args(args)
        .env_clear()
        .env("YAMS_HOME", home.path())
        .env("YAMS_NO_SERVICE", "1")
        // No model cache, no YAMS_ALLOW_NET: any attempted model load fails loudly.
        .output()
        .unwrap()
}

fn run_binary_with(variables: &[(&str, &str)], args: &[&str]) -> std::process::Output {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let home = temporary.path().join("home");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&home).unwrap();
    let mut command = isolated(cargo_bin_cmd!("yams"));
    command.current_dir(&project).args(args).env("HOME", home);
    for (name, value) in variables {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn echo_service(mut stream: UnixStream) {
    yams_protocol::peer::validate_peer(&stream).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let request = match receive_request(&mut stream).unwrap() {
        Message::Request(request) => request,
        other => panic!("expected request, got {other:?}"),
    };
    let request_id = "echo-request".to_owned();
    send_message(
        &mut stream,
        &Message::Accepted(Accepted {
            request_id: request_id.clone(),
        }),
    )
    .unwrap();
    thread::sleep(Duration::from_millis(10));
    send_message(
        &mut stream,
        &Message::Completed(Completed {
            request_id,
            exit_code: 1,
            stdout: serde_json::to_string(&request.argv).unwrap(),
            stderr: String::new(),
        }),
    )
    .unwrap();
}

#[test]
fn management_commands_run_directly_without_loading_a_model() {
    for flag in ["--projects", "--stats", "--gc"] {
        let output = management_command(&[flag]);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stderr.contains("not implemented"), "{flag}: {stderr}");
        assert!(
            !stderr.contains("model"),
            "{flag} must not touch the embedder: {stderr}"
        );
        assert!(
            output.status.success(),
            "{flag} on an empty store must succeed: {stderr}"
        );
    }
}

#[test]
fn set_but_empty_corpus_override_warns_once_naming_the_variable() {
    for variable in ["YAMS_DIRS"] {
        let socket_directory = tempfile::tempdir().unwrap();
        let missing_socket = socket_directory.path().join("missing-service.sock");
        let socket = missing_socket.to_str().unwrap();
        for (route, variables) in [
            ("direct", [("YAMS_NO_SERVICE", "1"), (variable, "")]),
            (
                "service fallback",
                [("YAMS_SERVICE_SOCKET", socket), (variable, "")],
            ),
        ] {
            let output = run_binary_with(&variables, &["query"]);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let expected = format!(
                "yams: warning: {variable} is set but empty; using ordinary corpus discovery"
            );
            assert_eq!(
                stderr.matches("is set but empty").count(),
                1,
                "{route}, {variable}: {stderr}"
            );
            assert!(
                stderr.lines().any(|line| line == expected),
                "warning must exactly name the selected variable: {stderr}"
            );
        }
    }
}

#[test]
fn option_shaped_binary_queries_remain_searches_through_the_service() {
    for query in ["--index", "--stats"] {
        let temporary = tempfile::tempdir().unwrap();
        let project = temporary.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let socket = temporary.path().join("service.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let service = thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            echo_service(stream);
        });

        let output = isolated(cargo_bin_cmd!("yams"))
            .current_dir(&project)
            .args(["--", query])
            .env("YAMS_SERVICE_SOCKET", &socket)
            .output()
            .unwrap();

        service.join().unwrap();
        assert_eq!(output.status.code(), Some(1));
        assert_eq!(output.stderr, b"");
        let argv: Vec<String> = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(&argv[argv.len() - 2..], ["--", query]);
        assert!(
            !argv[..argv.len() - 2]
                .iter()
                .any(|argument| argument == query),
            "{query} must only appear as the positional query: {argv:?}"
        );
    }
}

#[test]
fn stats_only_treats_a_missing_index_as_empty() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let state = temporary.path().join("state");
    std::fs::create_dir(&project).unwrap();
    let index = yams_store::StoreHome::new(&state)
        .project_path(&project)
        .unwrap();
    std::fs::create_dir_all(index.parent().unwrap()).unwrap();
    std::fs::set_permissions(
        state.join("rust-v1"),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::set_permissions(
        index.parent().unwrap(),
        std::fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    std::fs::write(&index, b"this is not sqlite").unwrap();
    std::fs::set_permissions(&index, std::fs::Permissions::from_mode(0o600)).unwrap();

    let output = isolated(cargo_bin_cmd!("yams"))
        .current_dir(&project)
        .arg("--stats")
        .env("YAMS_HOME", &state)
        .env("YAMS_NO_SERVICE", "1")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(4));
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(
        stderr,
        format!(
            "index is not a SQLite database: {}\n",
            index.canonicalize().unwrap().display()
        )
    );
    assert!(!stderr.contains("model"));
}

#[test]
fn primary_and_compatibility_help_are_identical_and_silent_on_stderr() {
    let primary = isolated(cargo_bin_cmd!("yams"))
        .arg("--help")
        .output()
        .unwrap();
    let compatibility = isolated(cargo_bin_cmd!("memory-search"))
        .arg("--help")
        .output()
        .unwrap();

    assert!(primary.status.success());
    assert!(compatibility.status.success());
    assert_eq!(primary.stdout, compatibility.stdout);
    assert_eq!(primary.stderr, b"");
    assert_eq!(compatibility.stderr, b"");
    let help = String::from_utf8(primary.stdout).unwrap();
    assert!(help.contains("Usage: yams"));
    assert!(!help.contains("Usage: memory-search"));
}

#[test]
fn primary_and_compatibility_version_are_identical() {
    let primary = isolated(cargo_bin_cmd!("yams"))
        .arg("--version")
        .output()
        .unwrap();
    let compatibility = isolated(cargo_bin_cmd!("memory-search"))
        .arg("--version")
        .output()
        .unwrap();

    assert!(primary.status.success());
    assert!(compatibility.status.success());
    assert_eq!(primary.stdout, compatibility.stdout);
    assert_eq!(primary.stderr, b"");
    assert_eq!(compatibility.stderr, b"");
    assert_eq!(
        String::from_utf8(primary.stdout).unwrap(),
        format!("yams {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn valid_search_refuses_a_missing_index_without_creating_state() {
    // The index file name is derived from the project directory, so pin the
    // project to a run-owned directory instead of the checkout location.
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let output = isolated(cargo_bin_cmd!("yams"))
        .current_dir(&project)
        .arg("fictional query")
        .env("YAMS_HOME", "/fictional/state")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(output.stdout, b"");
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(
        stderr.starts_with("index is missing: /fictional/state/rust-v1/indexes/project-"),
        "index name must derive from the project directory: {stderr:?}"
    );
    assert!(
        stderr.ends_with(".sqlite3\n"),
        "unexpected diagnostic shape: {stderr:?}"
    );
}

#[test]
fn write_binary_returns_machine_json_without_touching_store_state() {
    let temporary = tempfile::tempdir().unwrap();
    let state = temporary.path().join("state");
    let output = isolated(cargo_bin_cmd!("yams"))
        .current_dir(temporary.path())
        .arg("--write")
        .env("YAMS_HOME", &state)
        .write_stdin("not json")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stderr, b"");
    let body: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["exit"], 2);
    assert!(!state.exists());
}
