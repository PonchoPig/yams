use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use assert_cmd::cargo::cargo_bin_cmd;
use serde::Serialize;
use serde_json::json;
use tempfile::{TempDir, tempdir};
use yams_core::MAX_FILE_BYTES;
use yams_wiki::{
    AGENT_POLICY, ApplyResult, InitInspection, InitMode, InitPlanRequest, ManifestEnvelope,
    PageType, ProjectPageRequest, canonical_manifest_bytes, inspect_repository, sha256,
};

struct Repository {
    _temporary: TempDir,
    root: PathBuf,
}

impl Repository {
    fn new(name: &str) -> Self {
        let temporary = tempdir().unwrap();
        let root = temporary.path().join(name);
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--quiet"]);
        git(&root, &["config", "user.email", "test@example.invalid"]);
        git(&root, &["config", "user.name", "Yams Test"]);
        Self {
            _temporary: temporary,
            root,
        }
    }

    fn commit_all(&self) {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "--quiet", "-m", "fixture"]);
    }
}

fn git(root: &Path, args: &[&str]) {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("HOME", root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn one_json_line<T: serde::de::DeserializeOwned>(output: &[u8]) -> T {
    assert!(
        output.ends_with(b"\n"),
        "output lacks final newline: {output:?}"
    );
    assert_eq!(
        output.iter().filter(|byte| **byte == b'\n').count(),
        1,
        "output was not exactly one line: {output:?}"
    );
    serde_json::from_slice(&output[..output.len() - 1]).unwrap()
}

fn write_json(path: &Path, value: &impl Serialize) {
    fs::write(path, serde_json::to_vec(value).unwrap()).unwrap();
}

fn assert_one_safe_diagnostic(stderr: &[u8]) {
    let stderr = std::str::from_utf8(stderr).expect("diagnostic must be UTF-8");
    assert_eq!(
        stderr
            .chars()
            .filter(|character| *character == '\n')
            .count(),
        1,
        "diagnostic was not one line: {stderr:?}"
    );
    assert!(
        stderr.ends_with('\n'),
        "diagnostic lacks newline: {stderr:?}"
    );
    assert!(
        stderr
            .trim_end_matches('\n')
            .chars()
            .all(|character| !matches!(character, '\u{0}'..='\u{1f}' | '\u{7f}'..='\u{9f}')),
        "diagnostic retained terminal controls: {stderr:?}"
    );
}

fn request(inspection: &InitInspection, mode: InitMode) -> InitPlanRequest {
    InitPlanRequest {
        root: inspection.root.clone(),
        inspection_sha256: inspection.inspection_sha256.clone(),
        mode,
        date: "2026-08-12".to_owned(),
        agents_md: AGENT_POLICY.to_owned(),
        project_page: ProjectPageRequest {
            title: "Project context".to_owned(),
            page_type: PageType::ProjectState,
            fact: "The fictional project uses approved initialization manifests.".to_owned(),
            why: "Mutations must be reviewable.".to_owned(),
            how_to_apply: "Inspect, plan, approve, and apply.".to_owned(),
            falsified_by: "An unapproved mutation succeeds.".to_owned(),
            summary: "Memory initialization is manifest-driven.".to_owned(),
        },
    }
}

fn inspect_cli(root: &Path) -> InitInspection {
    let output = cargo_bin_cmd!("yams-wiki")
        .arg("init")
        .arg("inspect")
        .arg("--json")
        .arg(root)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stderr, b"");
    one_json_line(&output.stdout)
}

fn plan_cli(request_path: &Path) -> ManifestEnvelope {
    let output = cargo_bin_cmd!("yams-wiki")
        .arg("init")
        .arg("plan")
        .arg("--request")
        .arg(request_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stderr, b"");
    one_json_line(&output.stdout)
}

fn apply_cli(manifest_path: &Path) -> ApplyResult {
    let output = cargo_bin_cmd!("yams-wiki")
        .arg("init")
        .arg("apply")
        .arg("--manifest")
        .arg(manifest_path)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stderr, b"");
    one_json_line(&output.stdout)
}

#[test]
fn init_commands_pin_the_nested_clap_surface_and_required_flags() {
    for args in [
        vec!["init"],
        vec!["init", "inspect"],
        vec!["init", "inspect", "."],
        vec!["init", "plan"],
        vec!["init", "apply"],
    ] {
        cargo_bin_cmd!("yams-wiki")
            .args(args)
            .assert()
            .code(2)
            .stdout("");
    }

    let help = cargo_bin_cmd!("yams-wiki")
        .args(["init", "--help"])
        .output()
        .unwrap();
    assert_eq!(help.status.code(), Some(0));
    let stdout = String::from_utf8(help.stdout).unwrap();
    assert!(stdout.contains("inspect"), "{stdout}");
    assert!(stdout.contains("plan"), "{stdout}");
    assert!(stdout.contains("apply"), "{stdout}");

    let plan_help = cargo_bin_cmd!("yams-wiki")
        .args(["init", "plan", "--help"])
        .output()
        .unwrap();
    assert_eq!(plan_help.status.code(), Some(0));
    let plan_help = String::from_utf8(plan_help.stdout).unwrap();
    assert!(plan_help.contains("--from-inspect"), "{plan_help}");
    assert!(plan_help.contains("--project-page"), "{plan_help}");
    assert!(plan_help.contains("--request"), "{plan_help}");
}

#[test]
fn inspect_prints_the_exact_model_as_one_json_line_for_a_root_with_spaces() {
    let repository = Repository::new("repository with spaces");
    let expected = inspect_repository(&repository.root).unwrap();

    let output = cargo_bin_cmd!("yams-wiki")
        .arg("init")
        .arg("inspect")
        .arg("--json")
        .arg(&repository.root)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0));
    assert_eq!(output.stderr, b"");
    let actual: InitInspection = one_json_line(&output.stdout);
    assert_eq!(actual, expected);
    let mut exact = serde_json::to_vec(&expected).unwrap();
    exact.push(b'\n');
    assert_eq!(output.stdout, exact);
}

#[test]
fn plan_from_inspect_binds_inspection_and_omits_a_hand_copied_request() {
    let repository = Repository::new("from inspect target");
    let files = repository._temporary.path();
    let inspection = inspect_cli(&repository.root);
    let inspection_path = files.join("inspection.json");
    write_json(&inspection_path, &inspection);
    let page_path = files.join("project-page.json");
    write_json(
        &page_path,
        &ProjectPageRequest {
            title: "Project context".to_owned(),
            page_type: PageType::ProjectState,
            fact: "The fictional project uses approved initialization manifests.".to_owned(),
            why: "Mutations must be reviewable.".to_owned(),
            how_to_apply: "Inspect, plan, approve, and apply.".to_owned(),
            falsified_by: "An unapproved mutation succeeds.".to_owned(),
            summary: "Memory initialization is manifest-driven.".to_owned(),
        },
    );

    let output = cargo_bin_cmd!("yams-wiki")
        .args(["init", "plan", "--from-inspect"])
        .arg(&inspection_path)
        .arg("--project-page")
        .arg(&page_path)
        .arg("--date")
        .arg("2026-08-15")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert_eq!(output.stderr, b"");
    let envelope: ManifestEnvelope = one_json_line(&output.stdout);
    assert!(envelope.ok);
    assert_eq!(envelope.manifest.root, inspection.root);
    assert_eq!(
        envelope.manifest.inspection_sha256,
        inspection.inspection_sha256
    );
    assert_eq!(envelope.manifest.mode, InitMode::Full);
}

#[test]
fn minimal_init_flow_accepts_request_and_manifest_paths_with_spaces() {
    let repository = Repository::new("minimal target");
    let inspection = inspect_cli(&repository.root);
    let request_path = repository._temporary.path().join("plan request.json");
    write_json(&request_path, &request(&inspection, InitMode::Minimal));

    let envelope = plan_cli(&request_path);
    assert!(envelope.ok);
    assert_eq!(envelope.manifest.mode, InitMode::Minimal);
    let manifest_path = repository._temporary.path().join("approved manifest.json");
    write_json(&manifest_path, &envelope);

    let result = apply_cli(&manifest_path);
    assert!(result.ok, "{result:?}");
    assert!(result.validated);
    assert_eq!(
        inspect_cli(&repository.root).layout,
        yams_wiki::LayoutClass::Minimal
    );
    assert!(repository.root.join(".agents/memory/.write.lock").exists());
    assert_eq!(
        fs::read_to_string(repository.root.join(".agents/memory/.gitignore")).unwrap(),
        yams_wiki::MEMORY_GITIGNORE
    );
    assert!(
        envelope
            .manifest
            .operations
            .iter()
            .all(|operation| operation.path != ".agents/memory/.write.lock")
    );
}

#[test]
fn full_init_flow_prints_exact_models_and_keeps_the_runtime_lock_outside_the_manifest() {
    let repository = Repository::new("full target");
    let files = repository._temporary.path().join("files with spaces");
    fs::create_dir(&files).unwrap();
    let inspection = inspect_cli(&repository.root);
    let request_path = files.join("request document.json");
    write_json(&request_path, &request(&inspection, InitMode::Full));

    let plan_output = cargo_bin_cmd!("yams-wiki")
        .args(["init", "plan", "--request"])
        .arg(&request_path)
        .output()
        .unwrap();
    assert_eq!(plan_output.status.code(), Some(0));
    assert_eq!(plan_output.stderr, b"");
    let envelope: ManifestEnvelope = one_json_line(&plan_output.stdout);
    let mut expected_plan = serde_json::to_vec(&envelope).unwrap();
    expected_plan.push(b'\n');
    assert_eq!(plan_output.stdout, expected_plan);

    let manifest_path = files.join("manifest document.json");
    write_json(&manifest_path, &envelope);
    let apply_output = cargo_bin_cmd!("yams-wiki")
        .args(["init", "apply", "--manifest"])
        .arg(&manifest_path)
        .output()
        .unwrap();
    assert_eq!(apply_output.status.code(), Some(0));
    assert_eq!(apply_output.stderr, b"");
    let result: ApplyResult = one_json_line(&apply_output.stdout);
    assert!(result.ok, "{result:?}");
    assert_eq!(result.final_layout, yams_wiki::LayoutClass::Full);
    assert_eq!(result.next, ["yams --index"]);
    let mut expected_apply = serde_json::to_vec(&result).unwrap();
    expected_apply.push(b'\n');
    assert_eq!(apply_output.stdout, expected_apply);
    assert!(repository.root.join(".agents/memory/.write.lock").exists());
    assert_eq!(
        fs::read_to_string(repository.root.join(".agents/memory/.gitignore")).unwrap(),
        yams_wiki::MEMORY_GITIGNORE
    );
    assert!(
        envelope
            .manifest
            .operations
            .iter()
            .all(|operation| operation.path != ".agents/memory/.write.lock")
    );
}

#[test]
fn malformed_unknown_and_oversized_plan_requests_exit_two_without_target_changes() {
    let repository = Repository::new("refused planning target");
    let inspection = inspect_cli(&repository.root);
    let request_path = repository._temporary.path().join("request.json");
    let cases = [
        b"not json".to_vec(),
        serde_json::to_vec(&json!({
            "root": inspection.root,
            "inspection_sha256": inspection.inspection_sha256,
            "mode": "minimal",
            "date": "2026-08-12",
            "agents_md": AGENT_POLICY,
            "project_page": {
                "title": "Project context",
                "page_type": "project-state",
                "fact": "A fact.",
                "why": "A reason.",
                "how_to_apply": "Apply it.",
                "falsified_by": "Contrary evidence.",
                "summary": "A summary."
            },
            "unknown": true
        }))
        .unwrap(),
        vec![b' '; MAX_FILE_BYTES as usize + 1],
    ];

    for bytes in cases {
        fs::write(&request_path, bytes).unwrap();
        let output = cargo_bin_cmd!("yams-wiki")
            .args(["init", "plan", "--request"])
            .arg(&request_path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(output.stdout, b"");
        assert!(!output.stderr.is_empty());
        assert!(!repository.root.join("AGENTS.md").exists());
        assert!(!repository.root.join(".agents").exists());
    }
}

#[test]
fn refused_planning_exits_two_and_preserves_the_target() {
    let repository = Repository::new("dirty target");
    fs::write(repository.root.join("AGENTS.md"), AGENT_POLICY).unwrap();
    let inspection = inspect_cli(&repository.root);
    assert_eq!(inspection.dirty_paths, vec!["AGENTS.md"]);
    let before = fs::read(repository.root.join("AGENTS.md")).unwrap();
    let request_path = repository._temporary.path().join("request.json");
    write_json(&request_path, &request(&inspection, InitMode::Minimal));

    let output = cargo_bin_cmd!("yams-wiki")
        .args(["init", "plan", "--request"])
        .arg(&request_path)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stdout, b"");
    assert!(!output.stderr.is_empty());
    assert_eq!(fs::read(repository.root.join("AGENTS.md")).unwrap(), before);
    assert!(!repository.root.join(".agents").exists());
}

#[test]
fn malformed_unknown_oversized_and_unsupported_manifests_exit_two_without_mutation() {
    let repository = Repository::new("refused apply target");
    let inspection = inspect_cli(&repository.root);
    let request_path = repository._temporary.path().join("request.json");
    write_json(&request_path, &request(&inspection, InitMode::Minimal));
    let valid = plan_cli(&request_path);
    let manifest_path = repository._temporary.path().join("manifest.json");

    let mut unknown = serde_json::to_value(&valid).unwrap();
    unknown["unknown"] = json!(true);
    let mut unsupported = valid.clone();
    unsupported.manifest.manifest_contract = 2;
    unsupported.manifest_sha256 = sha256(&canonical_manifest_bytes(&unsupported.manifest).unwrap());
    let mut tampered = serde_json::to_value(&valid).unwrap();
    tampered["manifest"]["proposal"] = json!("tampered");
    let malformed_cases = [
        b"not json".to_vec(),
        serde_json::to_vec(&unknown).unwrap(),
        vec![b' '; MAX_FILE_BYTES as usize + 1],
    ];

    for bytes in malformed_cases {
        fs::write(&manifest_path, bytes).unwrap();
        let output = cargo_bin_cmd!("yams-wiki")
            .args(["init", "apply", "--manifest"])
            .arg(&manifest_path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(output.stdout, b"");
        assert!(!output.stderr.is_empty());
        assert!(!repository.root.join("AGENTS.md").exists());
        assert!(!repository.root.join(".agents").exists());
    }
    for bytes in [
        serde_json::to_vec(&unsupported).unwrap(),
        serde_json::to_vec(&tampered).unwrap(),
    ] {
        fs::write(&manifest_path, bytes).unwrap();
        let output = cargo_bin_cmd!("yams-wiki")
            .args(["init", "apply", "--manifest"])
            .arg(&manifest_path)
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(2), "{output:?}");
        assert_eq!(output.stderr, b"");
        let result: ApplyResult = one_json_line(&output.stdout);
        assert!(!result.ok);
        assert!(result.error.is_some());
        assert!(!repository.root.join("AGENTS.md").exists());
        assert!(!repository.root.join(".agents").exists());
    }
}

#[test]
fn apply_drift_is_a_machine_readable_exit_two_without_changes() {
    let repository = Repository::new("drift target");
    let inspection = inspect_cli(&repository.root);
    let request_path = repository._temporary.path().join("request.json");
    write_json(&request_path, &request(&inspection, InitMode::Minimal));
    let envelope = plan_cli(&request_path);
    let manifest_path = repository._temporary.path().join("manifest.json");
    write_json(&manifest_path, &envelope);
    fs::write(repository.root.join("AGENTS.md"), "foreign instructions\n").unwrap();

    let output = cargo_bin_cmd!("yams-wiki")
        .args(["init", "apply", "--manifest"])
        .arg(&manifest_path)
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(2));
    assert_eq!(output.stderr, b"");
    let result: ApplyResult = one_json_line(&output.stdout);
    assert!(!result.ok);
    assert!(result.error.unwrap().contains("drift"));
    assert_eq!(
        fs::read_to_string(repository.root.join("AGENTS.md")).unwrap(),
        "foreign instructions\n"
    );
    assert!(!repository.root.join(".agents").exists());
}

#[test]
fn git_and_filesystem_failures_exit_four_and_use_stderr_or_apply_json() {
    let non_git = tempdir().unwrap();
    let inspect = cargo_bin_cmd!("yams-wiki")
        .args(["init", "inspect", "--json"])
        .arg(non_git.path())
        .output()
        .unwrap();
    assert_eq!(inspect.status.code(), Some(4));
    assert_eq!(inspect.stdout, b"");
    assert!(!inspect.stderr.is_empty());

    let repository = Repository::new("git failure target");
    let git_failure = cargo_bin_cmd!("yams-wiki")
        .args(["init", "inspect", "--json"])
        .arg(&repository.root)
        .env("PATH", "")
        .output()
        .unwrap();
    assert_eq!(git_failure.status.code(), Some(4));
    assert_eq!(git_failure.stdout, b"");
    assert!(!git_failure.stderr.is_empty());

    let missing_request = non_git.path().join("missing request.json");
    let plan = cargo_bin_cmd!("yams-wiki")
        .args(["init", "plan", "--request"])
        .arg(&missing_request)
        .output()
        .unwrap();
    assert_eq!(plan.status.code(), Some(4));
    assert_eq!(plan.stdout, b"");
    assert!(!plan.stderr.is_empty());

    let repository = Repository::new("removed apply target");
    let inspection = inspect_cli(&repository.root);
    let request_path = repository._temporary.path().join("request.json");
    write_json(&request_path, &request(&inspection, InitMode::Minimal));
    let envelope = plan_cli(&request_path);
    let manifest_path = repository._temporary.path().join("manifest.json");
    write_json(&manifest_path, &envelope);
    fs::remove_dir_all(&repository.root).unwrap();
    let apply = cargo_bin_cmd!("yams-wiki")
        .args(["init", "apply", "--manifest"])
        .arg(&manifest_path)
        .output()
        .unwrap();
    assert_eq!(apply.status.code(), Some(4), "{apply:?}");
    assert_eq!(apply.stderr, b"");
    let result: ApplyResult = one_json_line(&apply.stdout);
    assert!(!result.ok);
    assert!(result.error.is_some());
}

#[test]
fn apply_root_symlink_loop_is_operational_without_matching_error_text() {
    let repository = Repository::new("symlink loop apply target");
    let inspection = inspect_cli(&repository.root);
    let request_path = repository._temporary.path().join("loop request.json");
    write_json(&request_path, &request(&inspection, InitMode::Minimal));
    let envelope = plan_cli(&request_path);
    let manifest_path = repository._temporary.path().join("loop manifest.json");
    write_json(&manifest_path, &envelope);
    let displaced = repository._temporary.path().join("displaced repository");
    fs::rename(&repository.root, displaced).unwrap();
    std::os::unix::fs::symlink(&repository.root, &repository.root).unwrap();

    let apply = cargo_bin_cmd!("yams-wiki")
        .args(["init", "apply", "--manifest"])
        .arg(&manifest_path)
        .output()
        .unwrap();

    assert_eq!(apply.status.code(), Some(4), "{apply:?}");
    assert_eq!(apply.stderr, b"");
    let result: ApplyResult = one_json_line(&apply.stdout);
    assert!(!result.ok);
    assert!(result.error.is_some());
}

#[test]
fn missing_manifest_file_is_an_operational_error() {
    let directory = tempdir().unwrap();
    let output = cargo_bin_cmd!("yams-wiki")
        .args(["init", "apply", "--manifest"])
        .arg(directory.path().join("missing manifest.json"))
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(4));
    assert_eq!(output.stdout, b"");
    assert!(!output.stderr.is_empty());
}

#[test]
fn init_json_inputs_reject_nonregular_and_symlink_paths_without_blocking() {
    let directory = tempdir().unwrap();
    let regular = directory.path().join("request.json");
    fs::write(&regular, b"{}").unwrap();
    let symlink = directory.path().join("request symlink.json");
    std::os::unix::fs::symlink(&regular, &symlink).unwrap();
    let fifo = directory.path().join("request fifo.json");
    assert!(
        Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap()
            .success()
    );

    for input in [
        directory.path(),
        symlink.as_path(),
        fifo.as_path(),
        Path::new("/dev/null"),
    ] {
        let output = cargo_bin_cmd!("yams-wiki")
            .args(["init", "plan", "--request"])
            .arg(input)
            .timeout(Duration::from_secs(2))
            .output()
            .unwrap();
        assert_eq!(output.status.code(), Some(4), "input {input:?}: {output:?}");
        assert_eq!(output.stdout, b"");
        assert_one_safe_diagnostic(&output.stderr);
    }
}

#[test]
fn init_and_clap_diagnostics_are_single_safe_lines() {
    let directory = tempdir().unwrap();
    let hostile = directory.path().join("missing🙂\n\u{1b}[31mrequest.json");
    let init = cargo_bin_cmd!("yams-wiki")
        .args(["init", "plan", "--request"])
        .arg(&hostile)
        .output()
        .unwrap();
    assert_eq!(init.status.code(), Some(4), "{init:?}");
    assert_eq!(init.stdout, b"");
    assert_one_safe_diagnostic(&init.stderr);

    let clap = cargo_bin_cmd!("yams-wiki")
        .arg("unknown\n\u{1b}[31mcommand")
        .output()
        .unwrap();
    assert_eq!(clap.status.code(), Some(2), "{clap:?}");
    assert_eq!(clap.stdout, b"");
    assert_one_safe_diagnostic(&clap.stderr);
}

#[test]
fn matching_full_apply_preserves_an_existing_runtime_lock() {
    let repository = Repository::new("runtime lock target");
    let inspection = inspect_cli(&repository.root);
    let request_path = repository._temporary.path().join("request.json");
    write_json(&request_path, &request(&inspection, InitMode::Full));
    let envelope = plan_cli(&request_path);
    let manifest_path = repository._temporary.path().join("manifest.json");
    write_json(&manifest_path, &envelope);
    assert!(apply_cli(&manifest_path).ok);
    repository.commit_all();

    let lock = repository.root.join(".agents/memory/.write.lock");
    fs::write(&lock, "pre-existing runtime lock\n").unwrap();
    let before = fs::read(&lock).unwrap();
    let inspection = inspect_cli(&repository.root);
    write_json(&request_path, &request(&inspection, InitMode::Full));
    let envelope = plan_cli(&request_path);
    write_json(&manifest_path, &envelope);

    let result = apply_cli(&manifest_path);
    assert!(result.ok, "{result:?}");
    assert_eq!(fs::read(&lock).unwrap(), before);
}
