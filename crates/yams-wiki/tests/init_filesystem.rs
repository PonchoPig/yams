//! Adversarial public-boundary tests for repository memory initialization.
//!
//! Deterministic races after apply's public preflight require the private seam
//! hooks. They remain covered by the `later_target_drift_recovers_earlier_creates`,
//! `final_validation_failure_recovers_every_mutation`, root/parent/file/directory
//! rebind, and foreign-recovery-drift unit tests in `init::apply::tests`. This
//! integration suite exercises the nearest public behavior without exporting
//! test-only hooks or relying on sleeps and scheduler timing.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Command;

use tempfile::{TempDir, tempdir};
use yams_wiki::{
    AGENT_POLICY, BEGIN_MARKER, CreateRequest, END_MARKER, InitInspection, InitMode,
    InitPlanRequest, LayoutClass, Owner, PageType, ProjectPageRequest, ReindexOptions, SCHEMA,
    apply_manifest, inspect_repository, parse_wiki_page, plan_repository, reindex_wiki,
    render_create, sha256,
};

struct Repository {
    temporary: TempDir,
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
        Self { temporary, root }
    }

    fn commit_all(&self) {
        git(&self.root, &["add", "-A"]);
        git(&self.root, &["commit", "--quiet", "-m", "fixture"]);
    }
}

fn git(root: &Path, args: &[&str]) {
    let path = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let mut command = Command::new("git");
    command
        .env_clear()
        .env("PATH", path)
        .env("HOME", root)
        .env("XDG_CONFIG_HOME", root.join(".test-xdg-config"))
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("LC_ALL", "C")
        .arg("-C")
        .arg(root)
        .args(args);
    let output = command.output().unwrap();
    assert!(
        output.status.success(),
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn project_page() -> ProjectPageRequest {
    ProjectPageRequest {
        title: "Project context".to_owned(),
        page_type: PageType::ProjectState,
        fact: "The project uses approved initialization manifests.".to_owned(),
        why: "Mutations must be reviewable.".to_owned(),
        how_to_apply: "Inspect, plan, approve, and apply.".to_owned(),
        falsified_by: "An unapproved mutation succeeds.".to_owned(),
        summary: "Memory initialization is manifest-driven.".to_owned(),
    }
}

fn request(
    inspection: &InitInspection,
    mode: InitMode,
    agents_md: impl Into<String>,
) -> InitPlanRequest {
    InitPlanRequest {
        root: inspection.root.clone(),
        inspection_sha256: inspection.inspection_sha256.clone(),
        mode,
        date: "2026-08-12".to_owned(),
        agents_md: agents_md.into(),
        project_page: project_page(),
    }
}

fn initialize(repository: &Repository, mode: InitMode, agents_md: impl Into<String>) {
    let inspection = inspect_repository(&repository.root).unwrap();
    let before_candidates = candidate_residue_snapshot(&repository.root);
    let envelope = plan_repository(&request(&inspection, mode, agents_md)).unwrap();
    assert_eq!(
        candidate_residue_snapshot(&repository.root),
        before_candidates
    );
    let result = apply_manifest(&envelope);
    assert!(result.ok, "{result:?}");
    assert!(result.validated, "{result:?}");
    assert_eq!(
        candidate_residue_snapshot(&repository.root),
        before_candidates
    );
}

fn candidate_residue_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<Vec<u8>>> {
    let root = root.canonicalize().unwrap();
    let digest = sha256(root.as_os_str().as_bytes());
    let prefix = format!(".yams-init-candidate-{}-", &digest[..12]);
    let mut bases = BTreeSet::new();
    if let Some(parent) = root.parent().and_then(|parent| parent.canonicalize().ok()) {
        bases.insert(parent);
    }
    for fallback in [Path::new("/tmp"), Path::new("/var/tmp")] {
        if let Ok(canonical) = fallback.canonicalize() {
            bases.insert(canonical);
        }
    }
    bases
        .into_iter()
        .filter_map(|base| {
            let entries = fs::read_dir(&base).ok()?;
            let mut matching = entries
                .filter_map(Result::ok)
                .map(|entry| entry.file_name().as_bytes().to_vec())
                .filter(|name| name.starts_with(prefix.as_bytes()))
                .collect::<Vec<_>>();
            matching.sort();
            Some((base, matching))
        })
        .collect()
}

#[test]
fn fixture_git_commands_ignore_poisoned_parent_environment() {
    const CHILD_MARKER: &str = "YAMS_INIT_FILESYSTEM_GIT_CHILD";
    if std::env::var_os(CHILD_MARKER).is_some() {
        let repository = Repository::new("poisoned git environment");
        initialize(&repository, InitMode::Minimal, AGENT_POLICY);
        repository.commit_all();
        assert_eq!(
            inspect_repository(&repository.root).unwrap().layout,
            LayoutClass::Minimal
        );
        return;
    }

    let trap = tempdir().unwrap();
    let trace = trap.path().join("git-trace.log");
    let trace2 = trap.path().join("git-trace2.json");
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "fixture_git_commands_ignore_poisoned_parent_environment",
            "--nocapture",
        ])
        .env(CHILD_MARKER, "1")
        .env("GIT_DIR", trap.path().join("foreign-git-dir"))
        .env("GIT_WORK_TREE", trap.path().join("foreign-work-tree"))
        .env("GIT_CONFIG_COUNT", "1")
        .env("GIT_CONFIG_KEY_0", "commit.gpgsign")
        .env("GIT_CONFIG_VALUE_0", "true")
        .env("GIT_TRACE", &trace)
        .env("GIT_TRACE2_EVENT", &trace2)
        .output()
        .unwrap();

    assert!(
        child.status.success(),
        "poisoned child failed: {}",
        String::from_utf8_lossy(&child.stderr)
    );
    assert!(!trace.exists(), "fixture Git inherited GIT_TRACE");
    assert!(!trace2.exists(), "fixture Git inherited GIT_TRACE2_EVENT");
}

fn write(root: &Path, relative: &str, bytes: impl AsRef<[u8]>) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, bytes).unwrap();
}

#[derive(Debug, Eq, PartialEq)]
struct Fingerprint {
    mode: u32,
    inode: u64,
    len: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
    bytes: Option<Vec<u8>>,
}

fn fingerprint(path: &Path) -> Fingerprint {
    let metadata = fs::symlink_metadata(path).unwrap();
    Fingerprint {
        mode: metadata.mode() & 0o7777,
        inode: metadata.ino(),
        len: metadata.len(),
        modified_seconds: metadata.mtime(),
        modified_nanoseconds: metadata.mtime_nsec(),
        changed_seconds: metadata.ctime(),
        changed_nanoseconds: metadata.ctime_nsec(),
        bytes: metadata
            .file_type()
            .is_file()
            .then(|| fs::read(path).unwrap()),
    }
}

fn fingerprints(root: &Path, paths: &[&str]) -> BTreeMap<String, Fingerprint> {
    paths
        .iter()
        .map(|relative| ((*relative).to_owned(), fingerprint(&root.join(relative))))
        .collect()
}

fn assert_no_apply_temporaries(root: &Path) {
    let mut pending = vec![root.to_owned()];
    while let Some(directory) = pending.pop() {
        for entry in fs::read_dir(&directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_name() == ".git" {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            assert!(
                !(name.contains(".yams-apply-") || name.contains(".yams-dir-")),
                "temporary initialization residue remained at {}",
                entry.path().display()
            );
            if entry.file_type().unwrap().is_dir() {
                pending.push(entry.path());
            }
        }
    }
}

#[test]
fn absent_to_minimal_preserves_unrelated_agents_and_harness_content() {
    let repository = Repository::new("minimal preservation");
    write(
        &repository.root,
        "AGENTS.md",
        "# Existing instructions\n\nKeep this section byte-for-byte.\n",
    );
    write(
        &repository.root,
        ".agents/harness/state.json",
        br#"{"owner":"another-harness"}\n"#,
    );
    write(
        &repository.root,
        ".claude/settings.json",
        br#"{"permissions":[]}\n"#,
    );
    write(&repository.root, "CLAUDE.md", "# Claude instructions\n");
    repository.commit_all();
    let harness_before = fingerprints(
        &repository.root,
        &[
            ".agents/harness/state.json",
            ".claude/settings.json",
            "CLAUDE.md",
        ],
    );
    let approved_agents =
        format!("# Existing instructions\n\nKeep this section byte-for-byte.\n\n{AGENT_POLICY}");

    initialize(&repository, InitMode::Minimal, approved_agents.clone());

    assert_eq!(
        fs::read_to_string(repository.root.join("AGENTS.md")).unwrap(),
        approved_agents
    );
    assert_eq!(
        inspect_repository(&repository.root).unwrap().layout,
        LayoutClass::Minimal
    );
    assert_eq!(
        fingerprints(
            &repository.root,
            &[
                ".agents/harness/state.json",
                ".claude/settings.json",
                "CLAUDE.md",
            ],
        ),
        harness_before
    );
    assert!(!repository.root.join(".agents/memory/SCHEMA.md").exists());
    assert!(repository.root.join(".agents/memory/.write.lock").exists());
}

#[test]
fn absent_to_full_installs_exact_validated_assets_with_a_runtime_lock() {
    let repository = Repository::new("full installation");

    initialize(&repository, InitMode::Full, AGENT_POLICY);

    let inspection = inspect_repository(&repository.root).unwrap();
    assert!(inspection.ok, "{inspection:?}");
    assert_eq!(inspection.layout, LayoutClass::Full);
    assert!(inspection.conflicts.is_empty(), "{inspection:?}");
    assert_eq!(
        fs::read_to_string(repository.root.join(".agents/memory/SCHEMA.md")).unwrap(),
        SCHEMA
    );
    let page = fs::read_to_string(
        repository
            .root
            .join(".agents/memory/pages/project-context.md"),
    )
    .unwrap();
    let parsed = parse_wiki_page(&page).unwrap();
    assert_eq!(parsed.slug, "project-context");
    assert_eq!(parsed.title, "Project context");
    let index = fs::read_to_string(repository.root.join(".agents/memory/INDEX.md")).unwrap();
    assert!(index.starts_with(BEGIN_MARKER));
    assert!(index.contains("[project-context](pages/project-context.md)"));
    assert!(index.ends_with(&format!("{END_MARKER}\n")));
    assert!(repository.root.join(".agents/memory/.write.lock").exists());
    assert_eq!(
        fs::read_to_string(repository.root.join(".agents/memory/.gitignore")).unwrap(),
        yams_wiki::MEMORY_GITIGNORE
    );
    assert_no_apply_temporaries(&repository.root);
}

#[test]
fn structured_partial_layout_refuses_minimal_without_mutation() {
    let repository = Repository::new("partial refusal");
    write(&repository.root, ".agents/memory/SCHEMA.md", SCHEMA);
    repository.commit_all();
    let before = fingerprints(
        &repository.root,
        &[".agents", ".agents/memory", ".agents/memory/SCHEMA.md"],
    );
    let inspection = inspect_repository(&repository.root).unwrap();
    assert_eq!(inspection.layout, LayoutClass::Partial);
    assert_eq!(inspection.attainable, vec![InitMode::Full]);

    let error =
        plan_repository(&request(&inspection, InitMode::Minimal, AGENT_POLICY)).unwrap_err();

    assert!(error.to_string().contains("not attainable"), "{error}");
    assert_eq!(
        fingerprints(
            &repository.root,
            &[".agents", ".agents/memory", ".agents/memory/SCHEMA.md"],
        ),
        before
    );
    assert!(!repository.root.join("AGENTS.md").exists());
    assert_no_apply_temporaries(&repository.root);
}

#[test]
fn differing_schema_is_a_stable_conflict_and_never_replaced() {
    let repository = Repository::new("schema conflict");
    write(
        &repository.root,
        ".agents/memory/SCHEMA.md",
        "# Foreign schema\n",
    );
    repository.commit_all();
    let before = fingerprints(&repository.root, &[".agents/memory/SCHEMA.md"]);

    let inspection = inspect_repository(&repository.root).unwrap();
    assert!(
        inspection
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "schema-mismatch"),
        "{inspection:?}"
    );
    assert!(plan_repository(&request(&inspection, InitMode::Full, AGENT_POLICY)).is_err());
    assert_eq!(
        fingerprints(&repository.root, &[".agents/memory/SCHEMA.md"]),
        before
    );
}

#[test]
fn symlinked_owned_ancestors_and_boundaries_are_never_followed() {
    for boundary in [".agents", ".agents/memory", ".agents/memory/pages"] {
        let repository = Repository::new(&format!("symlink-{}", boundary.replace('/', "-")));
        let outside = repository.temporary.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("sentinel"), "outside\n").unwrap();
        let target = repository.root.join(boundary);
        fs::create_dir_all(target.parent().unwrap()).unwrap();
        symlink(&outside, &target).unwrap();
        repository.commit_all();

        let inspection = inspect_repository(&repository.root).unwrap();

        assert!(
            inspection
                .conflicts
                .iter()
                .any(|conflict| { conflict.path == boundary && conflict.code.contains("symlink") }),
            "{boundary}: {inspection:?}"
        );
        assert!(plan_repository(&request(&inspection, InitMode::Full, AGENT_POLICY)).is_err());
        assert_eq!(
            fs::read_to_string(outside.join("sentinel")).unwrap(),
            "outside\n"
        );
        assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    }
}

#[test]
fn dirty_memory_is_reported_and_planning_refuses_without_changes() {
    let repository = Repository::new("dirty memory");
    initialize(&repository, InitMode::Minimal, AGENT_POLICY);
    repository.commit_all();
    fs::write(
        repository.root.join(".agents/memory/project-context.md"),
        "concurrent writer\n",
    )
    .unwrap();
    let before = fingerprints(&repository.root, &[".agents/memory/project-context.md"]);

    let inspection = inspect_repository(&repository.root).unwrap();

    assert_eq!(
        inspection.dirty_paths,
        vec![".agents/memory/project-context.md"]
    );
    assert!(plan_repository(&request(&inspection, InitMode::Full, AGENT_POLICY)).is_err());
    assert_eq!(
        fingerprints(&repository.root, &[".agents/memory/project-context.md"]),
        before
    );
}

#[test]
fn post_approval_agents_drift_is_rejected_before_any_write() {
    let repository = Repository::new("agents drift");
    let inspection = inspect_repository(&repository.root).unwrap();
    let envelope = plan_repository(&request(&inspection, InitMode::Minimal, AGENT_POLICY)).unwrap();
    fs::write(
        repository.root.join("AGENTS.md"),
        "foreign concurrent policy\n",
    )
    .unwrap();
    let root_before = fingerprint(&repository.root);

    let result = apply_manifest(&envelope);

    assert!(!result.ok, "{result:?}");
    assert!(!result.validated, "{result:?}");
    assert!(result.created.is_empty(), "{result:?}");
    assert!(result.changed.is_empty(), "{result:?}");
    assert!(result.removed.is_empty(), "{result:?}");
    assert!(result.restored.is_empty(), "{result:?}");
    assert_eq!(
        fs::read_to_string(repository.root.join("AGENTS.md")).unwrap(),
        "foreign concurrent policy\n"
    );
    assert!(!repository.root.join(".agents").exists());
    assert_eq!(fingerprint(&repository.root), root_before);
    assert_no_apply_temporaries(&repository.root);
}

#[test]
fn approved_root_parent_file_and_directory_competitors_are_refused() {
    // Root replacement.
    let repository = Repository::new("root competitor");
    let inspection = inspect_repository(&repository.root).unwrap();
    let envelope = plan_repository(&request(&inspection, InitMode::Minimal, AGENT_POLICY)).unwrap();
    let detached = repository.temporary.path().join("detached-approved-root");
    fs::rename(&repository.root, &detached).unwrap();
    fs::create_dir(&repository.root).unwrap();
    git(&repository.root, &["init", "--quiet"]);
    fs::write(repository.root.join("foreign"), "replacement root\n").unwrap();
    let replacement_inspection = inspect_repository(&repository.root).unwrap();
    assert_ne!(
        replacement_inspection.inspection_sha256,
        inspection.inspection_sha256
    );
    let mut approved_public_shape = inspection.clone();
    approved_public_shape.inspection_sha256.clear();
    let mut replacement_public_shape = replacement_inspection;
    replacement_public_shape.inspection_sha256.clear();
    assert_eq!(replacement_public_shape, approved_public_shape);
    let result = apply_manifest(&envelope);
    assert!(!result.ok, "{result:?}");
    assert_eq!(
        fs::read_to_string(repository.root.join("foreign")).unwrap(),
        "replacement root\n"
    );
    assert!(!detached.join(".agents").exists());

    // Existing owned parent replacement.
    let repository = Repository::new("parent competitor");
    initialize(&repository, InitMode::Minimal, AGENT_POLICY);
    repository.commit_all();
    let inspection = inspect_repository(&repository.root).unwrap();
    let envelope = plan_repository(&request(&inspection, InitMode::Full, AGENT_POLICY)).unwrap();
    let approved_parent = repository.root.join(".agents-approved");
    fs::rename(repository.root.join(".agents"), &approved_parent).unwrap();
    fs::create_dir(repository.root.join(".agents")).unwrap();
    fs::write(
        repository.root.join(".agents/foreign"),
        "replacement parent\n",
    )
    .unwrap();
    let result = apply_manifest(&envelope);
    assert!(!result.ok, "{result:?}");
    assert_eq!(
        fs::read_to_string(repository.root.join(".agents/foreign")).unwrap(),
        "replacement parent\n"
    );
    assert!(approved_parent.join("memory/project-context.md").exists());

    // Missing file and directory names populated after approval.
    let repository = Repository::new("leaf competitors");
    let inspection = inspect_repository(&repository.root).unwrap();
    let envelope = plan_repository(&request(&inspection, InitMode::Minimal, AGENT_POLICY)).unwrap();
    fs::write(repository.root.join("AGENTS.md"), "file competitor\n").unwrap();
    let result = apply_manifest(&envelope);
    assert!(!result.ok, "{result:?}");
    assert!(!repository.root.join(".agents").exists());
    assert_eq!(
        fs::read_to_string(repository.root.join("AGENTS.md")).unwrap(),
        "file competitor\n"
    );

    let repository = Repository::new("directory competitor");
    write(&repository.root, ".agents/unrelated", "retained\n");
    repository.commit_all();
    let inspection = inspect_repository(&repository.root).unwrap();
    let envelope = plan_repository(&request(&inspection, InitMode::Minimal, AGENT_POLICY)).unwrap();
    fs::create_dir(repository.root.join(".agents/memory")).unwrap();
    fs::write(
        repository.root.join(".agents/memory/foreign"),
        "directory competitor\n",
    )
    .unwrap();
    let result = apply_manifest(&envelope);
    assert!(!result.ok, "{result:?}");
    assert_eq!(
        fs::read_to_string(repository.root.join(".agents/memory/foreign")).unwrap(),
        "directory competitor\n"
    );
    assert!(!repository.root.join("AGENTS.md").exists());
}

#[test]
fn unsafe_runtime_lock_blocks_planning_and_is_not_followed() {
    let repository = Repository::new("unsafe runtime lock");
    initialize(&repository, InitMode::Full, AGENT_POLICY);
    repository.commit_all();
    let outside = repository.temporary.path().join("foreign-lock-target");
    fs::write(&outside, "foreign lock bytes\n").unwrap();
    fs::remove_file(repository.root.join(".agents/memory/.write.lock")).unwrap();
    symlink(&outside, repository.root.join(".agents/memory/.write.lock")).unwrap();

    let inspection = inspect_repository(&repository.root).unwrap();

    assert!(
        inspection
            .conflicts
            .iter()
            .any(|conflict| conflict.code == "unsafe-runtime-lock"),
        "{inspection:?}"
    );
    assert!(plan_repository(&request(&inspection, InitMode::Full, AGENT_POLICY)).is_err());
    assert_eq!(fs::read_to_string(outside).unwrap(), "foreign lock bytes\n");
}

#[test]
fn matching_full_rerun_preserves_extra_pages_custom_index_runtime_and_metadata() {
    let repository = Repository::new("full no-op");
    initialize(&repository, InitMode::Full, AGENT_POLICY);
    let extra = render_create(
        &CreateRequest {
            title: "Retained knowledge".to_owned(),
            page_type: PageType::Decision,
            owner: Owner::Shared,
            fact: "This additional page must survive initialization reruns.".to_owned(),
            why: "Full initialization is additive.".to_owned(),
            how_to_apply: "Keep existing canonical pages byte-identical.".to_owned(),
            falsified_by: "A matching rerun rewrites or removes this page.".to_owned(),
            summary: "An additional canonical page is retained.".to_owned(),
            related: Vec::new(),
        },
        "2026-08-12",
    )
    .unwrap();
    write(
        &repository.root,
        ".agents/memory/pages/retained-knowledge.md",
        extra,
    );
    fs::write(
        repository.root.join(".agents/memory/INDEX.md"),
        format!(
            "# Custom memory guide\n\nRetain this preamble.\n\n{BEGIN_MARKER}\n\n{END_MARKER}\n\nRetain this tail.\n"
        ),
    )
    .unwrap();
    reindex_wiki(
        &repository.root.join(".agents/memory"),
        &ReindexOptions::default(),
    )
    .unwrap();
    fs::write(
        repository.root.join(".agents/memory/.write.lock"),
        "persistent runtime state\n",
    )
    .unwrap();
    write(&repository.root, ".agents/harness/untouched", "harness\n");
    repository.commit_all();
    let tracked = [
        "AGENTS.md",
        ".agents",
        ".agents/memory",
        ".agents/memory/SCHEMA.md",
        ".agents/memory/INDEX.md",
        ".agents/memory/pages",
        ".agents/memory/pages/project-context.md",
        ".agents/memory/pages/retained-knowledge.md",
        ".agents/memory/.write.lock",
        ".agents/harness/untouched",
    ];
    let before = fingerprints(&repository.root, &tracked);
    let inspection = inspect_repository(&repository.root).unwrap();
    assert_eq!(inspection.layout, LayoutClass::Full, "{inspection:?}");

    let envelope = plan_repository(&request(&inspection, InitMode::Full, AGENT_POLICY)).unwrap();
    assert!(envelope.manifest.operations.is_empty(), "{envelope:?}");
    let result = apply_manifest(&envelope);

    assert!(result.ok, "{result:?}");
    assert!(result.validated, "{result:?}");
    assert!(result.created.is_empty() && result.changed.is_empty() && result.removed.is_empty());
    assert_eq!(fingerprints(&repository.root, &tracked), before);
    let index = fs::read_to_string(repository.root.join(".agents/memory/INDEX.md")).unwrap();
    assert!(index.starts_with("# Custom memory guide\n\nRetain this preamble.\n"));
    assert!(index.ends_with("\nRetain this tail.\n"));
    assert!(index.contains("[retained-knowledge](pages/retained-knowledge.md)"));
    assert_no_apply_temporaries(&repository.root);
}

#[test]
fn matching_full_rerun_accepts_read_only_owned_files_and_directories() {
    let repository = Repository::new("read only no-op");
    initialize(&repository, InitMode::Full, AGENT_POLICY);
    repository.commit_all();
    let files = [
        "AGENTS.md",
        ".agents/memory/SCHEMA.md",
        ".agents/memory/INDEX.md",
        ".agents/memory/pages/project-context.md",
    ];
    let directories = [".agents/memory/pages", ".agents/memory", ".agents"];
    for relative in files {
        fs::set_permissions(
            repository.root.join(relative),
            fs::Permissions::from_mode(0o444),
        )
        .unwrap();
    }
    for relative in directories {
        fs::set_permissions(
            repository.root.join(relative),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
    }
    let inspection = inspect_repository(&repository.root).unwrap();
    let envelope = plan_repository(&request(&inspection, InitMode::Full, AGENT_POLICY)).unwrap();
    assert!(envelope.manifest.operations.is_empty(), "{envelope:?}");
    let before = fingerprints(
        &repository.root,
        &[
            "AGENTS.md",
            ".agents",
            ".agents/memory",
            ".agents/memory/SCHEMA.md",
            ".agents/memory/INDEX.md",
            ".agents/memory/pages",
            ".agents/memory/pages/project-context.md",
        ],
    );

    let result = apply_manifest(&envelope);

    assert!(result.ok, "{result:?}");
    assert_eq!(
        fingerprints(
            &repository.root,
            &[
                "AGENTS.md",
                ".agents",
                ".agents/memory",
                ".agents/memory/SCHEMA.md",
                ".agents/memory/INDEX.md",
                ".agents/memory/pages",
                ".agents/memory/pages/project-context.md",
            ],
        ),
        before
    );
    for relative in directories.into_iter().rev() {
        fs::set_permissions(
            repository.root.join(relative),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
    }
}
