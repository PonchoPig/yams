#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs::{self, FileTimes, OpenOptions};
    use std::os::unix::ffi::OsStringExt;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::os::unix::net::UnixListener;
    use std::path::Path;
    use std::process::Command;

    use super::*;
    use crate::{
        AGENT_POLICY, INDEX_TEMPLATE, InitMode, LayoutClass, NodeKind, PAGE_TEMPLATE,
        ReindexOptions, SCHEMA, reindex_wiki, sha256,
    };

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn repository() -> tempfile::TempDir {
        let temporary = tempfile::tempdir().unwrap();
        git(temporary.path(), &["init", "--quiet"]);
        temporary
    }

    fn linked_worktree() -> (tempfile::TempDir, tempfile::TempDir, PathBuf) {
        let repository = repository();
        write(repository.path(), "tracked.txt", "tracked\n");
        git(repository.path(), &["add", "tracked.txt"]);
        git(
            repository.path(),
            &[
                "-c",
                "user.name=Fictional Test",
                "-c",
                "user.email=fictional@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked");
        git(
            repository.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );
        (repository, linked_parent, linked)
    }

    fn gitdir_pointer(path: &Path) -> PathBuf {
        let source = fs::read(path.join(".git")).unwrap();
        let payload = source
            .strip_prefix(b"gitdir: ")
            .and_then(|value| value.strip_suffix(b"\n"))
            .unwrap();
        let pointer = PathBuf::from(OsString::from_vec(payload.to_vec()));
        if pointer.is_absolute() {
            pointer
        } else {
            path.join(pointer)
        }
    }

    fn copy_tree(source: &Path, target: &Path) {
        fs::create_dir(target).unwrap();
        for entry in fs::read_dir(source).unwrap() {
            let entry = entry.unwrap();
            let from = entry.path();
            let to = target.join(entry.file_name());
            let metadata = fs::symlink_metadata(&from).unwrap();
            if metadata.file_type().is_dir() {
                copy_tree(&from, &to);
            } else if metadata.file_type().is_file() {
                fs::copy(&from, &to).unwrap();
            } else if metadata.file_type().is_symlink() {
                symlink(fs::read_link(&from).unwrap(), &to).unwrap();
            } else {
                panic!("unexpected Git administrative node: {}", from.display());
            }
        }
    }

    fn write(root: &Path, relative: &str, bytes: impl AsRef<[u8]>) {
        let path = root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, bytes).unwrap();
    }

    fn page(slug: &str) -> String {
        PAGE_TEMPLATE
            .replace("{{slug}}", slug)
            .replace("{{title}}", "Fictional project context")
            .replace("{{type}}", "project-state")
            .replace("{{date}}", "2026-08-12")
            .replace("{{summary}}", "fictional durable repository context")
            .replace("{{fact}}", "The fictional repository uses shared memory.")
            .replace("{{evidence}}", "A synthetic test created this page.")
            .replace("{{application}}", "Read it before editing the repository.")
            .replace(
                "{{falsifier}}",
                "The fictional repository changes its contract.",
            )
    }

    fn minimal(root: &Path) {
        write(root, "AGENTS.md", AGENT_POLICY);
        write(
            root,
            ".agents/memory/project-context.md",
            page("project-context"),
        );
    }

    fn full(root: &Path) {
        write(root, "AGENTS.md", AGENT_POLICY);
        write(root, ".agents/memory/SCHEMA.md", SCHEMA);
        write(root, ".agents/memory/INDEX.md", INDEX_TEMPLATE);
        write(
            root,
            ".agents/memory/pages/project-context.md",
            page("project-context"),
        );
        reindex_wiki(&root.join(".agents/memory"), &ReindexOptions::default()).unwrap();
    }

    fn conflict_codes(inspection: &crate::InitInspection) -> Vec<&str> {
        inspection
            .conflicts
            .iter()
            .map(|conflict| conflict.code.as_str())
            .collect()
    }

    type HookAction = (String, Box<dyn FnMut()>);

    #[derive(Default)]
    struct TestInventoryHooks {
        after_directory_opened: Option<HookAction>,
        after_directory_enumerated: Option<HookAction>,
        after_file_read: Option<HookAction>,
        after_runtime_lock_captured: Option<Box<dyn FnMut()>>,
        after_first_git_status: Option<Box<dyn FnMut()>>,
    }

    impl InventoryHooks for TestInventoryHooks {
        fn after_directory_opened(&mut self, path: &str) {
            run_hook(&mut self.after_directory_opened, path);
        }

        fn after_directory_enumerated(&mut self, path: &str) {
            run_hook(&mut self.after_directory_enumerated, path);
        }

        fn after_file_read(&mut self, path: &str) {
            run_hook(&mut self.after_file_read, path);
        }

        fn after_runtime_lock_captured(&mut self) {
            if let Some(mut action) = self.after_runtime_lock_captured.take() {
                action();
            }
        }

        fn after_first_git_status(&mut self) {
            if let Some(mut action) = self.after_first_git_status.take() {
                action();
            }
        }
    }

    fn run_hook(hook: &mut Option<HookAction>, path: &str) {
        if hook.as_ref().is_some_and(|(target, _)| target == path) {
            let (_, mut action) = hook.take().unwrap();
            action();
        }
    }

    fn index_metadata_signature(root: &Path) -> (u64, u64, u64, i64, i64, i64, i64) {
        let metadata = fs::symlink_metadata(root.join(".git/index")).unwrap();
        (
            metadata.dev(),
            metadata.ino(),
            metadata.size(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec(),
        )
    }

    #[derive(Debug, Eq, PartialEq)]
    struct TestNodeState {
        mode: u32,
        size: u64,
        device: u64,
        inode: u64,
        modified: (i64, i64),
        changed: (i64, i64),
        digest: Option<String>,
    }

    fn tree_snapshot(root: &Path) -> BTreeMap<String, TestNodeState> {
        fn visit(base: &Path, path: &Path, found: &mut BTreeMap<String, TestNodeState>) {
            let metadata = fs::symlink_metadata(path).unwrap();
            let relative = path
                .strip_prefix(base)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let digest = metadata.is_file().then(|| sha256(&fs::read(path).unwrap()));
            found.insert(
                relative,
                TestNodeState {
                    mode: metadata.mode(),
                    size: metadata.size(),
                    device: metadata.dev(),
                    inode: metadata.ino(),
                    modified: (metadata.mtime(), metadata.mtime_nsec()),
                    changed: (metadata.ctime(), metadata.ctime_nsec()),
                    digest,
                },
            );
            if metadata.is_dir() {
                let mut entries = fs::read_dir(path)
                    .unwrap()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap();
                entries.sort_by_key(fs::DirEntry::file_name);
                for entry in entries {
                    visit(base, &entry.path(), found);
                }
            }
        }

        let mut found = BTreeMap::new();
        visit(root, root, &mut found);
        found
    }

    fn fsmonitor_daemon_supported(root: &Path) -> bool {
        let output = Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["fsmonitor--daemon", "-h"])
            .env("LC_ALL", "C")
            .output()
            .unwrap();
        String::from_utf8_lossy(&output.stderr).contains("fsmonitor--daemon start")
            || String::from_utf8_lossy(&output.stdout).contains("fsmonitor--daemon start")
    }

    fn assert_fsmonitor_inspection_is_read_only(root: &Path, admin_root: &Path) {
        if !fsmonitor_daemon_supported(root) {
            eprintln!("skipping fsmonitor assertions: Git has no builtin fsmonitor daemon");
            return;
        }
        git(root, &["config", "core.fsmonitor", "true"]);
        git(root, &["config", "core.untrackedCache", "true"]);
        let before = tree_snapshot(admin_root);

        inspect_repository(root).unwrap();

        assert_eq!(tree_snapshot(admin_root), before);
        assert!(!root.join(".git/fsmonitor--daemon").exists());
    }

    #[test]
    fn empty_git_repository_is_absent_and_attainable() {
        let repo = repository();
        let inspected = inspect_repository(repo.path()).unwrap();
        assert!(inspected.ok);
        assert_eq!(
            inspected.root,
            repo.path().canonicalize().unwrap().to_str().unwrap()
        );
        assert_eq!(inspected.layout, LayoutClass::Absent);
        assert_eq!(inspected.attainable, [InitMode::Minimal, InitMode::Full]);
        assert_eq!(inspected.recommended_mode, Some(InitMode::Full));
        assert!(inspected.conflicts.is_empty());
        assert_eq!(inspected.inspection_sha256.len(), 64);
    }

    #[test]
    fn canonical_minimal_layout_is_detected() {
        let repo = repository();
        minimal(repo.path());
        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Minimal);
        assert_eq!(inspected.attainable, [InitMode::Minimal, InitMode::Full]);
        assert!(inspected.conflicts.is_empty());
    }

    #[test]
    fn canonical_reindexed_clean_full_layout_is_detected() {
        let repo = repository();
        full(repo.path());
        git(repo.path(), &["add", "AGENTS.md", ".agents/memory"]);
        git(repo.path(), &["config", "user.name", "Fictional Test"]);
        git(
            repo.path(),
            &["config", "user.email", "fictional@example.invalid"],
        );
        git(
            repo.path(),
            &["commit", "--quiet", "-m", "fictional full layout"],
        );

        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Full);
        assert_eq!(inspected.attainable, [InitMode::Full]);
        assert!(inspected.conflicts.is_empty());
        assert!(inspected.dirty_paths.is_empty());
    }

    #[test]
    fn structured_partial_only_offers_additive_full_completion() {
        for relative in [
            ".agents/memory/SCHEMA.md",
            ".agents/memory/INDEX.md",
            ".agents/memory/pages/example.md",
        ] {
            let repo = repository();
            write(
                repo.path(),
                relative,
                if relative.ends_with("SCHEMA.md") {
                    SCHEMA
                } else {
                    ""
                },
            );
            let inspected = inspect_repository(repo.path()).unwrap();
            assert_eq!(inspected.layout, LayoutClass::Partial, "{relative}");
            assert!(
                !inspected.attainable.contains(&InitMode::Minimal),
                "{relative}"
            );
            if inspected.conflicts.is_empty() {
                assert_eq!(inspected.attainable, [InitMode::Full], "{relative}");
            }
        }
    }

    #[test]
    fn duplicate_and_noncanonical_policy_are_blocking_conflicts() {
        let duplicate = repository();
        write(
            duplicate.path(),
            "AGENTS.md",
            format!("{AGENT_POLICY}\n{AGENT_POLICY}"),
        );
        let inspected = inspect_repository(duplicate.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert_eq!(conflict_codes(&inspected), ["duplicate-policy-heading"]);
        assert!(inspected.attainable.is_empty());
        assert_eq!(inspected.recommended_mode, None);

        let changed = repository();
        write(
            changed.path(),
            "AGENTS.md",
            AGENT_POLICY.replacen("Search early", "Search late", 1),
        );
        let inspected = inspect_repository(changed.path()).unwrap();
        assert_eq!(conflict_codes(&inspected), ["noncanonical-policy"]);
        assert!(inspected.attainable.is_empty());
    }

    #[test]
    fn committed_invalid_utf8_agents_file_is_a_blocking_conflict() {
        let repo = repository();
        write(repo.path(), "AGENTS.md", [0xff, 0xfe, b'\n']);
        git(repo.path(), &["add", "AGENTS.md"]);
        git(repo.path(), &["config", "user.name", "Fictional Test"]);
        git(
            repo.path(),
            &["config", "user.email", "fictional@example.invalid"],
        );
        git(
            repo.path(),
            &["commit", "--quiet", "-m", "fictional invalid agents"],
        );

        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert_eq!(conflict_codes(&inspected), ["invalid-agents-utf8"]);
        assert!(inspected.attainable.is_empty());
        assert!(inspected.dirty_paths.is_empty());
    }

    #[test]
    fn differing_schema_is_a_blocking_stable_conflict() {
        let repo = repository();
        write(
            repo.path(),
            ".agents/memory/SCHEMA.md",
            "# Different schema\n",
        );
        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert_eq!(conflict_codes(&inspected), ["schema-mismatch"]);
        assert!(inspected.attainable.is_empty());
    }

    #[test]
    fn differing_memory_gitignore_is_a_blocking_stable_conflict() {
        let repo = repository();
        write(
            repo.path(),
            ".agents/memory/.gitignore",
            "*.md\n",
        );
        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert_eq!(conflict_codes(&inspected), ["gitignore-mismatch"]);
        assert!(inspected.attainable.is_empty());
    }

    #[test]
    fn symlinks_at_owned_boundaries_are_never_followed() {
        for relative in [
            "AGENTS.md",
            ".agents",
            ".agents/memory",
            ".agents/memory/pages",
            ".agents/memory/project-context.md",
        ] {
            let repo = repository();
            let target = repo.path().join("fictional-target");
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(&target, AGENT_POLICY).unwrap();
            let link = repo.path().join(relative);
            fs::create_dir_all(link.parent().unwrap()).unwrap();
            symlink(&target, &link).unwrap();

            let inspected = inspect_repository(repo.path()).unwrap();
            assert_eq!(inspected.layout, LayoutClass::Partial, "{relative}");
            assert!(
                conflict_codes(&inspected).contains(&"unsafe-symlink"),
                "{relative}"
            );
            assert!(inspected.attainable.is_empty(), "{relative}");
            assert_eq!(
                inspected
                    .prestates
                    .iter()
                    .find(|node| node.path == relative)
                    .unwrap()
                    .kind,
                NodeKind::Symlink,
                "{relative}"
            );
        }
    }

    #[test]
    fn git_dirt_is_sorted_deduplicated_and_does_not_change_classification() {
        let repo = repository();
        minimal(repo.path());
        git(repo.path(), &["add", "AGENTS.md"]);
        git(repo.path(), &["config", "user.name", "Fictional Test"]);
        git(
            repo.path(),
            &["config", "user.email", "fictional@example.invalid"],
        );
        git(
            repo.path(),
            &["commit", "--quiet", "-m", "fictional policy"],
        );
        write(repo.path(), "AGENTS.md", format!("{AGENT_POLICY}\n"));

        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Minimal);
        assert_eq!(
            inspected.dirty_paths,
            [".agents/memory/project-context.md", "AGENTS.md"]
        );
        assert_eq!(inspected.attainable, [InitMode::Minimal, InitMode::Full]);
    }

    #[test]
    fn persistent_runtime_lock_is_safely_observed_but_exempt_from_git_dirt() {
        let repo = repository();
        full(repo.path());
        git(
            repo.path(),
            &["add", "AGENTS.md", ".agents/memory/SCHEMA.md"],
        );
        git(
            repo.path(),
            &["add", ".agents/memory/INDEX.md", ".agents/memory/pages"],
        );
        git(repo.path(), &["config", "user.name", "Fictional Test"]);
        git(
            repo.path(),
            &["config", "user.email", "fictional@example.invalid"],
        );
        git(repo.path(), &["commit", "--quiet", "-m", "portable layout"]);

        let inspected = inspect_repository(repo.path()).unwrap();

        assert_eq!(inspected.layout, LayoutClass::Full);
        assert!(inspected.dirty_paths.is_empty());
        assert!(inspected.conflicts.is_empty());
        let lock = inspected
            .prestates
            .iter()
            .find(|node| node.path == ".agents/memory/.write.lock")
            .unwrap();
        assert_eq!(lock.kind, NodeKind::File);
        assert!(lock.sha256.is_none());
        assert!(lock.entries_sha256.is_none());
    }

    #[test]
    fn unsafe_runtime_lock_kinds_are_blocking_conflicts() {
        for kind in ["directory", "symlink", "socket", "hardlink"] {
            let repo = repository();
            full(repo.path());
            let lock = repo.path().join(".agents/memory/.write.lock");
            fs::remove_file(&lock).unwrap();
            let _socket = match kind {
                "directory" => {
                    fs::create_dir(&lock).unwrap();
                    None
                }
                "symlink" => {
                    symlink("INDEX.md", &lock).unwrap();
                    None
                }
                "socket" => Some(UnixListener::bind(&lock).unwrap()),
                "hardlink" => {
                    let other = repo.path().join("lock-other-name");
                    fs::write(&other, b"").unwrap();
                    fs::hard_link(&other, &lock).unwrap();
                    None
                }
                _ => unreachable!(),
            };

            let inspected = inspect_repository(repo.path()).unwrap();

            assert_eq!(inspected.layout, LayoutClass::Partial, "{kind}");
            assert!(
                inspected
                    .conflicts
                    .iter()
                    .any(|conflict| conflict.path == ".agents/memory/.write.lock"),
                "{kind}: {:?}",
                inspected.conflicts
            );
            assert!(inspected.attainable.is_empty(), "{kind}");
        }
    }

    #[test]
    fn git_dirt_filter_exempts_only_the_exact_runtime_lock_endpoint() {
        let mut conflicts = Vec::new();
        let parsed = parse_git_status(
            concat!(
                "?? .agents/memory/.write.lock\0",
                "?? .agents/memory/pages/.write.lock\0",
                "R  AGENTS.md\0.agents/memory/.write.lock\0",
                "C  .agents/memory/.write.lock\0README.md\0",
            )
            .as_bytes(),
            &mut conflicts,
        )
        .unwrap();

        assert_eq!(
            parsed,
            [".agents/memory/pages/.write.lock", "AGENTS.md", "README.md",]
        );
        assert!(conflicts.is_empty());
    }

    #[test]
    fn non_git_and_unsafe_dot_git_roots_are_rejected_without_writes() {
        let non_git = tempfile::tempdir().unwrap();
        assert!(matches!(
            inspect_repository(non_git.path()),
            Err(crate::InitError::InvalidRoot(_))
        ));
        assert!(!non_git.path().join(".agents").exists());

        let unsafe_git = tempfile::tempdir().unwrap();
        fs::write(unsafe_git.path().join("target"), b"fictional").unwrap();
        symlink("target", unsafe_git.path().join(".git")).unwrap();
        assert!(matches!(
            inspect_repository(unsafe_git.path()),
            Err(crate::InitError::InvalidRoot(_))
        ));
        assert!(!unsafe_git.path().join(".agents").exists());
    }

    #[test]
    fn repeated_inspections_are_byte_identical() {
        let repo = repository();
        minimal(repo.path());
        let first = inspect_repository(repo.path()).unwrap();
        let second = inspect_repository(repo.path()).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn harness_only_paths_do_not_affect_absent_classification() {
        let repo = repository();
        for relative in [
            ".agents/skills/fictional/SKILL.md",
            ".claude/settings.json",
            ".codex/config.toml",
            "skills-lock.json",
        ] {
            write(repo.path(), relative, "fictional\n");
        }
        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Absent);
        assert_eq!(inspected.attainable, [InitMode::Minimal, InitMode::Full]);
    }

    #[test]
    fn each_unstructured_half_is_partial_but_additively_attainable() {
        let policy_only = repository();
        write(policy_only.path(), "AGENTS.md", AGENT_POLICY);
        let inspected = inspect_repository(policy_only.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert_eq!(inspected.attainable, [InitMode::Minimal, InitMode::Full]);

        let page_only = repository();
        write(
            page_only.path(),
            ".agents/memory/project-context.md",
            page("project-context"),
        );
        let inspected = inspect_repository(page_only.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert_eq!(inspected.attainable, [InitMode::Minimal, InitMode::Full]);
    }

    #[test]
    fn empty_memory_directory_can_additively_become_minimal_or_full() {
        let repo = repository();
        fs::create_dir_all(repo.path().join(".agents/memory")).unwrap();

        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert_eq!(inspected.attainable, [InitMode::Minimal, InitMode::Full]);
        assert!(inspected.conflicts.is_empty());
    }

    #[test]
    fn retained_unrelated_memory_entry_keeps_both_modes_attainable() {
        let repo = repository();
        write(
            repo.path(),
            ".agents/memory/retained-notes.txt",
            "fictional retained notes\n",
        );

        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert!(inspected.conflicts.is_empty());
        assert_eq!(inspected.attainable, [InitMode::Minimal, InitMode::Full]);
    }

    #[test]
    fn dirty_paths_preserve_distinct_unix_backslash_and_slash_names() {
        let repo = repository();
        write(
            repo.path(),
            ".agents/memory/back\\slash.md",
            "fictional backslash path\n",
        );
        write(
            repo.path(),
            ".agents/memory/back/slash.md",
            "fictional slash path\n",
        );

        let inspected = inspect_repository(repo.path()).unwrap();
        assert_eq!(
            inspected.dirty_paths,
            [
                ".agents/memory/back/slash.md",
                ".agents/memory/back\\slash.md",
            ]
        );
    }

    #[test]
    fn inspection_disables_git_index_refresh_and_optional_locks() {
        let repo = repository();
        write(repo.path(), "AGENTS.md", AGENT_POLICY);
        git(repo.path(), &["add", "AGENTS.md"]);
        git(repo.path(), &["config", "user.name", "Fictional Test"]);
        git(
            repo.path(),
            &["config", "user.email", "fictional@example.invalid"],
        );
        git(
            repo.path(),
            &["commit", "--quiet", "-m", "fictional policy"],
        );

        let agents = OpenOptions::new()
            .write(true)
            .open(repo.path().join("AGENTS.md"))
            .unwrap();
        agents
            .set_times(FileTimes::new().set_modified(std::time::SystemTime::UNIX_EPOCH))
            .unwrap();
        let index = repo.path().join(".git/index");
        let before = fs::read(&index).unwrap();
        let metadata_before = index_metadata_signature(repo.path());

        inspect_repository(repo.path()).unwrap();

        assert_eq!(fs::read(&index).unwrap(), before);
        assert_eq!(index_metadata_signature(repo.path()), metadata_before);
        assert!(!repo.path().join(".git/index.lock").exists());
    }

    #[test]
    fn fsmonitor_enabled_repository_inspection_leaves_git_state_untouched() {
        let repo = repository();
        minimal(repo.path());
        git(repo.path(), &["add", "AGENTS.md", ".agents/memory"]);
        git(repo.path(), &["config", "user.name", "Fictional Test"]);
        git(
            repo.path(),
            &["config", "user.email", "fictional@example.invalid"],
        );
        git(
            repo.path(),
            &["commit", "--quiet", "-m", "fictional layout"],
        );
        assert_fsmonitor_inspection_is_read_only(repo.path(), &repo.path().join(".git"));
    }

    #[test]
    fn fsmonitor_enabled_linked_worktree_inspection_leaves_common_git_state_untouched() {
        let repo = repository();
        minimal(repo.path());
        git(repo.path(), &["add", "AGENTS.md", ".agents/memory"]);
        git(repo.path(), &["config", "user.name", "Fictional Test"]);
        git(
            repo.path(),
            &["config", "user.email", "fictional@example.invalid"],
        );
        git(
            repo.path(),
            &["commit", "--quiet", "-m", "fictional layout"],
        );
        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked");
        git(
            repo.path(),
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );

        assert_fsmonitor_inspection_is_read_only(&linked, &repo.path().join(".git"));
    }

    #[test]
    fn linked_worktree_pointer_bytes_are_bound_even_when_its_inode_and_target_are_unchanged() {
        let (_repository, _linked_parent, linked) = linked_worktree();
        let first = inspect_repository(&linked).unwrap();
        let pointer = linked.join(".git");
        let inode = fs::metadata(&pointer).unwrap().ino();
        let admin = gitdir_pointer(&linked);

        fs::write(&pointer, format!("gitdir: {}/\n", admin.display())).unwrap();
        assert_eq!(fs::metadata(&pointer).unwrap().ino(), inode);

        let second = inspect_repository(&linked).unwrap();
        assert_ne!(first.inspection_sha256, second.inspection_sha256);
        let mut first_shape = first;
        first_shape.inspection_sha256.clear();
        let mut second_shape = second;
        second_shape.inspection_sha256.clear();
        assert_eq!(first_shape, second_shape);
    }

    #[test]
    fn linked_worktree_repeated_inspections_have_one_stable_token() {
        let (_repository, _linked_parent, linked) = linked_worktree();

        let first = inspect_repository(&linked).unwrap();
        let second = inspect_repository(&linked).unwrap();

        assert_eq!(first, second);
    }

    #[test]
    fn linked_worktree_accepts_and_binds_a_non_utf8_admin_path() {
        let main_parent = tempfile::tempdir().unwrap();
        let main = main_parent
            .path()
            .join(OsString::from_vec(b"main-\xff".to_vec()));
        if let Err(error) = fs::create_dir(&main) {
            if error.raw_os_error() == Some(Errno::ILSEQ.raw_os_error()) {
                eprintln!("skipping non-UTF-8 worktree fixture: filesystem rejected its name");
                return;
            }
            panic!("could not create non-UTF-8 repository path: {error}");
        }
        git(&main, &["init", "--quiet"]);
        write(&main, "tracked.txt", "tracked\n");
        git(&main, &["add", "tracked.txt"]);
        git(
            &main,
            &[
                "-c",
                "user.name=Fictional Test",
                "-c",
                "user.email=fictional@example.invalid",
                "commit",
                "--quiet",
                "-m",
                "fixture",
            ],
        );
        let linked_parent = tempfile::tempdir().unwrap();
        let linked = linked_parent.path().join("linked");
        git(
            &main,
            &[
                "worktree",
                "add",
                "--quiet",
                "--detach",
                linked.to_str().unwrap(),
            ],
        );

        let first = inspect_repository(&linked).unwrap();
        let second = inspect_repository(&linked).unwrap();
        assert_eq!(first, second);

        let admin = gitdir_pointer(&linked);
        assert!(admin.as_os_str().as_bytes().contains(&0xff));
        let displaced = linked_parent.path().join("displaced-non-utf8-admin");
        fs::rename(&admin, &displaced).unwrap();
        copy_tree(&displaced, &admin);

        let rebound = inspect_repository(&linked).unwrap();
        assert_ne!(first.inspection_sha256, rebound.inspection_sha256);
        let mut first_shape = first;
        first_shape.inspection_sha256.clear();
        let mut rebound_shape = rebound;
        rebound_shape.inspection_sha256.clear();
        assert_eq!(first_shape, rebound_shape);
    }

    #[test]
    fn gitdir_pointer_grammar_preserves_non_utf8_path_bytes() {
        let root = Path::new("/fictional/worktree");
        let payload = b"../admin-\xff";
        let expected = root.join(OsString::from_vec(payload.to_vec()));

        for ending in [b"".as_slice(), b"\n".as_slice(), b"\r\n".as_slice()] {
            let mut source = b"gitdir: ".to_vec();
            source.extend_from_slice(payload);
            source.extend_from_slice(ending);
            assert_eq!(decode_gitdir_pointer(root, &source).unwrap(), expected);
        }

        for malformed in [
            b"".as_slice(),
            b"gitdir: ".as_slice(),
            b"GITDIR: ../admin".as_slice(),
            b"gitdir:\t../admin".as_slice(),
            b"gitdir: ../admin\0suffix".as_slice(),
            b"gitdir: ../admin\ntrailing".as_slice(),
            b"gitdir: ../admin\n\n".as_slice(),
            b"gitdir: ../admin\rtrailing".as_slice(),
        ] {
            assert!(
                matches!(
                    decode_gitdir_pointer(root, malformed),
                    Err(InitError::InvalidRoot(_))
                ),
                "accepted malformed pointer {malformed:?}"
            );
        }
    }

    #[test]
    fn linked_worktree_pointer_drift_after_first_git_status_aborts_inspection() {
        let (_repository, _linked_parent, linked) = linked_worktree();
        let pointer = linked.join(".git");
        let admin = gitdir_pointer(&linked);
        let mut hooks = TestInventoryHooks {
            after_first_git_status: Some(Box::new(move || {
                fs::write(&pointer, format!("gitdir: {}/\n", admin.display())).unwrap();
            })),
            ..TestInventoryHooks::default()
        };

        let error = inspect_repository_with_hooks(&linked, &mut hooks).unwrap_err();

        assert!(
            matches!(
                error,
                InitError::Io {
                    operation: "capture stable inventory",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn linked_worktree_admin_directory_identity_is_bound_even_at_the_same_path() {
        let (_repository, linked_parent, linked) = linked_worktree();
        let first = inspect_repository(&linked).unwrap();
        let admin = gitdir_pointer(&linked);
        let displaced = linked_parent.path().join("displaced-admin");
        fs::rename(&admin, &displaced).unwrap();
        copy_tree(&displaced, &admin);

        let second = inspect_repository(&linked).unwrap();
        assert_ne!(first.inspection_sha256, second.inspection_sha256);
        let mut first_shape = first;
        first_shape.inspection_sha256.clear();
        let mut second_shape = second;
        second_shape.inspection_sha256.clear();
        assert_eq!(first_shape, second_shape);
    }

    #[test]
    fn linked_worktree_admin_drift_after_first_git_status_aborts_inspection() {
        let (_repository, linked_parent, linked) = linked_worktree();
        let admin = gitdir_pointer(&linked);
        let displaced = linked_parent.path().join("displaced-admin-race");
        let admin_for_hook = admin.clone();
        let mut hooks = TestInventoryHooks {
            after_first_git_status: Some(Box::new(move || {
                fs::rename(&admin_for_hook, &displaced).unwrap();
                copy_tree(&displaced, &admin_for_hook);
            })),
            ..TestInventoryHooks::default()
        };

        let error = inspect_repository_with_hooks(&linked, &mut hooks).unwrap_err();

        assert!(
            matches!(
                error,
                InitError::Io {
                    operation: "capture stable inventory",
                    ..
                }
            ),
            "{error:?}"
        );
    }

    #[test]
    fn malformed_missing_and_nondirectory_gitdir_pointers_are_invalid_roots() {
        for source in [
            "not a gitdir pointer\n".to_owned(),
            "gitdir: missing-admin\n".to_owned(),
            "gitdir: regular-admin\n".to_owned(),
        ] {
            let root = tempfile::tempdir().unwrap();
            if source.contains("regular-admin") {
                fs::write(root.path().join("regular-admin"), "not a directory\n").unwrap();
            }
            fs::write(root.path().join(".git"), source).unwrap();

            let error = inspect_repository(root.path()).unwrap_err();

            assert!(matches!(error, InitError::InvalidRoot(_)), "{error:?}");
            assert!(!root.path().join(".agents").exists());
        }
    }

    #[test]
    fn hostile_git_environment_child() {
        let Some(root) = std::env::var_os("YAMS_HOSTILE_GIT_TEST_ROOT") else {
            return;
        };
        let output = PathBuf::from(std::env::var_os("YAMS_HOSTILE_GIT_TEST_OUTPUT").unwrap());
        let inspected = inspect_repository(Path::new(&root)).unwrap();
        fs::write(output, serde_json::to_vec(&inspected).unwrap()).unwrap();
    }

    #[test]
    fn inspection_does_not_inherit_git_routing_object_ref_or_trace_environment() {
        let repo = repository();
        minimal(repo.path());
        let expected = inspect_repository(repo.path()).unwrap();
        let trap = tempfile::tempdir().unwrap();
        let alternate = repository();
        let output_path = trap.path().join("inspection.json");
        let trace = trap.path().join("trace.log");
        let trace2 = trap.path().join("trace2.json");
        let trace_fsmonitor = trap.path().join("trace-fsmonitor.log");
        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "init::inspect::tests::hostile_git_environment_child",
                "--nocapture",
            ])
            .env("YAMS_HOSTILE_GIT_TEST_ROOT", repo.path())
            .env("YAMS_HOSTILE_GIT_TEST_OUTPUT", &output_path)
            .env("GIT_INDEX_FILE", trap.path().join("hostile-index"))
            .env("GIT_DIR", alternate.path().join(".git"))
            .env("GIT_WORK_TREE", alternate.path())
            .env("GIT_COMMON_DIR", alternate.path().join(".git"))
            .env("GIT_OBJECT_DIRECTORY", trap.path().join("objects"))
            .env(
                "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                trap.path().join("alternate-objects"),
            )
            .env("GIT_NAMESPACE", "hostile-namespace")
            .env("GIT_TRACE", &trace)
            .env("GIT_TRACE2_EVENT", &trace2)
            .env("GIT_TRACE_FSMONITOR", &trace_fsmonitor)
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "child failed: {}",
            String::from_utf8_lossy(&child.stderr)
        );
        let actual: crate::InitInspection =
            serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
        assert_eq!(actual, expected);
        for path in [trace, trace2, trace_fsmonitor] {
            assert!(!path.exists(), "Git created hostile trace target {path:?}");
        }
        assert!(!trap.path().join("hostile-index").exists());
        assert!(!trap.path().join("objects").exists());
    }

    #[test]
    fn cleared_git_environment_honors_home_and_xdg_global_config() {
        for config_location in ["home", "xdg"] {
            let repo = repository();
            write(
                repo.path(),
                ".agents/memory/ignored.md",
                "fictional globally ignored memory\n",
            );
            let home = tempfile::tempdir().unwrap();
            let xdg = tempfile::tempdir().unwrap();
            let excludes = home.path().join("fictional-global-excludes");
            fs::write(&excludes, ".agents/memory/ignored.md\n").unwrap();
            let config = format!("[core]\n\texcludesFile = {}\n", excludes.display());
            match config_location {
                "home" => fs::write(home.path().join(".gitconfig"), config).unwrap(),
                "xdg" => {
                    fs::create_dir_all(xdg.path().join("git")).unwrap();
                    fs::write(xdg.path().join("git/config"), config).unwrap();
                }
                _ => unreachable!(),
            }

            let trap = tempfile::tempdir().unwrap();
            let alternate = repository();
            let output_path = trap.path().join("inspection.json");
            let trace = trap.path().join("trace.log");
            let trace2 = trap.path().join("trace2.json");
            let trace_fsmonitor = trap.path().join("trace-fsmonitor.log");
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "init::inspect::tests::hostile_git_environment_child",
                    "--nocapture",
                ])
                .env("YAMS_HOSTILE_GIT_TEST_ROOT", repo.path())
                .env("YAMS_HOSTILE_GIT_TEST_OUTPUT", &output_path)
                .env("HOME", home.path())
                .env("XDG_CONFIG_HOME", xdg.path())
                .env("GIT_INDEX_FILE", trap.path().join("hostile-index"))
                .env("GIT_DIR", alternate.path().join(".git"))
                .env("GIT_WORK_TREE", alternate.path())
                .env("GIT_COMMON_DIR", alternate.path().join(".git"))
                .env("GIT_OBJECT_DIRECTORY", trap.path().join("objects"))
                .env("GIT_NAMESPACE", "hostile-namespace")
                .env("GIT_TRACE", &trace)
                .env("GIT_TRACE2_EVENT", &trace2)
                .env("GIT_TRACE_FSMONITOR", &trace_fsmonitor)
                .output()
                .unwrap();
            assert!(
                child.status.success(),
                "{config_location} child failed: {}",
                String::from_utf8_lossy(&child.stderr)
            );
            let inspected: crate::InitInspection =
                serde_json::from_slice(&fs::read(output_path).unwrap()).unwrap();
            assert_eq!(inspected.dirty_paths, Vec::<String>::new());
            for path in [trace, trace2, trace_fsmonitor] {
                assert!(
                    !path.exists(),
                    "Git created hostile trace target {path:?} for {config_location}"
                );
            }
            assert!(!trap.path().join("hostile-index").exists());
            assert!(!trap.path().join("objects").exists());
        }
    }

    #[test]
    fn cleared_git_environment_still_honors_repository_local_config() {
        let repo = repository();
        let excludes = repo.path().join("fictional-excludes");
        fs::write(&excludes, ".agents/memory/ignored.md\n").unwrap();
        git(
            repo.path(),
            &["config", "core.excludesFile", excludes.to_str().unwrap()],
        );
        write(
            repo.path(),
            ".agents/memory/ignored.md",
            "fictional ignored memory\n",
        );

        let inspected = inspect_repository(repo.path()).unwrap();
        assert!(inspected.dirty_paths.is_empty());
        assert_eq!(inspected.layout, LayoutClass::Partial);
    }

    #[test]
    fn porcelain_parser_handles_rename_copy_and_embedded_newlines() {
        let mut conflicts = Vec::new();
        let parsed = parse_git_status(
            b"R  .agents/memory/new.md\0.agents/memory/old.md\0 C .agents/memory/copy.md\0.agents/memory/source.md\0?? .agents/memory/line\nbreak.md\0",
            &mut conflicts,
        )
        .unwrap();
        assert_eq!(
            parsed,
            [
                ".agents/memory/copy.md",
                ".agents/memory/line\nbreak.md",
                ".agents/memory/new.md",
                ".agents/memory/old.md",
                ".agents/memory/source.md",
            ]
        );
        assert!(conflicts.is_empty());
    }

    #[test]
    fn porcelain_parser_rejects_malformed_and_truncated_records() {
        for malformed in [
            b"?? unterminated".as_slice(),
            b"?\0".as_slice(),
            b"ZZ hostile\0".as_slice(),
            b"R  new\0".as_slice(),
        ] {
            let mut conflicts = Vec::new();
            assert!(matches!(
                parse_git_status(malformed, &mut conflicts),
                Err(crate::InitError::Git(_))
            ));
            assert!(conflicts.is_empty());
        }
    }

    /// A non-UTF-8 path is a different failure mode than a malformed
    /// porcelain record: the bytes are a well-formed `-z` status line, they
    /// just cannot be losslessly represented as a `String`. Parsing must not
    /// hard-fail the whole inspection over this (the file itself is real and
    /// separately diagnosable) — it must record a blocking conflict instead
    /// and continue parsing the remaining, well-formed records.
    #[test]
    fn porcelain_parser_flags_non_utf8_paths_as_a_conflict_instead_of_failing() {
        let mut conflicts = Vec::new();
        let parsed = parse_git_status(
            b"?? AGENTS.md\0?? .agents/memory/pages/x\xff.md\0MM README.md\0",
            &mut conflicts,
        )
        .unwrap();

        assert_eq!(parsed, ["AGENTS.md", "README.md"]);
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].path, ".agents/memory");
        assert_eq!(conflicts[0].code, "non-utf8-git-status-path");

        // A bare non-UTF-8 record with no other well-formed records around it
        // behaves the same way: `Ok` with the entry surfaced as a conflict
        // rather than dropped or turned into an `Err`.
        let mut conflicts = Vec::new();
        let parsed = parse_git_status(b"?? \xff\0", &mut conflicts).unwrap();
        assert!(parsed.is_empty());
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].code, "non-utf8-git-status-path");

        // Still rejected outright: an empty path (here, the old-path half of
        // a rename record) makes the porcelain stream itself untrustworthy,
        // independent of UTF-8 validity.
        let mut conflicts = Vec::new();
        assert!(matches!(
            parse_git_status(b"R  a\0\0", &mut conflicts),
            Err(crate::InitError::Git(_))
        ));
        assert!(conflicts.is_empty());
    }

    #[test]
    fn replacing_agents_ancestor_after_open_aborts_without_following_replacement() {
        let repo = repository();
        minimal(repo.path());
        let root = repo.path().to_path_buf();
        let outside = tempfile::tempdir().unwrap();
        write(
            outside.path(),
            "memory/project-context.md",
            page("project-context"),
        );
        let outside_path = outside.path().to_path_buf();
        let mut hooks = TestInventoryHooks {
            after_directory_opened: Some((
                ".agents".to_owned(),
                Box::new(move || {
                    fs::rename(root.join(".agents"), root.join(".agents-stale")).unwrap();
                    symlink(&outside_path, root.join(".agents")).unwrap();
                }),
            )),
            ..Default::default()
        };

        assert!(matches!(
            inspect_repository_with_hooks(repo.path(), &mut hooks),
            Err(crate::InitError::Io {
                operation: "capture stable inventory",
                ..
            })
        ));
    }

    #[test]
    fn replacing_pages_directory_during_enumeration_aborts() {
        let repo = repository();
        full(repo.path());
        let root = repo.path().to_path_buf();
        let mut hooks = TestInventoryHooks {
            after_directory_enumerated: Some((
                ".agents/memory/pages".to_owned(),
                Box::new(move || {
                    let pages = root.join(".agents/memory/pages");
                    fs::rename(&pages, root.join(".agents/memory/pages-stale")).unwrap();
                    fs::create_dir(&pages).unwrap();
                    fs::write(pages.join("project-context.md"), page("project-context")).unwrap();
                }),
            )),
            ..Default::default()
        };

        assert!(matches!(
            inspect_repository_with_hooks(repo.path(), &mut hooks),
            Err(crate::InitError::Io {
                operation: "capture stable inventory",
                ..
            })
        ));
    }

    #[test]
    fn replacing_runtime_lock_after_capture_aborts_stable_inspection() {
        let repo = repository();
        full(repo.path());
        let root = repo.path().to_path_buf();
        let mut hooks = TestInventoryHooks {
            after_runtime_lock_captured: Some(Box::new(move || {
                let lock = root.join(".agents/memory/.write.lock");
                fs::rename(&lock, root.join(".agents/memory/.write.lock-stale")).unwrap();
                fs::write(&lock, b"").unwrap();
            })),
            ..Default::default()
        };

        assert!(matches!(
            inspect_repository_with_hooks(repo.path(), &mut hooks),
            Err(crate::InitError::Io {
                operation: "capture stable inventory",
                ..
            })
        ));
    }

    #[test]
    fn same_size_agents_rewrite_after_read_aborts() {
        let repo = repository();
        write(repo.path(), "AGENTS.md", AGENT_POLICY);
        let root = repo.path().to_path_buf();
        let mut hooks = TestInventoryHooks {
            after_file_read: Some((
                "AGENTS.md".to_owned(),
                Box::new(move || {
                    fs::write(root.join("AGENTS.md"), vec![b'x'; AGENT_POLICY.len()]).unwrap();
                }),
            )),
            ..Default::default()
        };

        assert!(matches!(
            inspect_repository_with_hooks(repo.path(), &mut hooks),
            Err(crate::InitError::Io {
                operation: "capture stable inventory",
                ..
            })
        ));
    }

    #[test]
    fn full_inspection_is_read_only_and_rejects_out_of_profile_pages() {
        let clean = repository();
        full(clean.path());
        fs::remove_file(clean.path().join(".agents/memory/.write.lock")).unwrap();
        let inspected = inspect_repository(clean.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Full);
        assert!(!clean.path().join(".agents/memory/.write.lock").exists());

        let incompatible = repository();
        full(incompatible.path());
        let page_path = incompatible
            .path()
            .join(".agents/memory/pages/project-context.md");
        let source = fs::read_to_string(&page_path).unwrap();
        fs::write(&page_path, format!("{source}\n![[external-page]]\n")).unwrap();
        let inspected = inspect_repository(incompatible.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert!(conflict_codes(&inspected).contains(&"invalid-full-wiki"));
        assert!(inspected.attainable.is_empty());
    }

    #[test]
    fn invalid_flat_and_structured_project_pages_block_completion() {
        let flat = repository();
        write(flat.path(), "AGENTS.md", AGENT_POLICY);
        write(
            flat.path(),
            ".agents/memory/project-context.md",
            "not a page\n",
        );
        let inspected = inspect_repository(flat.path()).unwrap();
        assert_eq!(conflict_codes(&inspected), ["invalid-project-page"]);
        assert!(inspected.attainable.is_empty());

        let structured = repository();
        write(structured.path(), "AGENTS.md", AGENT_POLICY);
        write(structured.path(), ".agents/memory/SCHEMA.md", SCHEMA);
        write(structured.path(), ".agents/memory/INDEX.md", INDEX_TEMPLATE);
        write(
            structured.path(),
            ".agents/memory/pages/project-context.md",
            page("wrong-slug"),
        );
        let inspected = inspect_repository(structured.path()).unwrap();
        assert_eq!(inspected.layout, LayoutClass::Partial);
        assert!(conflict_codes(&inspected).contains(&"invalid-project-page"));
        assert!(inspected.attainable.is_empty());
    }

    #[test]
    fn inventory_records_modes_digests_missing_nodes_sorted_entries_and_symlinks() {
        let repo = repository();
        write(repo.path(), "AGENTS.md", AGENT_POLICY);
        fs::set_permissions(
            repo.path().join("AGENTS.md"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        write(repo.path(), ".agents/memory/pages/zeta.md", page("zeta"));
        write(repo.path(), ".agents/memory/pages/alpha.md", page("alpha"));
        symlink(
            "fictional-target",
            repo.path().join(".agents/memory/pages/link.md"),
        )
        .unwrap();

        let inspected = inspect_repository(repo.path()).unwrap();
        let paths: Vec<_> = inspected
            .prestates
            .iter()
            .map(|node| node.path.as_str())
            .collect();
        let mut sorted = paths.clone();
        sorted.sort_unstable();
        assert_eq!(paths, sorted);

        let agents = inspected
            .prestates
            .iter()
            .find(|node| node.path == "AGENTS.md")
            .unwrap();
        assert_eq!(agents.kind, NodeKind::File);
        assert_eq!(agents.mode, Some(0o640));
        assert_eq!(
            agents.sha256.as_deref(),
            Some(sha256(AGENT_POLICY.as_bytes()).as_str())
        );
        assert!(agents.entries_sha256.is_none());

        let schema = inspected
            .prestates
            .iter()
            .find(|node| node.path == ".agents/memory/SCHEMA.md")
            .unwrap();
        assert_eq!(schema.kind, NodeKind::Missing);
        assert_eq!(
            (schema.mode, &schema.sha256, &schema.entries_sha256),
            (None, &None, &None)
        );

        let pages = inspected
            .prestates
            .iter()
            .find(|node| node.path == ".agents/memory/pages")
            .unwrap();
        assert_eq!(pages.kind, NodeKind::Directory);
        assert_eq!(pages.entries_sha256.as_ref().unwrap().len(), 64);
        assert!(paths.contains(&".agents/memory/pages/alpha.md"));
        assert!(paths.contains(&".agents/memory/pages/zeta.md"));
        assert_eq!(
            inspected
                .prestates
                .iter()
                .find(|node| node.path == ".agents/memory/pages/link.md")
                .unwrap()
                .kind,
            NodeKind::Symlink
        );
    }

    #[test]
    fn non_utf8_page_names_and_oversized_files_have_stable_blocking_conflicts() {
        let non_utf8 = repository();
        fs::create_dir_all(non_utf8.path().join(".agents/memory/pages")).unwrap();
        let non_utf8_created = fs::write(
            non_utf8
                .path()
                .join(".agents/memory/pages")
                .join(OsString::from_vec(vec![b'x', 0xff, b'.', b'm', b'd'])),
            page("x"),
        )
        .is_ok();
        if non_utf8_created {
            let inspected = inspect_repository(non_utf8.path()).unwrap();
            assert!(conflict_codes(&inspected).contains(&"non-utf8-page-name"));
            assert!(inspected.attainable.is_empty());
        }

        let oversized = repository();
        write(
            oversized.path(),
            "AGENTS.md",
            vec![b'x'; yams_core::MAX_FILE_BYTES as usize + 1],
        );
        let inspected = inspect_repository(oversized.path()).unwrap();
        assert!(conflict_codes(&inspected).contains(&"oversized-file"));
        assert!(inspected.attainable.is_empty());
    }
}
