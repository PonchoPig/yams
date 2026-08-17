#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::{canonical_manifest_bytes, plan_repository};
    use crate::{
        AGENT_POLICY, InitManifest, InitMode, InitPlanRequest, LayoutClass, ManifestEnvelope,
        OperationKind, PageType, ProjectPageRequest, SCHEMA, inspect_repository,
    };

    fn git(root: &Path, args: &[&str]) {
        let status = Command::new("git")
            .args(["-C", root.to_str().unwrap()])
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("HOME", root)
            .status()
            .unwrap();
        assert!(status.success(), "git {args:?} failed");
    }

    fn repository() -> tempfile::TempDir {
        let temporary = tempfile::tempdir().unwrap();
        git(temporary.path(), &["init", "-q"]);
        git(
            temporary.path(),
            &["config", "user.email", "test@example.invalid"],
        );
        git(temporary.path(), &["config", "user.name", "Yams Test"]);
        temporary
    }

    fn commit_all(root: &Path) {
        git(root, &["add", "-A"]);
        git(root, &["commit", "-qm", "fixture"]);
    }

    fn project_page(title: &str) -> ProjectPageRequest {
        ProjectPageRequest {
            title: title.to_owned(),
            page_type: PageType::ProjectState,
            fact: "The fictional project uses manifest-driven memory initialization.".to_owned(),
            why: "The initialization behavior must be deterministic.".to_owned(),
            how_to_apply: "Inspect, approve, and apply the exact manifest.".to_owned(),
            falsified_by: "Initialization mutates files while planning.".to_owned(),
            summary: "Project memory initialization is deterministic.".to_owned(),
        }
    }

    fn from_inspect_page() -> ProjectPageRequest {
        project_page("Project context")
    }

    #[test]
    fn plan_request_from_inspection_binds_root_and_digest() {
        let repository = repository();
        let inspection = inspect_repository(repository.path()).unwrap();
        let request = crate::plan_request_from_inspection(
            &inspection,
            None,
            "2026-08-15".to_owned(),
            from_inspect_page(),
            String::new(),
        )
        .unwrap();
        assert_eq!(request.root, inspection.root);
        assert_eq!(request.inspection_sha256, inspection.inspection_sha256);
        assert_eq!(request.mode, InitMode::Full);
        assert_eq!(request.date, "2026-08-15");
        assert_eq!(request.agents_md, "");
    }

    #[test]
    fn plan_request_from_inspection_requires_mode_when_none_is_recommended() {
        let repository = repository();
        fs::write(
            repository.path().join("AGENTS.md"),
            format!("{AGENT_POLICY}\n{AGENT_POLICY}"),
        )
        .unwrap();
        let inspection = inspect_repository(repository.path()).unwrap();
        assert_eq!(inspection.recommended_mode, None);

        let error = crate::plan_request_from_inspection(
            &inspection,
            None,
            "2026-08-15".to_owned(),
            from_inspect_page(),
            String::new(),
        )
        .unwrap_err();

        assert!(
            error
                .to_string()
                .contains("mode is required when recommended_mode is null"),
            "{error}"
        );
    }

    fn request(root: &Path, mode: InitMode, agents_md: String) -> InitPlanRequest {
        let inspection = inspect_repository(root).unwrap();
        InitPlanRequest {
            root: inspection.root,
            inspection_sha256: inspection.inspection_sha256,
            mode,
            date: "2026-08-12".to_owned(),
            agents_md,
            project_page: project_page("Project context"),
        }
    }

    fn operation_lines(envelope: &ManifestEnvelope) -> Vec<String> {
        envelope
            .manifest
            .proposal
            .lines()
            .map(str::to_owned)
            .collect()
    }

    fn operation_paths(envelope: &ManifestEnvelope) -> Vec<(OperationKind, String)> {
        envelope
            .manifest
            .operations
            .iter()
            .map(|operation| (operation.kind, operation.path.clone()))
            .collect()
    }

    const HOSTILE_TMPDIR_CHILD: &str = "YAMS_PLAN_HOSTILE_TMPDIR_CHILD";
    const HOSTILE_TMPDIR_ROOT: &str = "YAMS_PLAN_HOSTILE_TMPDIR_ROOT";

    fn staged_tree_digest(root: &Path) -> String {
        fn visit(root: &Path, path: &Path, nodes: &mut BTreeMap<String, super::DesiredNode>) {
            let mut entries = fs::read_dir(path)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .collect::<Vec<_>>();
            entries.sort();
            for entry in entries {
                let metadata = fs::symlink_metadata(&entry).unwrap();
                let relative = entry
                    .strip_prefix(root)
                    .unwrap()
                    .to_str()
                    .unwrap()
                    .to_owned();
                let mode = metadata.permissions().mode() & 0o7777;
                if metadata.is_dir() {
                    nodes.insert(relative, super::DesiredNode::Directory { mode });
                    visit(root, &entry, nodes);
                } else {
                    nodes.insert(
                        relative,
                        super::DesiredNode::File {
                            mode,
                            bytes: fs::read(&entry).unwrap(),
                        },
                    );
                }
            }
        }

        let mut nodes = BTreeMap::new();
        visit(root, root, &mut nodes);
        super::owned_candidate_sha256(&nodes)
    }

    fn seed_minimal(root: &Path) {
        fs::create_dir(root.join(".agents")).unwrap();
        fs::create_dir(root.join(".agents/memory")).unwrap();
        fs::write(root.join("AGENTS.md"), AGENT_POLICY).unwrap();
        let initial = request(root, InitMode::Minimal, AGENT_POLICY.to_owned());
        let page = crate::render_create(
            &crate::CreateRequest {
                title: initial.project_page.title,
                page_type: initial.project_page.page_type,
                owner: crate::Owner::Shared,
                fact: initial.project_page.fact,
                why: initial.project_page.why,
                how_to_apply: initial.project_page.how_to_apply,
                falsified_by: initial.project_page.falsified_by,
                summary: initial.project_page.summary,
                related: Vec::new(),
            },
            &initial.date,
        )
        .unwrap();
        fs::write(root.join(".agents/memory/project-context.md"), page).unwrap();
        fs::write(
            root.join(".agents/memory/.gitignore"),
            crate::MEMORY_GITIGNORE,
        )
        .unwrap();
        commit_all(root);
    }

    fn chmod(root: &Path, relative: &str, mode: u32) {
        fs::set_permissions(root.join(relative), fs::Permissions::from_mode(mode)).unwrap();
    }

    fn candidate_siblings(root: &Path) -> Vec<PathBuf> {
        let prefix = super::candidate_prefix(root);
        let mut paths = fs::read_dir(root.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with(&prefix))
            })
            .collect::<Vec<_>>();
        paths.sort();
        paths
    }

    fn remove_test_candidate(path: &Path) {
        fn make_directories_writable(path: &Path) {
            let metadata = fs::symlink_metadata(path).unwrap();
            if !metadata.is_dir() {
                return;
            }
            for entry in fs::read_dir(path).unwrap() {
                make_directories_writable(&entry.unwrap().path());
            }
            fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
        }

        make_directories_writable(path);
        fs::remove_dir_all(path).unwrap();
    }

    fn assert_no_new_candidate_siblings<T>(root: &Path, action: impl FnOnce() -> T) -> T {
        let before = candidate_siblings(root);
        let result = action();
        let after = candidate_siblings(root);
        for leaked in after.iter().filter(|path| !before.contains(path)) {
            remove_test_candidate(leaked);
        }
        assert_eq!(after, before, "candidate staging directory leaked");
        result
    }

    fn seed_full(root: &Path) {
        fs::create_dir(root.join(".agents")).unwrap();
        fs::create_dir(root.join(".agents/memory")).unwrap();
        fs::create_dir(root.join(".agents/memory/pages")).unwrap();
        fs::write(root.join("AGENTS.md"), AGENT_POLICY).unwrap();
        fs::write(root.join(".agents/memory/SCHEMA.md"), SCHEMA).unwrap();
        fs::write(
            root.join(".agents/memory/.gitignore"),
            crate::MEMORY_GITIGNORE,
        )
        .unwrap();
        fs::write(root.join(".agents/memory/INDEX.md"), crate::INDEX_TEMPLATE).unwrap();
        let create = crate::CreateRequest {
            title: "Project context".to_owned(),
            page_type: PageType::ProjectState,
            owner: crate::Owner::Shared,
            fact: "The fictional project uses manifest-driven memory initialization.".to_owned(),
            why: "The initialization behavior must be deterministic.".to_owned(),
            how_to_apply: "Inspect, approve, and apply the exact manifest.".to_owned(),
            falsified_by: "Initialization mutates files while planning.".to_owned(),
            summary: "Project memory initialization is deterministic.".to_owned(),
            related: Vec::new(),
        };
        fs::write(
            root.join(".agents/memory/pages/project-context.md"),
            crate::render_create(&create, "2026-08-12").unwrap(),
        )
        .unwrap();
        crate::reindex_wiki(
            &root.join(".agents/memory"),
            &crate::ReindexOptions::default(),
        )
        .unwrap();
        fs::remove_file(root.join(".agents/memory/.write.lock")).unwrap();
        commit_all(root);
    }

    #[test]
    fn stale_inspection_is_rejected() {
        let repository = repository();
        let request = request(
            repository.path(),
            InitMode::Minimal,
            AGENT_POLICY.to_owned(),
        );
        fs::write(repository.path().join("AGENTS.md"), "# Drift\n").unwrap();

        let error = plan_repository(&request).unwrap_err();

        assert!(matches!(error, crate::InitError::Drift(_)));
    }

    #[test]
    fn absent_to_minimal_has_only_minimal_operations() {
        let repository = repository();
        let request = request(
            repository.path(),
            InitMode::Minimal,
            AGENT_POLICY.to_owned(),
        );

        let envelope = plan_repository(&request).unwrap();

        assert_eq!(
            operation_paths(&envelope),
            vec![
                (OperationKind::CreateDirectory, ".agents".to_owned()),
                (OperationKind::CreateDirectory, ".agents/memory".to_owned()),
                (OperationKind::CreateFile, "AGENTS.md".to_owned()),
                (
                    OperationKind::CreateFile,
                    ".agents/memory/.gitignore".to_owned()
                ),
                (
                    OperationKind::CreateFile,
                    ".agents/memory/project-context.md".to_owned()
                ),
            ]
        );
        assert_eq!(
            operation_lines(&envelope),
            vec![
                "CREATE DIR .agents",
                "CREATE DIR .agents/memory",
                "CREATE FILE AGENTS.md",
                "CREATE FILE .agents/memory/.gitignore",
                "CREATE FILE .agents/memory/project-context.md",
            ]
        );
        assert_eq!(envelope.manifest.mode, InitMode::Minimal);
        assert_eq!(envelope.manifest.layout_version, 1);
        assert_eq!(envelope.manifest.manifest_contract, 1);
        assert_eq!(envelope.manifest.asset_sha256.len(), 5);
    }

    #[test]
    fn omitted_agents_md_installs_canonical_policy_on_an_absent_layout() {
        let repository = repository();
        let inspection = inspect_repository(repository.path()).unwrap();
        let mut request = serde_json::to_value(request(
            repository.path(),
            InitMode::Minimal,
            AGENT_POLICY.to_owned(),
        ))
        .unwrap();
        request
            .as_object_mut()
            .unwrap()
            .remove("agents_md");
        let request: InitPlanRequest = serde_json::from_value(request).unwrap();
        assert_eq!(request.inspection_sha256, inspection.inspection_sha256);

        let envelope = plan_repository(&request).unwrap();

        let agents = envelope
            .manifest
            .operations
            .iter()
            .find(|operation| operation.path == "AGENTS.md")
            .and_then(|operation| operation.content.as_deref());
        assert_eq!(agents, Some(AGENT_POLICY));
    }

    #[test]
    fn omitted_agents_md_keeps_an_existing_file_that_already_has_the_canonical_section() {
        let repository = repository();
        let existing = format!("# Local instructions\n\n{AGENT_POLICY}");
        fs::write(repository.path().join("AGENTS.md"), &existing).unwrap();
        commit_all(repository.path());
        let request = request(repository.path(), InitMode::Minimal, String::new());

        let envelope = plan_repository(&request).unwrap();

        let agents = envelope
            .manifest
            .operations
            .iter()
            .find(|operation| operation.path == "AGENTS.md")
            .and_then(|operation| operation.content.as_deref());
        assert_eq!(agents, None);
        assert_eq!(
            fs::read_to_string(repository.path().join("AGENTS.md")).unwrap(),
            existing
        );
    }

    #[test]
    fn omitted_agents_md_is_rejected_when_existing_file_is_empty() {
        let repository = repository();
        fs::write(repository.path().join("AGENTS.md"), "").unwrap();
        commit_all(repository.path());
        let request = request(repository.path(), InitMode::Minimal, String::new());

        let error = plan_repository(&request).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("omit agents_md only when AGENTS.md is missing"),
            "{error}"
        );
    }

    #[test]
    fn omitted_agents_md_is_rejected_when_existing_file_lacks_the_canonical_section() {
        let repository = repository();
        fs::write(repository.path().join("AGENTS.md"), "local notes\n").unwrap();
        commit_all(repository.path());
        let request = request(repository.path(), InitMode::Minimal, String::new());

        let error = plan_repository(&request).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("omit agents_md only when AGENTS.md is missing"),
            "{error}"
        );
    }

    #[test]
    fn absent_to_full_creates_valid_structured_layout_operations() {
        let repository = repository();
        let request = request(repository.path(), InitMode::Full, AGENT_POLICY.to_owned());

        let envelope = plan_repository(&request).unwrap();

        assert_eq!(
            operation_paths(&envelope),
            vec![
                (OperationKind::CreateDirectory, ".agents".to_owned()),
                (OperationKind::CreateDirectory, ".agents/memory".to_owned()),
                (
                    OperationKind::CreateDirectory,
                    ".agents/memory/pages".to_owned()
                ),
                (OperationKind::CreateFile, "AGENTS.md".to_owned()),
                (
                    OperationKind::CreateFile,
                    ".agents/memory/.gitignore".to_owned()
                ),
                (
                    OperationKind::CreateFile,
                    ".agents/memory/INDEX.md".to_owned()
                ),
                (
                    OperationKind::CreateFile,
                    ".agents/memory/SCHEMA.md".to_owned()
                ),
                (
                    OperationKind::CreateFile,
                    ".agents/memory/pages/project-context.md".to_owned()
                ),
            ]
        );
        let schema = envelope
            .manifest
            .operations
            .iter()
            .find(|operation| operation.path == ".agents/memory/SCHEMA.md")
            .unwrap();
        assert_eq!(schema.content.as_deref(), Some(SCHEMA));
        let gitignore = envelope
            .manifest
            .operations
            .iter()
            .find(|operation| operation.path == ".agents/memory/.gitignore")
            .unwrap();
        assert_eq!(gitignore.content.as_deref(), Some(crate::MEMORY_GITIGNORE));
        let index = envelope
            .manifest
            .operations
            .iter()
            .find(|operation| operation.path == ".agents/memory/INDEX.md")
            .unwrap();
        let page = envelope
            .manifest
            .operations
            .iter()
            .find(|operation| operation.path == ".agents/memory/pages/project-context.md")
            .unwrap()
            .content
            .as_deref()
            .unwrap();
        let expected_index = crate::rebuild_index(
            crate::INDEX_TEMPLATE,
            &[crate::parse_index_page("project-context.md", page).unwrap()],
        )
        .unwrap();
        assert_eq!(index.content.as_deref(), Some(expected_index.as_str()));
    }

    #[test]
    fn legacy_v1_layout_without_memory_gitignore_plans_additive_creation() {
        fn assert_upgrade(
            seed: fn(&Path),
            mode: InitMode,
            expected_layout: LayoutClass,
        ) {
            let repository = repository();
            seed(repository.path());
            fs::remove_file(repository.path().join(".agents/memory/.gitignore")).unwrap();
            commit_all(repository.path());

            let inspection = inspect_repository(repository.path()).unwrap();
            assert_eq!(inspection.layout, expected_layout);
            assert!(inspection.conflicts.is_empty());

            let request = request(repository.path(), mode, AGENT_POLICY.to_owned());
            let envelope = plan_repository(&request).unwrap();

            assert_eq!(
                operation_paths(&envelope),
                [(
                    OperationKind::CreateFile,
                    ".agents/memory/.gitignore".to_owned(),
                )]
            );
            assert_eq!(
                envelope.manifest.operations[0].content.as_deref(),
                Some(crate::MEMORY_GITIGNORE)
            );
        }

        assert_upgrade(seed_minimal, InitMode::Minimal, LayoutClass::Minimal);
        assert_upgrade(seed_full, InitMode::Full, LayoutClass::Full);
    }

    #[test]
    fn matching_full_plan_preserves_custom_index_bytes_outside_generated_markers() {
        let repository = repository();
        seed_full(repository.path());
        let custom_index = format!(
            "# Fictional memory guide\n\nKeep this preamble byte-for-byte.\n\n{}\n\n{}\n\n## Curated tail\n\nKeep this tail too.\n",
            crate::BEGIN_MARKER,
            crate::END_MARKER,
        );
        fs::write(
            repository.path().join(".agents/memory/INDEX.md"),
            custom_index,
        )
        .unwrap();
        crate::reindex_wiki(
            &repository.path().join(".agents/memory"),
            &crate::ReindexOptions::default(),
        )
        .unwrap();
        fs::remove_file(repository.path().join(".agents/memory/.write.lock")).unwrap();
        commit_all(repository.path());
        let retained = fs::read(repository.path().join(".agents/memory/INDEX.md")).unwrap();
        let request = request(repository.path(), InitMode::Full, AGENT_POLICY.to_owned());

        let envelope = plan_repository(&request).unwrap();

        assert!(envelope.manifest.operations.is_empty());
        assert_eq!(
            fs::read(repository.path().join(".agents/memory/INDEX.md")).unwrap(),
            retained
        );
    }

    #[test]
    fn persistent_runtime_lock_is_exempt_and_matching_full_plan_is_a_noop() {
        let repository = repository();
        seed_full(repository.path());
        crate::reindex_wiki(
            &repository.path().join(".agents/memory"),
            &crate::ReindexOptions::default(),
        )
        .unwrap();
        assert!(
            repository
                .path()
                .join(".agents/memory/.write.lock")
                .exists()
        );

        let inspection = inspect_repository(repository.path()).unwrap();
        assert_eq!(inspection.layout, LayoutClass::Full);
        assert!(inspection.dirty_paths.is_empty());
        assert!(inspection.conflicts.is_empty());
        let request = InitPlanRequest {
            root: inspection.root,
            inspection_sha256: inspection.inspection_sha256,
            mode: InitMode::Full,
            date: "2026-08-12".to_owned(),
            agents_md: AGENT_POLICY.to_owned(),
            project_page: project_page("Project context"),
        };

        let envelope = plan_repository(&request).unwrap();

        assert!(envelope.manifest.operations.is_empty());
        assert!(
            repository
                .path()
                .join(".agents/memory/.write.lock")
                .exists()
        );
    }

    #[test]
    fn owned_candidate_digest_excludes_runtime_and_unrelated_nodes_but_binds_owned_state() {
        let mut nodes = BTreeMap::from([
            (
                ".agents".to_owned(),
                super::DesiredNode::Directory { mode: 0o755 },
            ),
            (
                ".agents/memory/pages/project-context.md".to_owned(),
                super::DesiredNode::File {
                    mode: 0o644,
                    bytes: b"project context".to_vec(),
                },
            ),
        ]);
        let baseline = super::owned_candidate_sha256(&nodes);

        nodes.insert(
            ".agents/memory/.write.lock".to_owned(),
            super::DesiredNode::File {
                mode: 0o600,
                bytes: Vec::new(),
            },
        );
        nodes.insert(
            "README.md".to_owned(),
            super::DesiredNode::File {
                mode: 0o644,
                bytes: b"unrelated".to_vec(),
            },
        );
        assert_eq!(super::owned_candidate_sha256(&nodes), baseline);

        nodes.insert(
            ".agents/memory/pages/retained.md".to_owned(),
            super::DesiredNode::File {
                mode: 0o444,
                bytes: b"retained one".to_vec(),
            },
        );
        let retained = super::owned_candidate_sha256(&nodes);
        assert_ne!(retained, baseline);
        nodes.insert(
            ".agents/memory/pages/retained.md".to_owned(),
            super::DesiredNode::File {
                mode: 0o440,
                bytes: b"retained one".to_vec(),
            },
        );
        let changed_mode = super::owned_candidate_sha256(&nodes);
        assert_ne!(changed_mode, retained);
        nodes.insert(
            ".agents/memory/pages/retained.md".to_owned(),
            super::DesiredNode::File {
                mode: 0o440,
                bytes: b"retained two".to_vec(),
            },
        );
        assert_ne!(super::owned_candidate_sha256(&nodes), changed_mode);
    }

    #[test]
    fn hostile_in_root_tmpdir_never_changes_the_target_or_leaves_candidate_debris() {
        if std::env::var_os(HOSTILE_TMPDIR_CHILD).is_some() {
            let root = PathBuf::from(std::env::var_os(HOSTILE_TMPDIR_ROOT).unwrap());
            let request = request(&root, InitMode::Full, AGENT_POLICY.to_owned());
            plan_repository(&request).unwrap();
            let mut invalid = request;
            invalid.project_page.fact = "An out-of-profile ![[embed]] is refused.".to_owned();
            assert!(matches!(
                plan_repository(&invalid),
                Err(crate::InitError::Candidate(_))
            ));
            return;
        }

        let repository = repository();
        fs::write(repository.path().join("README.md"), "unrelated\n").unwrap();
        commit_all(repository.path());
        let before = snapshot_owned(repository.path());
        let prefix = super::candidate_prefix(repository.path());
        let sibling_candidates = || {
            fs::read_dir(repository.path().parent().unwrap())
                .unwrap()
                .map(|entry| entry.unwrap().file_name())
                .filter(|name| name.to_string_lossy().starts_with(&prefix))
                .collect::<Vec<_>>()
        };
        let candidates_before = sibling_candidates();

        let status = Command::new(std::env::current_exe().unwrap())
            .arg("--exact")
            .arg("init::plan::tests::hostile_in_root_tmpdir_never_changes_the_target_or_leaves_candidate_debris")
            .arg("--nocapture")
            .env(HOSTILE_TMPDIR_CHILD, "1")
            .env(HOSTILE_TMPDIR_ROOT, repository.path())
            .env("TMPDIR", repository.path())
            .status()
            .unwrap();

        assert!(status.success());
        assert_eq!(snapshot_owned(repository.path()), before);
        assert_eq!(sibling_candidates(), candidates_before);
        assert!(fs::read_dir(repository.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains("yams-init-candidate")
        }));
    }

    #[test]
    fn candidate_staging_path_is_canonical_and_outside_the_target() {
        let repository = repository();

        let candidate = super::create_candidate_dir(repository.path()).unwrap();
        let candidate_path = candidate.path().canonicalize().unwrap();

        assert!(!candidate_path.starts_with(repository.path().canonicalize().unwrap()));
        let closed: Result<(), crate::InitError> = candidate.close();
        closed.unwrap();
        assert!(!candidate_path.exists());
    }

    #[test]
    fn candidate_drop_during_unwind_cleans_read_only_dirs_without_following_symlinks() {
        let repository = repository();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("marker"), "outside\n").unwrap();
        let before = candidate_siblings(repository.path());

        let unwind = std::panic::catch_unwind(|| {
            let mut candidate = super::create_candidate_dir(repository.path()).unwrap();
            fs::create_dir(candidate.path().join("read-only")).unwrap();
            fs::write(candidate.path().join("read-only/file"), "candidate\n").unwrap();
            symlink(outside.path(), candidate.path().join("read-only/outside")).unwrap();
            candidate.capture_owned_tree().unwrap();
            fs::set_permissions(
                candidate.path().join("read-only"),
                fs::Permissions::from_mode(0o555),
            )
            .unwrap();
            panic!("exercise candidate drop during unwind");
        });

        assert!(unwind.is_err());
        assert_eq!(candidate_siblings(repository.path()), before);
        assert_eq!(
            fs::read(outside.path().join("marker")).unwrap(),
            b"outside\n"
        );
    }

    #[test]
    fn explicit_candidate_close_reports_cleanup_failure() {
        let repository = repository();
        let candidate = super::create_candidate_dir(repository.path()).unwrap();
        let candidate_path = candidate.path().to_path_buf();
        fs::remove_dir(&candidate_path).unwrap();
        fs::write(&candidate_path, "not a candidate directory\n").unwrap();

        let error = candidate.close().unwrap_err();

        assert!(error.to_string().contains("candidate cleanup"));
        if candidate_path.exists() {
            fs::remove_file(candidate_path).unwrap();
        }
    }

    #[derive(Clone, Copy)]
    enum CleanupRacePoint {
        BeforeChildOpen,
        AfterChildOpen,
    }

    struct RebindCleanupChild {
        point: CleanupRacePoint,
        candidate_child: PathBuf,
        displaced_child: PathBuf,
        fired: bool,
    }

    impl RebindCleanupChild {
        fn swap(&mut self, name: &std::ffi::OsStr) {
            if self.fired || name != "read-only" {
                return;
            }
            fs::rename(&self.candidate_child, &self.displaced_child).unwrap();
            fs::set_permissions(&self.displaced_child, fs::Permissions::from_mode(0o555)).unwrap();
            fs::create_dir(&self.candidate_child).unwrap();
            fs::write(self.candidate_child.join("foreign"), "foreign child\n").unwrap();
            fs::set_permissions(&self.candidate_child, fs::Permissions::from_mode(0o555)).unwrap();
            self.fired = true;
        }
    }

    impl super::CandidateCleanupHooks for RebindCleanupChild {
        fn before_child_open(&mut self, relative: &Path) {
            if matches!(self.point, CleanupRacePoint::BeforeChildOpen) {
                self.swap(relative.as_os_str());
            }
        }

        fn after_child_opened(&mut self, relative: &Path) {
            if matches!(self.point, CleanupRacePoint::AfterChildOpen) {
                self.swap(relative.as_os_str());
            }
        }
    }

    fn exercise_cleanup_child_rebinding(point: CleanupRacePoint) -> Result<(), crate::InitError> {
        let repository = repository();
        let outside = tempfile::tempdir().unwrap();
        let sentinel = outside.path().join("sentinel");
        fs::create_dir(&sentinel).unwrap();
        fs::write(sentinel.join("marker"), "outside\n").unwrap();
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o555)).unwrap();
        let sentinel_before = snapshot_owned(&sentinel);

        let mut candidate = super::create_candidate_dir(repository.path()).unwrap();
        let candidate_path = candidate.path().to_path_buf();
        let candidate_child = candidate_path.join("read-only");
        fs::create_dir(&candidate_child).unwrap();
        fs::write(candidate_child.join("file"), "candidate\n").unwrap();
        fs::set_permissions(&candidate_child, fs::Permissions::from_mode(0o755)).unwrap();
        candidate.capture_owned_tree().unwrap();
        let displaced_child = outside.path().join("displaced");
        let mut hooks = RebindCleanupChild {
            point,
            candidate_child,
            displaced_child: displaced_child.clone(),
            fired: false,
        };
        let result = candidate.close_with_hooks(&mut hooks);

        assert!(hooks.fired);
        assert!(candidate_path.exists());
        assert_eq!(snapshot_owned(&sentinel), sentinel_before);
        assert_eq!(
            fs::read(candidate_path.join("read-only/foreign")).unwrap(),
            b"foreign child\n"
        );
        assert_eq!(
            fs::symlink_metadata(candidate_path.join("read-only"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o555
        );
        assert_eq!(
            fs::symlink_metadata(&displaced_child)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o555
        );
        fs::set_permissions(
            candidate_path.join("read-only"),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        fs::remove_file(candidate_path.join("read-only/foreign")).unwrap();
        fs::remove_dir(candidate_path.join("read-only")).unwrap();
        fs::remove_dir(candidate_path).unwrap();
        fs::set_permissions(&displaced_child, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&sentinel, fs::Permissions::from_mode(0o755)).unwrap();
        result
    }

    #[test]
    fn cleanup_refuses_a_child_directory_replacement_before_open() {
        let error =
            exercise_cleanup_child_rebinding(CleanupRacePoint::BeforeChildOpen).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("candidate cleanup binding changed")
        );
    }

    #[test]
    fn cleanup_refuses_a_child_directory_replacement_after_open_without_chmodding_it() {
        let error = exercise_cleanup_child_rebinding(CleanupRacePoint::AfterChildOpen).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("candidate cleanup binding changed")
        );
    }

    struct ReplaceCleanupRoot {
        candidate: PathBuf,
        displaced: PathBuf,
    }

    impl super::CandidateCleanupHooks for ReplaceCleanupRoot {
        fn before_root_check(&mut self) {
            fs::rename(&self.candidate, &self.displaced).unwrap();
            fs::create_dir(&self.candidate).unwrap();
            fs::write(self.candidate.join("foreign"), "foreign root\n").unwrap();
            fs::set_permissions(&self.candidate, fs::Permissions::from_mode(0o555)).unwrap();
        }
    }

    #[test]
    fn cleanup_never_chmods_or_removes_a_replacement_root_directory() {
        let repository = repository();
        let outside = tempfile::tempdir().unwrap();
        let candidate = super::create_candidate_dir(repository.path()).unwrap();
        let candidate_path = candidate.path().to_path_buf();
        let displaced = outside.path().join("displaced-root");
        let mut hooks = ReplaceCleanupRoot {
            candidate: candidate_path.clone(),
            displaced,
        };

        let error = candidate.close_with_hooks(&mut hooks).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("candidate cleanup root binding changed")
        );
        assert_eq!(
            fs::read(candidate_path.join("foreign")).unwrap(),
            b"foreign root\n"
        );
        assert_eq!(
            fs::symlink_metadata(&candidate_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o555
        );
        fs::set_permissions(&candidate_path, fs::Permissions::from_mode(0o755)).unwrap();
        fs::remove_file(candidate_path.join("foreign")).unwrap();
        fs::remove_dir(candidate_path).unwrap();
    }

    struct ReplaceCleanupFile {
        candidate_file: PathBuf,
        displaced_file: PathBuf,
    }

    impl super::CandidateCleanupHooks for ReplaceCleanupFile {
        fn before_leaf_remove(&mut self, relative: &Path) {
            if relative != Path::new("owned") {
                return;
            }
            fs::rename(&self.candidate_file, &self.displaced_file).unwrap();
            fs::write(&self.candidate_file, "foreign file\n").unwrap();
            fs::set_permissions(&self.candidate_file, fs::Permissions::from_mode(0o444)).unwrap();
        }
    }

    #[test]
    fn cleanup_never_removes_a_replacement_file() {
        let repository = repository();
        let outside = tempfile::tempdir().unwrap();
        let mut candidate = super::create_candidate_dir(repository.path()).unwrap();
        let candidate_path = candidate.path().to_path_buf();
        let candidate_file = candidate_path.join("owned");
        fs::write(&candidate_file, "owned file\n").unwrap();
        candidate.capture_owned_tree().unwrap();
        let displaced_file = outside.path().join("displaced-file");
        let mut hooks = ReplaceCleanupFile {
            candidate_file: candidate_file.clone(),
            displaced_file,
        };

        let error = candidate.close_with_hooks(&mut hooks).unwrap_err();

        assert!(
            error
                .to_string()
                .contains("candidate cleanup binding changed")
        );
        assert_eq!(fs::read(&candidate_file).unwrap(), b"foreign file\n");
        assert_eq!(
            fs::symlink_metadata(&candidate_file)
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o444
        );
        fs::remove_file(candidate_file).unwrap();
        fs::remove_dir(candidate_path).unwrap();
    }

    struct RebindCandidateBase {
        base: PathBuf,
        displaced: PathBuf,
        replacement_marker: PathBuf,
    }

    impl super::CandidateCreationHooks for RebindCandidateBase {
        fn after_base_opened(&mut self, base: &Path) {
            assert_eq!(base, self.base);
            fs::rename(&self.base, &self.displaced).unwrap();
            fs::create_dir(&self.base).unwrap();
            fs::write(&self.replacement_marker, "foreign base\n").unwrap();
        }
    }

    #[test]
    fn candidate_creation_uses_the_pinned_base_and_aborts_cleanly_if_its_path_rebinds() {
        let sandbox = tempfile::tempdir().unwrap();
        let base = sandbox.path().join("base");
        fs::create_dir(&base).unwrap();
        let base = base.canonicalize().unwrap();
        let repository = base.join("repository");
        fs::create_dir(&repository).unwrap();
        git(&repository, &["init", "-q"]);
        let displaced = sandbox.path().join("original-base");
        let replacement_marker = base.join("foreign");
        let mut hooks = RebindCandidateBase {
            base: base.clone(),
            displaced: displaced.clone(),
            replacement_marker: replacement_marker.clone(),
        };

        let error = super::create_candidate_dir_with_hooks(&repository, &mut hooks).unwrap_err();

        assert!(error.to_string().contains("staging base binding changed"));
        assert_eq!(fs::read(&replacement_marker).unwrap(), b"foreign base\n");
        assert!(fs::read_dir(&displaced).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(&super::candidate_prefix(&repository))
        }));
    }

    struct CollideOnce {
        attempts: usize,
    }

    impl super::CandidateCreationHooks for CollideOnce {
        fn mkdir_candidate(
            &mut self,
            base: std::os::fd::BorrowedFd<'_>,
            name: &std::ffi::OsStr,
        ) -> Result<(), rustix::io::Errno> {
            self.attempts += 1;
            if self.attempts == 1 {
                Err(rustix::io::Errno::EXIST)
            } else {
                rustix::fs::mkdirat(base, name, rustix::fs::Mode::RWXU)
            }
        }
    }

    #[test]
    fn candidate_creation_retries_an_exclusive_name_collision() {
        let repository = repository();
        let mut hooks = CollideOnce { attempts: 0 };

        let candidate =
            super::create_candidate_dir_with_hooks(repository.path(), &mut hooks).unwrap();

        assert_eq!(hooks.attempts, 2);
        candidate.close().unwrap();
    }

    struct RejectAndFailRollback;

    impl super::CandidateCreationHooks for RejectAndFailRollback {
        fn after_candidate_pinned(&mut self) -> Result<(), rustix::io::Errno> {
            Err(rustix::io::Errno::IO)
        }

        fn remove_created(
            &mut self,
            _base: std::os::fd::BorrowedFd<'_>,
            _name: &std::ffi::OsStr,
        ) -> Result<(), rustix::io::Errno> {
            Err(rustix::io::Errno::PERM)
        }
    }

    #[test]
    fn candidate_creation_reports_rollback_failure_and_unresolved_residue() {
        let repository = repository();
        let before = candidate_siblings(repository.path());
        let mut hooks = RejectAndFailRollback;

        let error =
            super::create_candidate_dir_with_hooks(repository.path(), &mut hooks).unwrap_err();

        assert!(error.to_string().contains("rollback also failed"));
        let after = candidate_siblings(repository.path());
        let leaked = after
            .iter()
            .find(|path| !before.contains(path))
            .expect("injected cleanup failure leaves explicit residue");
        remove_test_candidate(leaked);
    }

    #[test]
    fn minimal_to_full_creates_structured_children_before_removing_flat_page() {
        let repository = repository();
        seed_minimal(repository.path());
        let request = request(repository.path(), InitMode::Full, AGENT_POLICY.to_owned());

        let envelope = plan_repository(&request).unwrap();
        let paths = operation_paths(&envelope);

        assert_eq!(
            paths.last(),
            Some(&(
                OperationKind::RemoveFile,
                ".agents/memory/project-context.md".to_owned()
            ))
        );
        assert!(paths.contains(&(
            OperationKind::CreateFile,
            ".agents/memory/pages/project-context.md".to_owned()
        )));
    }

    #[test]
    fn invalid_or_noncanonical_policy_and_project_page_are_rejected() {
        let repository = repository();
        let mut invalid_policy = request(
            repository.path(),
            InitMode::Minimal,
            "## Project memory\n\n- Improvise.\n".to_owned(),
        );
        assert!(matches!(
            plan_repository(&invalid_policy),
            Err(crate::InitError::InvalidRequest(_))
        ));

        invalid_policy.agents_md = AGENT_POLICY.to_owned();
        invalid_policy.project_page = project_page("Another context");
        assert!(matches!(
            plan_repository(&invalid_policy),
            Err(crate::InitError::InvalidRequest(_))
        ));

        invalid_policy.project_page = project_page("Project context");
        invalid_policy.date = "2026-8-12".to_owned();
        assert!(matches!(
            plan_repository(&invalid_policy),
            Err(crate::InitError::InvalidRequest(_))
        ));
    }

    #[test]
    fn dirty_owned_paths_and_unattainable_transitions_are_rejected() {
        let repository = repository();
        fs::write(repository.path().join("AGENTS.md"), AGENT_POLICY).unwrap();
        let dirty = request(
            repository.path(),
            InitMode::Minimal,
            AGENT_POLICY.to_owned(),
        );
        assert!(matches!(
            plan_repository(&dirty),
            Err(crate::InitError::Conflict(_))
        ));

        fs::remove_file(repository.path().join("AGENTS.md")).unwrap();
        fs::create_dir(repository.path().join(".agents")).unwrap();
        fs::create_dir(repository.path().join(".agents/memory")).unwrap();
        fs::write(repository.path().join(".agents/memory/SCHEMA.md"), SCHEMA).unwrap();
        commit_all(repository.path());
        let inspection = inspect_repository(repository.path()).unwrap();
        assert_eq!(inspection.layout, LayoutClass::Partial);
        assert_eq!(inspection.attainable, vec![InitMode::Full]);
        let unattainable = InitPlanRequest {
            root: inspection.root,
            inspection_sha256: inspection.inspection_sha256,
            mode: InitMode::Minimal,
            date: "2026-08-12".to_owned(),
            agents_md: AGENT_POLICY.to_owned(),
            project_page: project_page("Project context"),
        };
        assert!(matches!(
            plan_repository(&unattainable),
            Err(crate::InitError::Conflict(_))
        ));
    }

    #[test]
    fn operation_sorting_creates_parents_first_and_removes_children_first() {
        use crate::{InitOperation, NodeKind, NodePrestate};

        let missing = |path: &str| NodePrestate {
            path: path.to_owned(),
            kind: NodeKind::Missing,
            mode: None,
            sha256: None,
            entries_sha256: None,
        };
        let present = |path: &str| NodePrestate {
            path: path.to_owned(),
            kind: NodeKind::File,
            mode: Some(0o644),
            sha256: Some("before".to_owned()),
            entries_sha256: None,
        };
        let mut operations = vec![
            InitOperation {
                kind: OperationKind::RemoveFile,
                path: "parent".to_owned(),
                prestate: present("parent"),
                mode: None,
                content: None,
                post_sha256: None,
            },
            InitOperation {
                kind: OperationKind::CreateDirectory,
                path: "a/b".to_owned(),
                prestate: missing("a/b"),
                mode: Some(0o755),
                content: None,
                post_sha256: None,
            },
            InitOperation {
                kind: OperationKind::RemoveFile,
                path: "parent/child".to_owned(),
                prestate: present("parent/child"),
                mode: None,
                content: None,
                post_sha256: None,
            },
            InitOperation {
                kind: OperationKind::CreateDirectory,
                path: "a".to_owned(),
                prestate: missing("a"),
                mode: Some(0o755),
                content: None,
                post_sha256: None,
            },
        ];

        super::sort_operations(&mut operations);

        assert_eq!(
            operations
                .iter()
                .map(|op| op.path.as_str())
                .collect::<Vec<_>>(),
            vec!["a", "a/b", "parent/child", "parent"]
        );
    }

    #[test]
    fn repeat_planning_is_byte_identical_and_manifest_hashes_only_the_manifest() {
        let repository = repository();
        let request = request(repository.path(), InitMode::Full, AGENT_POLICY.to_owned());

        let first = plan_repository(&request).unwrap();
        let second = plan_repository(&request).unwrap();

        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&second).unwrap()
        );
        assert_eq!(
            first.manifest_sha256,
            crate::sha256(&canonical_manifest_bytes(&first.manifest).unwrap())
        );
        let envelope_bytes = serde_json::to_vec(&first).unwrap();
        assert_ne!(first.manifest_sha256, crate::sha256(&envelope_bytes));
    }

    #[test]
    fn canonical_manifest_bytes_ignore_map_insertion_order() {
        let repository = repository();
        let request = request(
            repository.path(),
            InitMode::Minimal,
            AGENT_POLICY.to_owned(),
        );
        let envelope = plan_repository(&request).unwrap();
        let mut left: InitManifest = envelope.manifest.clone();
        let mut right: InitManifest = envelope.manifest;
        left.asset_sha256 = [
            ("z".to_owned(), "last".to_owned()),
            ("a".to_owned(), "first".to_owned()),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>();
        right.asset_sha256 = [
            ("a".to_owned(), "first".to_owned()),
            ("z".to_owned(), "last".to_owned()),
        ]
        .into_iter()
        .collect::<BTreeMap<_, _>>();

        assert_eq!(
            canonical_manifest_bytes(&left).unwrap(),
            canonical_manifest_bytes(&right).unwrap()
        );
    }

    #[derive(Debug, Eq, PartialEq)]
    struct MetadataSnapshot {
        path: PathBuf,
        mode: u32,
        len: u64,
        modified_ns: i128,
        inode: u64,
        bytes: Option<Vec<u8>>,
    }

    fn snapshot_owned(root: &Path) -> Vec<MetadataSnapshot> {
        fn visit(root: &Path, path: &Path, snapshots: &mut Vec<MetadataSnapshot>) {
            let metadata = fs::symlink_metadata(path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            let bytes = metadata.is_file().then(|| fs::read(path).unwrap());
            snapshots.push(MetadataSnapshot {
                path: relative,
                mode: metadata.permissions().mode(),
                len: metadata.len(),
                modified_ns: i128::from(metadata.mtime()) * 1_000_000_000
                    + i128::from(metadata.mtime_nsec()),
                inode: metadata.ino(),
                bytes,
            });
            if metadata.is_dir() {
                let mut children = fs::read_dir(path)
                    .unwrap()
                    .map(|entry| entry.unwrap().path())
                    .filter(|child| child.file_name().unwrap() != ".git")
                    .collect::<Vec<_>>();
                children.sort();
                for child in children {
                    visit(root, &child, snapshots);
                }
            }
        }

        let mut snapshots = Vec::new();
        visit(root, root, &mut snapshots);
        snapshots
    }

    #[test]
    fn planning_does_not_change_target_bytes_or_metadata() {
        let repository = repository();
        fs::write(repository.path().join("README.md"), "unrelated\n").unwrap();
        commit_all(repository.path());
        let request = request(repository.path(), InitMode::Full, AGENT_POLICY.to_owned());
        let before = snapshot_owned(repository.path());

        let _ = plan_repository(&request).unwrap();

        assert_eq!(snapshot_owned(repository.path()), before);
    }

    #[test]
    fn full_candidate_validation_leaves_no_lock_or_unmanifested_nodes() {
        let repository = repository();
        let request = request(repository.path(), InitMode::Full, AGENT_POLICY.to_owned());
        let snapshot = super::capture_repository(Path::new(&request.root)).unwrap();
        let page = super::render_project_page(&request).unwrap();
        let mut desired =
            super::desired_candidate(&request, &snapshot.contents, &snapshot.inspection, page);
        super::canonicalize_full_index(&mut desired).unwrap();
        let mut candidate = super::create_candidate_dir(repository.path()).unwrap();
        super::stage_candidate(&mut candidate, &desired).unwrap();

        super::validate_candidate(candidate.path(), InitMode::Full).unwrap();

        assert!(!candidate.path().join(".agents/memory/.write.lock").exists());
        super::verify_candidate(candidate.path(), &desired).unwrap();
        assert_eq!(
            super::owned_candidate_sha256(&desired),
            staged_tree_digest(candidate.path())
        );
    }

    #[test]
    fn retained_read_only_tree_plans_full_noop_with_exact_final_modes() {
        let repository = repository();
        seed_full(repository.path());
        for path in [
            "AGENTS.md",
            ".agents/memory/SCHEMA.md",
            ".agents/memory/INDEX.md",
        ] {
            chmod(repository.path(), path, 0o444);
        }
        chmod(
            repository.path(),
            ".agents/memory/pages/project-context.md",
            0o444,
        );
        for path in [".agents/memory/pages", ".agents/memory", ".agents"] {
            chmod(repository.path(), path, 0o555);
        }
        let request = request(repository.path(), InitMode::Full, AGENT_POLICY.to_owned());

        let first = plan_repository(&request).unwrap();
        let second = plan_repository(&request).unwrap();

        assert!(first.manifest.operations.is_empty());
        assert_eq!(first, second);
    }

    #[test]
    fn read_only_minimal_tree_can_upgrade_and_replace_retained_files() {
        let repository = repository();
        seed_minimal(repository.path());
        chmod(repository.path(), "AGENTS.md", 0o444);
        chmod(
            repository.path(),
            ".agents/memory/project-context.md",
            0o444,
        );
        for path in [".agents/memory", ".agents"] {
            chmod(repository.path(), path, 0o555);
        }
        let mut replacement = request(
            repository.path(),
            InitMode::Minimal,
            AGENT_POLICY.to_owned(),
        );
        replacement.project_page.fact =
            "The fictional project replaced its retained read-only context.".to_owned();

        let replaced = plan_repository(&replacement).unwrap();
        let replace_operation = replaced
            .manifest
            .operations
            .iter()
            .find(|operation| operation.path == ".agents/memory/project-context.md")
            .unwrap();
        assert_eq!(replace_operation.kind, OperationKind::ReplaceFile);
        assert_eq!(replace_operation.prestate.mode, Some(0o444));
        assert_eq!(replace_operation.mode, Some(0o444));

        let mut upgrade = replacement;
        upgrade.mode = InitMode::Full;
        let first = plan_repository(&upgrade).unwrap();
        let second = plan_repository(&upgrade).unwrap();

        assert_eq!(first, second);
        assert_eq!(
            first
                .manifest
                .operations
                .iter()
                .find(|operation| operation.path == ".agents/memory/pages/project-context.md")
                .unwrap()
                .mode,
            Some(0o644)
        );
        assert_eq!(
            first
                .manifest
                .operations
                .iter()
                .find(|operation| operation.path == ".agents/memory/project-context.md")
                .unwrap()
                .prestate
                .mode,
            Some(0o444)
        );
    }

    #[test]
    fn read_only_candidate_lifecycles_leave_no_staging_residue() {
        let full = repository();
        seed_full(full.path());
        for path in [
            "AGENTS.md",
            ".agents/memory/SCHEMA.md",
            ".agents/memory/INDEX.md",
            ".agents/memory/pages/project-context.md",
        ] {
            chmod(full.path(), path, 0o444);
        }
        for path in [".agents/memory/pages", ".agents/memory", ".agents"] {
            chmod(full.path(), path, 0o555);
        }
        let no_op = request(full.path(), InitMode::Full, AGENT_POLICY.to_owned());
        let planned = assert_no_new_candidate_siblings(full.path(), || plan_repository(&no_op));
        assert!(planned.unwrap().manifest.operations.is_empty());

        let minimal = repository();
        seed_minimal(minimal.path());
        chmod(minimal.path(), "AGENTS.md", 0o444);
        chmod(minimal.path(), ".agents/memory/project-context.md", 0o444);
        for path in [".agents/memory", ".agents"] {
            chmod(minimal.path(), path, 0o555);
        }

        let upgrade = request(minimal.path(), InitMode::Full, AGENT_POLICY.to_owned());
        assert_no_new_candidate_siblings(minimal.path(), || plan_repository(&upgrade)).unwrap();

        let mut replacement = request(minimal.path(), InitMode::Minimal, AGENT_POLICY.to_owned());
        replacement.project_page.fact =
            "The fictional project replaced its retained read-only context.".to_owned();
        assert_no_new_candidate_siblings(minimal.path(), || plan_repository(&replacement)).unwrap();

        let mut invalid = request(minimal.path(), InitMode::Full, AGENT_POLICY.to_owned());
        invalid.project_page.fact = "An out-of-profile ![[embed]] is refused.".to_owned();
        assert!(matches!(
            assert_no_new_candidate_siblings(minimal.path(), || plan_repository(&invalid)),
            Err(crate::InitError::Candidate(_))
        ));
    }
}
