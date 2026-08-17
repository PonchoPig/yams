use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};

use yams_cli::{InvocationTime, Platform, RuntimeInputs, execute_direct};
use yams_core::{ExitCode, MAX_FILE_BYTES};

const INITIAL_PAGE: &str = "---\n\
slug: alpha\n\
title: Alpha\n\
type: gotcha\n\
status: historical\n\
owner: shared\n\
updated: 2026-07-28\n\
verified: 2026-07-28\n\
summary: a real trap\n\
---\n\n\
body.\n";

const INITIAL_INDEX: &str = "<!-- BEGIN GENERATED INDEX — edited by yams-wiki catalog, not by hand -->\n\n\
## Gotchas\n\n\
- [alpha](pages/alpha.md) — a real trap\n\n\
<!-- END GENERATED INDEX -->\n";

const CREATE_JSON: &str = r#"{
  "title": "A sapphire observatory needs night mode",
  "type": "gotcha",
  "owner": "codex",
  "fact": "A fictional sapphire observatory rejects jobs unless night mode is enabled.",
  "why": "Synthetic observations show jobs succeed only after enabling night mode.",
  "how_to_apply": "Enable night mode before submitting a fictional job.",
  "falsified_by": "A fictional job succeeding with night mode disabled.",
  "summary": "fictional observatory jobs require night mode",
  "related": ["alpha"]
}"#;

fn runtime(project: &Path) -> RuntimeInputs {
    RuntimeInputs {
        cwd: project.to_owned(),
        home: project.join("fictional-home"),
        temporary_directory: project.join("fictional-tmp"),
        uid: 42,
        platform: Platform::MacOs,
    }
}

fn when() -> InvocationTime {
    InvocationTime {
        civil_date: "2026-08-09".into(),
        utc_timestamp: "2026-08-09T00:00:00.000Z".into(),
    }
}

fn create_wiki(project: &Path) -> PathBuf {
    let corpus = project.join(".agents/memory");
    fs::create_dir_all(corpus.join("pages")).unwrap();
    fs::write(corpus.join("pages/alpha.md"), INITIAL_PAGE).unwrap();
    fs::write(corpus.join("INDEX.md"), INITIAL_INDEX).unwrap();
    corpus
}

fn json(completion: &yams_cli::DirectCompletion) -> serde_json::Value {
    assert_eq!(completion.stderr, "");
    assert_eq!(completion.stdout.lines().count(), 1);
    serde_json::from_str(completion.stdout.trim()).unwrap()
}

#[test]
fn direct_write_uses_the_shared_wiki_without_opening_runtime_state() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("copper-garden");
    fs::create_dir(&project).unwrap();
    let corpus = create_wiki(&project);
    let state = temporary.path().join("state-must-not-be-created");

    let completion = execute_direct(
        ["yams", "--write"],
        [(OsString::from("YAMS_HOME"), state.clone().into_os_string())],
        &runtime(&project),
        CREATE_JSON.as_bytes(),
        &when(),
    );

    assert_eq!(completion.exit_code, ExitCode::Ok);
    let body = json(&completion);
    assert_eq!(body["ok"], true);
    assert!(
        corpus
            .join("pages/a-sapphire-observatory-needs-night-mode.md")
            .is_file()
    );
    assert!(!state.exists());
}

#[test]
fn every_direct_write_failure_is_one_compact_json_object() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("silver-orchard");
    fs::create_dir(&project).unwrap();
    create_wiki(&project);
    let runtime = runtime(&project);
    let mut missing_home = runtime.clone();
    missing_home.home = PathBuf::new();

    let cases = [
        execute_direct(
            ["yams", "--write", "--unknown"],
            std::iter::empty::<(OsString, OsString)>(),
            &runtime,
            b"{}",
            &when(),
        ),
        execute_direct(
            ["yams", "--write"],
            std::iter::empty::<(OsString, OsString)>(),
            &runtime,
            b"not json",
            &when(),
        ),
        execute_direct(
            ["yams", "--write"],
            std::iter::empty::<(OsString, OsString)>(),
            &runtime,
            &vec![b' '; MAX_FILE_BYTES as usize + 1],
            &when(),
        ),
        execute_direct(
            ["yams", "--write"],
            std::iter::empty::<(OsString, OsString)>(),
            &missing_home,
            b"{}",
            &when(),
        ),
    ];
    for completion in cases {
        assert_ne!(completion.exit_code, ExitCode::Ok);
        let body = json(&completion);
        assert_eq!(body["ok"], false);
        assert_eq!(body["exit"], i32::from(completion.exit_code));
    }
}

#[test]
fn missing_write_corpus_is_an_operational_json_result() {
    let temporary = tempfile::tempdir().unwrap();
    let missing_project = temporary.path().join("project-without-wiki");
    fs::create_dir(&missing_project).unwrap();
    let completion = execute_direct(
        [
            "yams",
            "--write",
            "--project",
            missing_project.to_str().unwrap(),
        ],
        [("YAMS_HOME", temporary.path().to_str().unwrap())],
        &runtime(temporary.path()),
        CREATE_JSON.as_bytes(),
        &when(),
    );
    assert_eq!(completion.exit_code, ExitCode::Operational);
    let body = json(&completion);
    assert_eq!(body["ok"], false);
    assert_eq!(body["exit"], 4);
    assert!(!missing_project.join(".agents/memory").exists());
}

#[test]
fn execute_direct_without_an_embedder_does_not_load_a_model() {
    let temporary = tempfile::tempdir().unwrap();
    let completion = execute_direct(
        ["yams", "fictional query"],
        [("YAMS_HOME", temporary.path().to_str().unwrap())],
        &runtime(temporary.path()),
        b"ignored",
        &when(),
    );
    assert_eq!(completion.exit_code, ExitCode::Operational);
    assert_eq!(completion.stdout, "");
    assert_eq!(completion.stderr, "yams: search requires a model\n");
}
