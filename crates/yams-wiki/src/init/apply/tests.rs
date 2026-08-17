#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fs;
    use std::os::fd::BorrowedFd;
    use std::path::Path;
    use std::process::Command;

    use rustix::io::Errno;

    use super::{
        ApplyExitClass, ApplyHooks, apply_manifest, apply_manifest_classified,
        apply_manifest_classified_with_hooks, apply_manifest_with_hooks,
    };
    use crate::{
        AGENT_POLICY, InitMode, InitPlanRequest, LayoutClass, LockError, LockLease, LockMode,
        ManifestEnvelope, PageType, ProjectPageRequest, acquire_lock, acquire_lock_with_timeout,
        canonical_manifest_bytes, inspect_repository, plan_repository, sha256,
    };
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Duration;

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
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        root
    }

    fn commit_all(root: &Path) {
        git(root, &["add", "-A"]);
        git(
            root,
            &[
                "-c",
                "user.email=test@example.invalid",
                "-c",
                "user.name=Yams Test",
                "commit",
                "-qm",
                "fixture",
            ],
        );
    }

    fn directory_temps(root: &Path) -> Vec<std::path::PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![root.to_owned()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(directory).unwrap() {
                let entry = entry.unwrap();
                if entry.file_name().to_string_lossy().contains(".yams-dir-") {
                    found.push(entry.path());
                } else if entry.file_type().unwrap().is_dir() && entry.file_name() != ".git" {
                    pending.push(entry.path());
                }
            }
        }
        found.sort();
        found
    }

    fn resign(envelope: &mut ManifestEnvelope) {
        envelope.manifest_sha256 = sha256(&canonical_manifest_bytes(&envelope.manifest).unwrap());
    }

    #[derive(Default)]
    struct TestHooks {
        fail_after: Option<usize>,
        before_action: Option<(usize, Box<dyn FnOnce()>)>,
        after_failure_action: Option<Box<dyn FnOnce()>>,
        fail_directory_fsync: bool,
        fail_file_fsync: bool,
        fail_final: bool,
        fail_after_mkdir: bool,
        fail_after_file_open: bool,
        fail_file_open_path: Option<String>,
        fail_after_rename: bool,
        fail_parent_restore: bool,
        fail_parent_verification: bool,
        fail_normal_parent_restore_fsync: bool,
        fail_recovery_parent_restore_fsync: bool,
        fail_replaced_temp_cleanup_fsync: bool,
        fail_directory_install: bool,
        fail_restore_temp_cleanup: bool,
        after_file_fsync_action: Option<Box<dyn FnOnce()>>,
        usage_failure_before_operation: Option<usize>,
        fail_final_root_verification: bool,
        before_final_action: Option<Box<dyn FnOnce()>>,
        during_final_action: Option<Box<dyn FnOnce()>>,
        after_parent_widened_action: Option<Box<dyn FnOnce()>>,
        after_mkdir_action: Option<Box<dyn FnOnce()>>,
        after_directory_temp_action: Option<Box<dyn FnOnce()>>,
        before_directory_install_action: Option<Box<dyn FnOnce()>>,
        after_file_open_action: Option<Box<dyn FnOnce()>>,
        before_replace_install_action: Option<(String, Box<dyn FnOnce()>)>,
        before_create_file_open_action: Option<Box<dyn FnOnce()>>,
        before_remove_action: Option<Box<dyn FnOnce()>>,
        fail_remove: bool,
        immediate_wiki_lock: bool,
    }

    impl ApplyHooks for TestHooks {
        fn acquire_wiki_lock(
            &mut self,
            corpus: &std::path::Path,
        ) -> Result<LockLease, crate::LockError> {
            if self.immediate_wiki_lock {
                acquire_lock_with_timeout(corpus, LockMode::Exclusive, Duration::ZERO)
            } else {
                acquire_lock(corpus, LockMode::Exclusive)
            }
        }

        fn before_operation(&mut self, index: usize, _path: &str) -> Result<(), String> {
            if self
                .before_action
                .as_ref()
                .is_some_and(|(at, _)| *at == index)
            {
                let (_, action) = self.before_action.take().unwrap();
                action();
            }
            Ok(())
        }

        fn usage_failure_before_operation(&mut self, index: usize, path: &str) -> Option<String> {
            (self.usage_failure_before_operation == Some(index)).then(|| {
                self.usage_failure_before_operation = None;
                format!("injected approved-state drift at {path}")
            })
        }

        fn after_operation(&mut self, index: usize, _path: &str) -> Result<(), String> {
            if self.fail_after == Some(index) {
                Err("injected failure after operation".to_owned())
            } else {
                Ok(())
            }
        }

        fn before_parent_verification(&mut self, _path: &str) -> Result<(), String> {
            if self.fail_parent_verification {
                self.fail_parent_verification = false;
                Err("injected parent verification I/O failure".to_owned())
            } else {
                Ok(())
            }
        }

        fn before_create_file_open(&mut self, _path: &str) -> Result<(), String> {
            if let Some(action) = self.before_create_file_open_action.take() {
                action();
            }
            Ok(())
        }

        fn before_final_validation(&mut self) -> Result<(), String> {
            if let Some(action) = self.before_final_action.take() {
                action();
            }
            if self.fail_final {
                Err("injected final validation failure".to_owned())
            } else {
                Ok(())
            }
        }

        fn during_final_validation(&mut self) -> Result<(), String> {
            if let Some(action) = self.during_final_action.take() {
                action();
            }
            Ok(())
        }

        fn after_mkdir_journaled(&mut self, _path: &str) -> Result<(), String> {
            if let Some(action) = self.after_directory_temp_action.take() {
                action();
                return Err("injected directory temporary rebind after mkdir".to_owned());
            }
            if self.fail_after_mkdir {
                self.fail_after_mkdir = false;
                Err("injected failure after mkdir journal".to_owned())
            } else {
                Ok(())
            }
        }

        fn before_directory_install(&mut self, _path: &str) -> Result<(), String> {
            if let Some(action) = self.before_directory_install_action.take() {
                action();
            }
            Ok(())
        }

        fn after_mkdir_identified(&mut self, _path: &str) -> Result<(), String> {
            if let Some(action) = self.after_mkdir_action.take() {
                action();
            }
            Ok(())
        }

        fn after_file_open_journaled(&mut self, path: &str) -> Result<(), String> {
            if let Some(action) = self.after_file_open_action.take() {
                action();
            }
            if self.fail_after_file_open || self.fail_file_open_path.as_deref() == Some(path) {
                self.fail_after_file_open = false;
                self.fail_file_open_path = None;
                Err("injected failure after file open journal".to_owned())
            } else {
                Ok(())
            }
        }

        fn after_rename_journaled(&mut self, _path: &str) -> Result<(), String> {
            if self.fail_after_rename {
                self.fail_after_rename = false;
                Err("injected failure after rename journal".to_owned())
            } else {
                Ok(())
            }
        }

        fn before_replace_install(&mut self, path: &str) -> Result<(), String> {
            if self
                .before_replace_install_action
                .as_ref()
                .is_some_and(|(target, _)| target == path)
            {
                let (_, action) = self.before_replace_install_action.take().unwrap();
                action();
            }
            Ok(())
        }

        fn after_parent_widened(&mut self, _path: &str) -> Result<(), String> {
            if let Some(action) = self.after_parent_widened_action.take() {
                action();
            }
            Ok(())
        }

        fn after_failure(&mut self) {
            if let Some(action) = self.after_failure_action.take() {
                action();
            }
        }

        fn directory_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            if self.fail_directory_fsync {
                self.fail_directory_fsync = false;
                Err(Errno::IO)
            } else {
                rustix::fs::fsync(fd)
            }
        }

        fn file_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            if self.fail_file_fsync {
                self.fail_file_fsync = false;
                Err(Errno::IO)
            } else {
                rustix::fs::fsync(fd)?;
                if let Some(action) = self.after_file_fsync_action.take() {
                    action();
                }
                Ok(())
            }
        }

        fn restore_parent_mode(
            &mut self,
            fd: BorrowedFd<'_>,
            mode: rustix::fs::Mode,
        ) -> Result<(), Errno> {
            if self.fail_parent_restore {
                self.fail_parent_restore = false;
                Err(Errno::IO)
            } else {
                rustix::fs::fchmod(fd, mode)
            }
        }

        fn parent_mode_fsync(&mut self, fd: BorrowedFd<'_>, recovery: bool) -> Result<(), Errno> {
            let fail = if recovery {
                &mut self.fail_recovery_parent_restore_fsync
            } else {
                &mut self.fail_normal_parent_restore_fsync
            };
            if *fail {
                *fail = false;
                Err(Errno::IO)
            } else {
                rustix::fs::fsync(fd)
            }
        }

        fn replaced_temp_cleanup_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            if self.fail_replaced_temp_cleanup_fsync {
                self.fail_replaced_temp_cleanup_fsync = false;
                Err(Errno::IO)
            } else {
                rustix::fs::fsync(fd)
            }
        }

        fn remove_file(&mut self, parent: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
            if let Some(action) = self.before_remove_action.take() {
                action();
            }
            if self.fail_remove {
                self.fail_remove = false;
                Err(Errno::IO)
            } else {
                rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty())
            }
        }

        fn install_directory(
            &mut self,
            old_parent: BorrowedFd<'_>,
            old_name: &OsStr,
            new_parent: BorrowedFd<'_>,
            new_name: &OsStr,
        ) -> Result<(), Errno> {
            if self.fail_directory_install {
                self.fail_directory_install = false;
                Err(Errno::IO)
            } else {
                rustix::fs::renameat_with(
                    old_parent,
                    old_name,
                    new_parent,
                    new_name,
                    rustix::fs::RenameFlags::NOREPLACE,
                )
            }
        }

        fn remove_restore_temporary(
            &mut self,
            parent: BorrowedFd<'_>,
            name: &OsStr,
        ) -> Result<(), Errno> {
            if self.fail_restore_temp_cleanup {
                self.fail_restore_temp_cleanup = false;
                Err(Errno::IO)
            } else {
                rustix::fs::unlinkat(parent, name, rustix::fs::AtFlags::empty())
            }
        }

        fn before_final_root_verification(&mut self) -> Result<(), String> {
            if self.fail_final_root_verification {
                self.fail_final_root_verification = false;
                Err("injected final root verification I/O failure".to_owned())
            } else {
                Ok(())
            }
        }
    }

    fn request(root: &Path, mode: InitMode) -> InitPlanRequest {
        let inspection = inspect_repository(root).unwrap();
        InitPlanRequest {
            root: inspection.root,
            inspection_sha256: inspection.inspection_sha256,
            mode,
            date: "2026-08-12".to_owned(),
            agents_md: AGENT_POLICY.to_owned(),
            project_page: ProjectPageRequest {
                title: "Project context".to_owned(),
                page_type: PageType::ProjectState,
                fact: "The project uses approved initialization manifests.".to_owned(),
                why: "Mutations must be reviewable.".to_owned(),
                how_to_apply: "Inspect, plan, approve, and apply.".to_owned(),
                falsified_by: "An unapproved mutation succeeds.".to_owned(),
                summary: "Memory initialization is manifest-driven.".to_owned(),
            },
        }
    }

    fn seed_minimal(root: &Path) {
        let envelope = plan_repository(&request(root, InitMode::Minimal)).unwrap();
        assert!(apply_manifest(&envelope).ok);
        commit_all(root);
    }

    fn seed_full_with_extra_pages(root: &Path) {
        let envelope = plan_repository(&request(root, InitMode::Full)).unwrap();
        assert!(apply_manifest(&envelope).ok);
        for title in ["Retained alpha", "Retained beta"] {
            let rendered = crate::render_create(
                &crate::CreateRequest {
                    title: title.to_owned(),
                    page_type: PageType::ProjectState,
                    owner: crate::Owner::Shared,
                    fact: format!("{title} remains retained."),
                    why: "It exercises full-corpus candidate preservation.".to_owned(),
                    how_to_apply: "Keep this page byte-identical.".to_owned(),
                    falsified_by: "Initialization changes this page.".to_owned(),
                    summary: format!("{title} is retained."),
                    related: Vec::new(),
                },
                "2026-08-12",
            )
            .unwrap();
            let slug = crate::parse_wiki_page(&rendered).unwrap().slug;
            fs::write(
                root.join(format!(".agents/memory/pages/{slug}.md")),
                rendered,
            )
            .unwrap();
        }
        crate::reindex_wiki(
            &root.join(".agents/memory"),
            &crate::ReindexOptions::default(),
        )
        .unwrap();
        commit_all(root);
    }

    fn full_upgrade(root: &Path) -> ManifestEnvelope {
        let mut request = request(root, InitMode::Full);
        request.agents_md = format!("# Changed instructions\n\n{AGENT_POLICY}");
        plan_repository(&request).unwrap()
    }

    #[test]
    fn apply_refuses_when_the_wiki_lock_is_busy() {
        let root = repository();
        seed_minimal(root.path());
        let memory = root.path().join(".agents/memory");
        let _holder = match acquire_lock(&memory, LockMode::Exclusive).unwrap() {
            LockLease::Isolated(guard) => guard,
            LockLease::Unisolated(_) => panic!("seeded memory must take an exclusive lock"),
        };

        let outcome = apply_manifest_classified_with_hooks(
            &full_upgrade(root.path()),
            &mut TestHooks {
                immediate_wiki_lock: true,
                ..TestHooks::default()
            },
        );

        assert_eq!(outcome.class, ApplyExitClass::Operational);
        assert!(!outcome.result.ok);
        let error = outcome.result.error.expect("busy apply reports an error");
        assert!(error.contains("busy") || error.contains("lock"), "{error}");
        assert!(!root.path().join(".agents/memory/SCHEMA.md").exists());
    }

    #[test]
    fn apply_refuses_an_unisolated_wiki_lock() {
        let root = repository();
        seed_minimal(root.path());
        let memory = root.path().join(".agents/memory");
        let pages = memory.join("pages");
        fs::create_dir(&pages).unwrap();
        let _ = fs::remove_file(memory.join(crate::LOCK_NAME));
        fs::set_permissions(&pages, fs::Permissions::from_mode(0o555)).unwrap();
        fs::set_permissions(&memory, fs::Permissions::from_mode(0o555)).unwrap();

        let outcome = apply_manifest_classified_with_hooks(
            &full_upgrade(root.path()),
            &mut TestHooks::default(),
        );

        fs::set_permissions(&memory, fs::Permissions::from_mode(0o755)).unwrap();
        fs::set_permissions(&pages, fs::Permissions::from_mode(0o755)).unwrap();

        assert_eq!(outcome.class, ApplyExitClass::Operational);
        assert!(!outcome.result.ok);
        let error = outcome
            .result
            .error
            .expect("unisolated apply reports an error");
        assert!(
            error.contains("unisolated")
                || error.contains("not writable")
                || error.contains("lock"),
            "{error}"
        );
    }

    #[test]
    fn apply_keeps_an_existing_wiki_lock_through_recovery() {
        let root = repository();
        seed_minimal(root.path());
        let memory = root.path().join(".agents/memory");
        let held = Arc::new(AtomicBool::new(false));
        let flag = held.clone();
        let lock_path = memory.clone();
        let outcome = apply_manifest_classified_with_hooks(
            &full_upgrade(root.path()),
            &mut TestHooks {
                fail_final: true,
                after_failure_action: Some(Box::new(move || {
                    flag.store(
                        matches!(
                            acquire_lock_with_timeout(
                                &lock_path,
                                LockMode::Exclusive,
                                Duration::from_millis(50),
                            ),
                            Err(LockError::Busy {
                                mode: LockMode::Exclusive,
                                ..
                            })
                        ),
                        Ordering::SeqCst,
                    );
                })),
                ..TestHooks::default()
            },
        );

        assert!(!outcome.result.ok, "{:?}", outcome.result);
        assert!(
            held.load(Ordering::SeqCst),
            "existing exclusive wiki lock was released before recovery"
        );
    }

    #[test]
    fn classified_apply_distinguishes_success_from_manifest_refusal() {
        let root = repository();
        let valid = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut unsupported = valid.clone();
        unsupported.manifest.manifest_contract += 1;
        resign(&mut unsupported);

        let refused = apply_manifest_classified(&unsupported);
        assert_eq!(refused.class, ApplyExitClass::Usage);
        assert!(!refused.result.ok);
        assert!(!root.path().join(".agents").exists());
        assert!(
            serde_json::to_value(&refused.result)
                .unwrap()
                .get("class")
                .is_none()
        );

        let applied = apply_manifest_classified(&valid);
        assert_eq!(applied.class, ApplyExitClass::Success);
        assert!(applied.result.ok, "{:?}", applied.result);
    }

    #[test]
    fn classified_apply_marks_mutation_io_failures_operational() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_file_fsync: true,
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Operational);
        assert!(!outcome.result.ok);
    }

    #[test]
    fn classified_apply_marks_parent_verification_io_operational() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_parent_verification: true,
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Operational);
        assert!(!outcome.result.ok);
        assert!(!root.path().join(".agents").exists());
    }

    #[test]
    fn classified_apply_marks_final_name_competitor_as_usage() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let competitor = root.path().join("AGENTS.md");
        let mut hooks = TestHooks {
            before_create_file_open_action: Some(Box::new(move || {
                fs::write(competitor, "foreign competitor\n").unwrap();
            })),
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Usage);
        assert!(!outcome.result.ok);
        assert_eq!(
            fs::read(root.path().join("AGENTS.md")).unwrap(),
            b"foreign competitor\n"
        );
    }

    #[test]
    fn missing_captures_with_equal_complete_prestates_match() {
        let expected = super::missing_capture("missing");

        assert!(super::captured_matches(expected.clone(), &expected));
    }

    #[test]
    fn unchanged_directory_install_with_injected_syscall_io_is_operational() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_directory_install: true,
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Operational);
        assert!(!outcome.result.ok);
        assert!(!root.path().join(".agents").exists());
    }

    #[test]
    fn recovery_io_failure_upgrades_a_primary_usage_refusal_to_operational() {
        use std::os::unix::fs::PermissionsExt;

        let root = repository();
        seed_minimal(root.path());
        let memory = root.path().join(".agents/memory");
        fs::set_permissions(&memory, fs::Permissions::from_mode(0o555)).unwrap();
        let envelope = full_upgrade(root.path());
        let competitor = memory.join("pages");
        let mut hooks = TestHooks {
            before_directory_install_action: Some(Box::new(move || {
                fs::create_dir(competitor).unwrap();
            })),
            fail_normal_parent_restore_fsync: true,
            fail_recovery_parent_restore_fsync: true,
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Operational);
        assert!(!outcome.result.ok);
        assert!(outcome.result.error.unwrap().contains("install"));
        assert!(root.path().join(".agents/memory/pages").is_dir());
    }

    #[test]
    fn primary_usage_and_failed_normal_parent_restoration_is_operational() {
        use std::os::unix::fs::PermissionsExt;

        let root = repository();
        seed_minimal(root.path());
        let memory = root.path().join(".agents/memory");
        fs::set_permissions(&memory, fs::Permissions::from_mode(0o555)).unwrap();
        let envelope = full_upgrade(root.path());
        let competitor = memory.join("pages");
        let mut hooks = TestHooks {
            before_directory_install_action: Some(Box::new(move || {
                fs::create_dir(competitor).unwrap();
            })),
            fail_normal_parent_restore_fsync: true,
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Operational);
        let error = outcome.result.error.unwrap();
        assert!(error.contains("install"), "{error}");
        assert!(error.contains("restoring parent mode"), "{error}");
    }

    #[test]
    fn primary_usage_and_failed_restore_temporary_cleanup_is_operational() {
        use std::os::fd::AsFd;

        let root = repository();
        let target = root.path().join("target");
        fs::write(&target, "approved\n").unwrap();
        let parent = rustix::fs::open(
            root.path(),
            super::DIRECTORY_FLAGS,
            rustix::fs::Mode::empty(),
        )
        .unwrap();
        let expected =
            super::capture_named(parent.as_fd(), OsStr::new("target"), "target").unwrap();
        let mut hooks = TestHooks {
            after_file_fsync_action: Some(Box::new(move || {
                fs::write(target, "foreign\n").unwrap();
            })),
            fail_restore_temp_cleanup: true,
            ..TestHooks::default()
        };

        let failure = super::restore_file_atomic(
            parent.as_fd(),
            OsStr::new("target"),
            "target",
            b"original\n",
            0o644,
            &expected,
            &mut hooks,
        )
        .unwrap_err();

        assert_eq!(failure.class, ApplyExitClass::Operational);
        assert!(failure.message.contains("recovery target drifted"));
        assert!(failure.message.contains("temporary cleanup"));
    }

    #[test]
    fn final_root_verification_io_upgrades_a_clean_usage_recovery() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            usage_failure_before_operation: Some(1),
            fail_final_root_verification: true,
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Operational);
        assert!(!outcome.result.ok);
        assert!(!root.path().join(".agents").exists());
    }

    #[test]
    fn clean_recovery_from_a_pure_usage_refusal_stays_usage() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            usage_failure_before_operation: Some(1),
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Usage);
        assert!(!outcome.result.ok);
        assert!(!root.path().join(".agents").exists());
    }

    #[test]
    fn guarded_foreign_recovery_drift_keeps_a_primary_usage_refusal() {
        use std::os::unix::fs::PermissionsExt;

        let root = repository();
        seed_minimal(root.path());
        let memory = root.path().join(".agents/memory");
        fs::set_permissions(&memory, fs::Permissions::from_mode(0o555)).unwrap();
        let envelope = full_upgrade(root.path());
        let competitor = memory.join("pages");
        let mut hooks = TestHooks {
            before_directory_install_action: Some(Box::new(move || {
                fs::create_dir(competitor).unwrap();
            })),
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Usage);
        assert!(!outcome.result.ok);
        assert!(root.path().join(".agents/memory/pages").is_dir());
    }

    #[test]
    fn replace_target_changed_to_directory_at_the_syscall_seam_is_usage() {
        let root = repository();
        seed_minimal(root.path());
        let envelope = full_upgrade(root.path());
        let target = root.path().join("AGENTS.md");
        let target_for_hook = target.clone();
        let mut hooks = TestHooks {
            before_replace_install_action: Some((
                "AGENTS.md".to_owned(),
                Box::new(move || {
                    fs::remove_file(&target_for_hook).unwrap();
                    fs::create_dir(&target_for_hook).unwrap();
                }),
            )),
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Usage);
        assert!(!outcome.result.ok);
        assert!(target.is_dir());
    }

    #[test]
    fn remove_target_changed_to_directory_at_the_syscall_seam_is_usage() {
        let root = repository();
        seed_minimal(root.path());
        let envelope = full_upgrade(root.path());
        let target = root.path().join(".agents/memory/project-context.md");
        let target_for_hook = target.clone();
        let mut hooks = TestHooks {
            before_remove_action: Some(Box::new(move || {
                fs::remove_file(&target_for_hook).unwrap();
                fs::create_dir(&target_for_hook).unwrap();
            })),
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Usage);
        assert!(!outcome.result.ok);
        assert!(target.is_dir());
    }

    #[test]
    fn unchanged_remove_target_with_injected_syscall_io_is_operational() {
        let root = repository();
        seed_minimal(root.path());
        let envelope = full_upgrade(root.path());
        let target = root.path().join(".agents/memory/project-context.md");
        let before = fs::read(&target).unwrap();
        let mut hooks = TestHooks {
            fail_remove: true,
            ..TestHooks::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);

        assert_eq!(outcome.class, ApplyExitClass::Operational);
        assert!(!outcome.result.ok);
        assert_eq!(fs::read(target).unwrap(), before);
    }

    #[test]
    fn applies_absent_repository_to_minimal() {
        let root = repository();
        fs::write(root.path().join("unrelated.txt"), "preserve me\n").unwrap();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();

        let result = apply_manifest(&envelope);

        assert!(result.ok, "{result:?}");
        assert!(result.validated);
        assert_eq!(result.final_layout, LayoutClass::Minimal);
        assert_eq!(result.next, ["yams --index"]);
        assert_eq!(
            fs::read_to_string(root.path().join("unrelated.txt")).unwrap(),
            "preserve me\n"
        );
        assert_eq!(
            inspect_repository(root.path()).unwrap().layout,
            LayoutClass::Minimal
        );
        assert!(directory_temps(root.path()).is_empty());
    }

    #[test]
    fn applies_absent_repository_to_full_and_keeps_runtime_lock_outside_candidate() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Full)).unwrap();
        let result = apply_manifest(&envelope);
        assert!(result.ok, "{result:?}");
        assert_eq!(result.final_layout, LayoutClass::Full);
        assert!(root.path().join(".agents/memory/.write.lock").exists());
        assert!(
            envelope
                .manifest
                .operations
                .iter()
                .all(|operation| operation.path != ".agents/memory/.write.lock")
        );
        assert_eq!(
            inspect_repository(root.path()).unwrap().layout,
            LayoutClass::Full
        );
        assert!(directory_temps(root.path()).is_empty());
    }

    #[test]
    fn matching_rerun_is_a_true_owned_noop() {
        let root = repository();
        let first = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        assert!(apply_manifest(&first).ok);
        commit_all(root.path());
        let before = fs::metadata(root.path().join("AGENTS.md"))
            .unwrap()
            .modified()
            .unwrap();
        let second = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        assert!(second.manifest.operations.is_empty());
        let result = apply_manifest(&second);
        assert!(result.ok, "{result:?}");
        assert!(
            result.created.is_empty() && result.changed.is_empty() && result.removed.is_empty()
        );
        assert_eq!(
            fs::metadata(root.path().join("AGENTS.md"))
                .unwrap()
                .modified()
                .unwrap(),
            before
        );
    }

    #[test]
    fn full_noop_preserves_all_retained_pages_and_runtime_lock() {
        let root = repository();
        seed_full_with_extra_pages(root.path());
        let alpha = root.path().join(".agents/memory/pages/retained-alpha.md");
        let beta = root.path().join(".agents/memory/pages/retained-beta.md");
        let lock = root.path().join(".agents/memory/.write.lock");
        let before = [fs::read(&alpha).unwrap(), fs::read(&beta).unwrap()];
        let lock_before = fs::metadata(&lock).unwrap();
        let envelope = plan_repository(&request(root.path(), InitMode::Full)).unwrap();
        assert!(envelope.manifest.operations.is_empty());
        let result = apply_manifest(&envelope);
        assert!(result.ok, "{result:?}");
        assert_eq!(
            [fs::read(&alpha).unwrap(), fs::read(&beta).unwrap()],
            before
        );
        let lock_after = fs::metadata(&lock).unwrap();
        assert_eq!(
            lock_before.modified().unwrap(),
            lock_after.modified().unwrap()
        );
        assert_eq!(lock_before.len(), lock_after.len());
    }

    #[test]
    fn full_project_context_replacement_keeps_extra_pages_in_candidate_digest() {
        let root = repository();
        seed_full_with_extra_pages(root.path());
        let alpha = root.path().join(".agents/memory/pages/retained-alpha.md");
        let beta = root.path().join(".agents/memory/pages/retained-beta.md");
        let before = [fs::read(&alpha).unwrap(), fs::read(&beta).unwrap()];
        let mut changed = request(root.path(), InitMode::Full);
        changed.project_page.fact = "The project context was deliberately updated.".to_owned();
        let envelope = plan_repository(&changed).unwrap();
        let result = apply_manifest(&envelope);
        assert!(result.ok, "{result:?}");
        assert_eq!(
            [fs::read(&alpha).unwrap(), fs::read(&beta).unwrap()],
            before
        );
    }

    #[test]
    fn rejects_contract_tampering_before_target_access() {
        let root = repository();
        let mut envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        envelope.manifest.manifest_contract = 99;
        envelope.manifest.root = root
            .path()
            .join("does-not-exist")
            .to_str()
            .unwrap()
            .to_owned();
        resign(&mut envelope);
        let result = apply_manifest(&envelope);
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("unsupported"));
        assert!(!root.path().join(".agents").exists());
    }

    #[test]
    fn rejects_operation_tampering_before_target_access() {
        let root = repository();
        let mut envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let file = envelope
            .manifest
            .operations
            .iter_mut()
            .find(|op| op.content.is_some())
            .unwrap();
        file.content = Some("tampered".to_owned());
        resign(&mut envelope);
        let result = apply_manifest(&envelope);
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("internally inconsistent"));
        assert!(!root.path().join(".agents").exists());
    }

    #[test]
    fn detects_drift_before_first_write() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        fs::write(root.path().join("AGENTS.md"), "foreign\n").unwrap();
        let result = apply_manifest(&envelope);
        assert!(!result.ok);
        assert!(result.created.is_empty());
        assert!(!root.path().join(".agents").exists());
        assert_eq!(
            fs::read_to_string(root.path().join("AGENTS.md")).unwrap(),
            "foreign\n"
        );
    }

    #[test]
    fn later_target_drift_recovers_earlier_creates() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let target = root.path().join("AGENTS.md");
        let mut hooks = TestHooks {
            before_action: Some((2, Box::new(move || fs::write(target, "foreign\n").unwrap()))),
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert_eq!(
            result,
            crate::ApplyResult {
                ok: false,
                manifest_sha256: envelope.manifest_sha256.clone(),
                created: vec![".agents".to_owned(), ".agents/memory".to_owned()],
                changed: Vec::new(),
                removed: Vec::new(),
                restored: vec![".agents".to_owned(), ".agents/memory".to_owned()],
                unresolved: Vec::new(),
                final_layout: LayoutClass::Absent,
                validated: false,
                error: Some("approved repository state drifted at AGENTS.md".to_owned()),
                next: Vec::new(),
            }
        );
        assert_eq!(
            fs::read_to_string(root.path().join("AGENTS.md")).unwrap(),
            "foreign\n"
        );
        assert!(!root.path().join(".agents").exists());
    }

    #[test]
    fn failure_after_create_recovers_all_run_owned_nodes() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_after: Some(2),
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert!(!root.path().join("AGENTS.md").exists());
        assert!(!root.path().join(".agents").exists());
        assert!(result.unresolved.is_empty(), "{result:?}");
    }

    #[test]
    fn final_validation_failure_recovers_every_mutation() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_final: true,
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert_eq!(
            result,
            crate::ApplyResult {
                ok: false,
                manifest_sha256: envelope.manifest_sha256.clone(),
                created: vec![
                    ".agents".to_owned(),
                    ".agents/memory".to_owned(),
                    ".agents/memory/.gitignore".to_owned(),
                    ".agents/memory/project-context.md".to_owned(),
                    "AGENTS.md".to_owned(),
                ],
                changed: Vec::new(),
                removed: Vec::new(),
                restored: vec![
                    ".agents".to_owned(),
                    ".agents/memory".to_owned(),
                    ".agents/memory/.gitignore".to_owned(),
                    ".agents/memory/project-context.md".to_owned(),
                    "AGENTS.md".to_owned(),
                ],
                unresolved: Vec::new(),
                final_layout: LayoutClass::Absent,
                validated: false,
                error: Some("injected final validation failure".to_owned()),
                next: Vec::new(),
            }
        );
        assert!(!root.path().join(".agents").exists());
        assert!(!root.path().join("AGENTS.md").exists());
    }

    #[test]
    fn recovery_classifies_absent_with_unrelated_agents_and_harness_paths() {
        let root = repository();
        fs::write(root.path().join("AGENTS.md"), "# Unrelated agent notes\n").unwrap();
        fs::create_dir(root.path().join(".agents")).unwrap();
        fs::create_dir(root.path().join(".agents/harness-cache")).unwrap();
        fs::write(
            root.path().join(".agents/harness-cache/state"),
            "unrelated\n",
        )
        .unwrap();
        commit_all(root.path());
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_final: true,
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert_eq!(result.final_layout, LayoutClass::Absent);
        assert!(result.unresolved.is_empty(), "{result:?}");
        assert_eq!(
            fs::read_to_string(root.path().join("AGENTS.md")).unwrap(),
            "# Unrelated agent notes\n"
        );
        assert_eq!(
            fs::read_to_string(root.path().join(".agents/harness-cache/state")).unwrap(),
            "unrelated\n"
        );
        assert!(!root.path().join(".agents/memory").exists());
    }

    #[test]
    fn recovery_never_deletes_a_foreign_replacement() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let target = root.path().join("AGENTS.md");
        let mut hooks = TestHooks {
            fail_after: Some(2),
            after_failure_action: Some(Box::new(move || fs::write(target, "foreign\n").unwrap())),
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert_eq!(
            fs::read_to_string(root.path().join("AGENTS.md")).unwrap(),
            "foreign\n"
        );
        assert!(result.unresolved.contains(&"AGENTS.md".to_owned()));
    }

    #[test]
    fn recovery_never_deletes_an_identical_foreign_replacement() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let target = root.path().join("AGENTS.md");
        let displaced = root.path().join("AGENTS-created-displaced");
        let approved = envelope
            .manifest
            .operations
            .iter()
            .find(|operation| operation.path == "AGENTS.md")
            .unwrap()
            .content
            .clone()
            .unwrap();
        let target_for_hook = target.clone();
        let mut hooks = TestHooks {
            fail_after: Some(2),
            after_failure_action: Some(Box::new(move || {
                fs::rename(&target_for_hook, displaced).unwrap();
                fs::write(&target_for_hook, approved).unwrap();
            })),
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert!(target.exists(), "{result:?}");
        assert!(result.unresolved.contains(&"AGENTS.md".to_owned()));
    }

    #[test]
    fn recovery_never_removes_a_concurrently_populated_directory() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let foreign = root.path().join(".agents/foreign");
        let mut hooks = TestHooks {
            fail_after: Some(0),
            after_failure_action: Some(Box::new(move || fs::write(foreign, "foreign\n").unwrap())),
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert_eq!(
            fs::read_to_string(root.path().join(".agents/foreign")).unwrap(),
            "foreign\n"
        );
        assert!(result.unresolved.contains(&".agents".to_owned()));
    }

    #[test]
    fn directory_fsync_failure_recovers_the_just_created_directory() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_directory_fsync: true,
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert!(!root.path().join(".agents").exists(), "{result:?}");
        assert!(result.unresolved.is_empty(), "{result:?}");
    }

    #[test]
    fn file_fsync_failure_recovers_the_just_created_file() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_file_fsync: true,
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert!(!root.path().join("AGENTS.md").exists(), "{result:?}");
        assert!(!root.path().join(".agents").exists(), "{result:?}");
        assert!(result.unresolved.is_empty(), "{result:?}");
    }

    #[test]
    fn failure_after_replace_restores_original_bytes_and_mode() {
        use std::os::unix::fs::PermissionsExt;
        let root = repository();
        seed_minimal(root.path());
        let original = fs::read(root.path().join("AGENTS.md")).unwrap();
        let original_mode = fs::metadata(root.path().join("AGENTS.md"))
            .unwrap()
            .permissions()
            .mode()
            & 0o7777;
        let envelope = full_upgrade(root.path());
        let replace = envelope
            .manifest
            .operations
            .iter()
            .position(|op| op.path == "AGENTS.md")
            .unwrap();
        let mut hooks = TestHooks {
            fail_after: Some(replace),
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert_eq!(fs::read(root.path().join("AGENTS.md")).unwrap(), original);
        assert_eq!(
            fs::metadata(root.path().join("AGENTS.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            original_mode
        );
        assert!(
            result.restored.contains(&"AGENTS.md".to_owned()),
            "{result:?}"
        );
        assert!(result.unresolved.is_empty(), "{result:?}");
        assert_eq!(result.final_layout, LayoutClass::Minimal);
    }

    #[test]
    fn failure_after_remove_restores_removed_flat_page() {
        let root = repository();
        seed_minimal(root.path());
        let original = fs::read(root.path().join(".agents/memory/project-context.md")).unwrap();
        let envelope = full_upgrade(root.path());
        let remove = envelope
            .manifest
            .operations
            .iter()
            .position(|op| op.path == ".agents/memory/project-context.md")
            .unwrap();
        let mut hooks = TestHooks {
            fail_after: Some(remove),
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert_eq!(
            fs::read(root.path().join(".agents/memory/project-context.md")).unwrap(),
            original
        );
        assert!(
            result
                .restored
                .contains(&".agents/memory/project-context.md".to_owned()),
            "{result:?}"
        );
        assert_eq!(
            inspect_repository(root.path()).unwrap().layout,
            LayoutClass::Minimal
        );
        assert!(result.unresolved.is_empty(), "{result:?}");
        assert_eq!(result.final_layout, LayoutClass::Minimal);
    }

    #[test]
    fn successful_minimal_to_full_upgrade_accounts_for_replace_and_remove() {
        let root = repository();
        seed_minimal(root.path());
        let envelope = full_upgrade(root.path());
        let result = apply_manifest(&envelope);
        assert!(result.ok, "{result:?}");
        assert!(result.changed.contains(&"AGENTS.md".to_owned()));
        assert!(
            result
                .removed
                .contains(&".agents/memory/project-context.md".to_owned())
        );
        assert_eq!(
            inspect_repository(root.path()).unwrap().layout,
            LayoutClass::Full
        );
    }

    #[test]
    fn candidate_digest_tampering_is_rejected_before_mutation() {
        let root = repository();
        let mut envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        envelope.manifest.candidate_sha256 = "0".repeat(64);
        resign(&mut envelope);
        let result = apply_manifest(&envelope);
        assert!(!result.ok);
        assert!(result.error.as_ref().unwrap().contains("candidate digest"));
        assert!(!root.path().join(".agents").exists());
        assert!(
            result.created.is_empty() && result.changed.is_empty() && result.removed.is_empty()
        );
        assert!(result.unresolved.is_empty(), "{result:?}");
    }

    #[test]
    fn symlink_and_hardlink_drift_are_rejected_without_writes() {
        use std::os::unix::fs::symlink;
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        symlink("foreign", root.path().join(".agents")).unwrap();
        let result = apply_manifest(&envelope);
        assert!(!result.ok);
        assert!(result.created.is_empty());
        fs::remove_file(root.path().join(".agents")).unwrap();

        seed_minimal(root.path());
        let envelope = full_upgrade(root.path());
        fs::hard_link(
            root.path().join("AGENTS.md"),
            root.path().join("hardlink-copy"),
        )
        .unwrap();
        let result = apply_manifest(&envelope);
        assert!(!result.ok);
        assert!(result.created.is_empty());
        assert!(!root.path().join(".agents/memory/pages").exists());
    }

    #[test]
    fn full_upgrade_temporarily_handles_a_read_only_memory_parent() {
        use std::os::unix::fs::PermissionsExt;
        let root = repository();
        seed_minimal(root.path());
        fs::set_permissions(
            root.path().join(".agents/memory"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let envelope = full_upgrade(root.path());
        let result = apply_manifest(&envelope);
        assert!(result.ok, "{result:?}");
        assert_eq!(
            fs::metadata(root.path().join(".agents/memory"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o555
        );
    }

    #[test]
    fn rejects_digest_asset_version_path_and_order_tampering_before_access() {
        let root = repository();
        let original = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();

        let mut cases = Vec::new();
        let mut digest = original.clone();
        digest.manifest_sha256 = "0".repeat(64);
        cases.push(digest);
        let mut asset = original.clone();
        asset
            .manifest
            .asset_sha256
            .insert("SCHEMA.md".to_owned(), "0".repeat(64));
        resign(&mut asset);
        cases.push(asset);
        let mut version = original.clone();
        version.manifest.yams_version = "999.0.0".to_owned();
        resign(&mut version);
        cases.push(version);
        let mut path = original.clone();
        path.manifest.operations[0].path = "../escape".to_owned();
        path.manifest.operations[0].prestate.path = "../escape".to_owned();
        resign(&mut path);
        cases.push(path);
        let mut order = original;
        order.manifest.operations.swap(0, 1);
        order.manifest.proposal = order
            .manifest
            .operations
            .iter()
            .map(super::proposal_line)
            .collect::<Vec<_>>()
            .join("\n");
        resign(&mut order);
        cases.push(order);

        for envelope in cases {
            let result = apply_manifest(&envelope);
            assert!(!result.ok, "{result:?}");
            assert!(!root.path().join(".agents").exists(), "{result:?}");
            assert!(!root.path().join("AGENTS.md").exists(), "{result:?}");
        }
    }

    #[test]
    fn canonical_root_alias_is_rejected_without_mutating_the_repository() {
        use std::os::unix::fs::symlink;
        let root = repository();
        let alias_parent = tempfile::tempdir().unwrap();
        let alias = alias_parent.path().join("alias");
        symlink(root.path(), &alias).unwrap();
        let mut envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        envelope.manifest.root = alias.to_str().unwrap().to_owned();
        resign(&mut envelope);
        let result = apply_manifest(&envelope);
        assert!(!result.ok);
        assert!(result.error.unwrap().contains("canonical"));
        assert!(!root.path().join(".agents").exists());
    }

    #[test]
    fn later_parent_rebinding_preserves_the_foreign_directory() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let root_path = root.path().to_owned();
        let mut hooks = TestHooks {
            before_action: Some((
                1,
                Box::new(move || {
                    fs::rename(
                        root_path.join(".agents"),
                        root_path.join(".agents-run-owned"),
                    )
                    .unwrap();
                    fs::create_dir(root_path.join(".agents")).unwrap();
                    fs::write(root_path.join(".agents/foreign"), "foreign\n").unwrap();
                }),
            )),
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert_eq!(
            fs::read_to_string(root.path().join(".agents/foreign")).unwrap(),
            "foreign\n"
        );
        assert!(
            result.unresolved.contains(&".agents".to_owned()),
            "{result:?}"
        );
    }

    #[test]
    fn failure_immediately_after_mkdir_is_journaled_and_recovered() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_after_mkdir: true,
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert!(!root.path().join(".agents").exists(), "{result:?}");
        let temporary = directory_temps(root.path()).pop().unwrap();
        let temporary_relative = temporary.file_name().unwrap().to_str().unwrap().to_owned();
        assert!(
            result.unresolved.contains(&temporary_relative),
            "{result:?}"
        );
        assert_eq!(result.final_layout, LayoutClass::Partial);
    }

    #[test]
    fn directory_rebind_at_mkdir_seam_never_chmods_or_removes_foreign_node() {
        use std::os::unix::fs::PermissionsExt;
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let root_path = root.path().to_owned();
        let mut hooks = TestHooks {
            after_mkdir_action: Some(Box::new(move || {
                fs::rename(
                    root_path.join(".agents"),
                    root_path.join(".agents-displaced"),
                )
                .unwrap();
                fs::create_dir(root_path.join(".agents")).unwrap();
                fs::write(root_path.join(".agents/foreign"), "foreign\n").unwrap();
                fs::set_permissions(root_path.join(".agents"), fs::Permissions::from_mode(0o500))
                    .unwrap();
            })),
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert_eq!(
            fs::read(root.path().join(".agents/foreign")).unwrap(),
            b"foreign\n"
        );
        assert_eq!(
            fs::metadata(root.path().join(".agents"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o500
        );
        assert!(root.path().join(".agents-displaced").exists());
        assert!(
            result.unresolved.contains(&".agents".to_owned()),
            "{result:?}"
        );
        assert_eq!(result.final_layout, LayoutClass::Partial);
    }

    #[test]
    fn directory_temp_rebind_before_pin_is_foreign_and_unresolved() {
        use std::os::unix::fs::PermissionsExt;
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let root_path = root.path().to_owned();
        let displaced = root.path().join("displaced-run-owned-directory");
        let displaced_for_hook = displaced.clone();
        let mut hooks = TestHooks {
            after_directory_temp_action: Some(Box::new(move || {
                let temporary = directory_temps(&root_path).pop().unwrap();
                fs::rename(&temporary, &displaced_for_hook).unwrap();
                fs::create_dir(&temporary).unwrap();
                fs::write(temporary.join("foreign"), "foreign\n").unwrap();
                fs::set_permissions(&temporary, fs::Permissions::from_mode(0o500)).unwrap();
            })),
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        let temporary = directory_temps(root.path()).pop().unwrap();
        let temporary_relative = temporary.file_name().unwrap().to_str().unwrap().to_owned();
        assert_eq!(fs::read(temporary.join("foreign")).unwrap(), b"foreign\n");
        assert_eq!(
            fs::metadata(temporary).unwrap().permissions().mode() & 0o7777,
            0o500
        );
        assert!(displaced.exists());
        assert!(
            result.unresolved.contains(&temporary_relative),
            "{result:?}"
        );
    }

    #[test]
    fn exclusive_directory_install_preserves_final_name_competitor() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let foreign = root.path().join(".agents/foreign");
        let foreign_for_hook = foreign.clone();
        let mut hooks = TestHooks {
            before_directory_install_action: Some(Box::new(move || {
                fs::create_dir(foreign_for_hook.parent().unwrap()).unwrap();
                fs::write(&foreign_for_hook, "foreign\n").unwrap();
            })),
            ..Default::default()
        };

        let outcome = apply_manifest_classified_with_hooks(&envelope, &mut hooks);
        let result = outcome.result;

        assert_eq!(outcome.class, ApplyExitClass::Usage);
        assert!(!result.ok, "{result:?}");
        assert_eq!(fs::read(foreign).unwrap(), b"foreign\n");
        assert!(directory_temps(root.path()).is_empty(), "{result:?}");
    }

    #[test]
    fn directory_temp_rebind_before_exclusive_install_is_not_relocated() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let root_path = root.path().to_owned();
        let displaced = root.path().join("pinned-directory-displaced");
        let displaced_for_hook = displaced.clone();
        let mut hooks = TestHooks {
            before_directory_install_action: Some(Box::new(move || {
                let temporary = directory_temps(&root_path).pop().unwrap();
                fs::rename(&temporary, &displaced_for_hook).unwrap();
                fs::create_dir(&temporary).unwrap();
                fs::write(temporary.join("foreign"), "foreign\n").unwrap();
            })),
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert!(!root.path().join(".agents").exists(), "{result:?}");
        let foreign_temporary = directory_temps(root.path()).pop().unwrap();
        assert_eq!(
            fs::read(foreign_temporary.join("foreign")).unwrap(),
            b"foreign\n"
        );
        assert!(displaced.exists());
        assert_eq!(result.final_layout, LayoutClass::Partial);
    }

    #[test]
    fn failure_immediately_after_file_open_is_journaled_and_recovered() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            fail_after_file_open: true,
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert!(!root.path().join("AGENTS.md").exists(), "{result:?}");
        assert!(
            result.restored.contains(&"AGENTS.md".to_owned()),
            "{result:?}"
        );
        assert!(result.unresolved.is_empty(), "{result:?}");
        assert_eq!(result.final_layout, LayoutClass::Absent);
    }

    #[test]
    fn file_rebind_at_open_seam_preserves_foreign_and_displaced_created_inode() {
        use std::os::unix::fs::PermissionsExt;
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let root_path = root.path().to_owned();
        let mut hooks = TestHooks {
            after_file_open_action: Some(Box::new(move || {
                fs::rename(
                    root_path.join("AGENTS.md"),
                    root_path.join("AGENTS-created-displaced"),
                )
                .unwrap();
                fs::write(root_path.join("AGENTS.md"), "foreign\n").unwrap();
                fs::set_permissions(
                    root_path.join("AGENTS.md"),
                    fs::Permissions::from_mode(0o400),
                )
                .unwrap();
            })),
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert_eq!(
            fs::read(root.path().join("AGENTS.md")).unwrap(),
            b"foreign\n"
        );
        assert_eq!(
            fs::metadata(root.path().join("AGENTS.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o400
        );
        assert!(root.path().join("AGENTS-created-displaced").exists());
        assert!(
            result.unresolved.contains(&"AGENTS.md".to_owned()),
            "{result:?}"
        );
        assert_eq!(result.final_layout, LayoutClass::Partial);
    }

    #[test]
    fn failure_immediately_after_replace_rename_restores_original() {
        let root = repository();
        seed_minimal(root.path());
        let original = fs::read(root.path().join("AGENTS.md")).unwrap();
        let envelope = full_upgrade(root.path());
        let mut hooks = TestHooks {
            fail_after_rename: true,
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert_eq!(fs::read(root.path().join("AGENTS.md")).unwrap(), original);
        assert!(
            result.restored.contains(&"AGENTS.md".to_owned()),
            "{result:?}"
        );
        assert!(result.unresolved.is_empty(), "{result:?}");
    }

    #[test]
    fn failure_after_replace_temporary_open_is_journaled_and_cleans_residue() {
        let root = repository();
        seed_minimal(root.path());
        let original = fs::read(root.path().join("AGENTS.md")).unwrap();
        let envelope = full_upgrade(root.path());
        let mut hooks = TestHooks {
            fail_file_open_path: Some("AGENTS.md".to_owned()),
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert_eq!(fs::read(root.path().join("AGENTS.md")).unwrap(), original);
        assert_eq!(result.final_layout, LayoutClass::Minimal);
        assert!(result.unresolved.is_empty(), "{result:?}");
        assert!(
            fs::read_dir(root.path()).unwrap().all(|entry| {
                !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("yams-apply")
            }),
            "{result:?}"
        );
    }

    #[test]
    fn replacement_temp_cleanup_fsync_failure_is_unresolved() {
        let root = repository();
        seed_minimal(root.path());
        let original = fs::read(root.path().join("AGENTS.md")).unwrap();
        let envelope = full_upgrade(root.path());
        let mut hooks = TestHooks {
            fail_file_open_path: Some("AGENTS.md".to_owned()),
            fail_replaced_temp_cleanup_fsync: true,
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert_eq!(fs::read(root.path().join("AGENTS.md")).unwrap(), original);
        assert!(
            result.unresolved.contains(&"AGENTS.md".to_owned()),
            "{result:?}"
        );
        assert_eq!(result.final_layout, LayoutClass::Partial);
    }

    #[test]
    fn root_replacement_before_final_validation_never_switches_trees() {
        let root = repository();
        let root_path = root.path().to_owned();
        let detached = root.path().with_extension("run-owned-detached");
        let detached_for_hook = detached.clone();
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
        let mut hooks = TestHooks {
            before_final_action: Some(Box::new(move || {
                fs::rename(&root_path, &detached_for_hook).unwrap();
                fs::create_dir(&root_path).unwrap();
                fs::create_dir(root_path.join(".git")).unwrap();
                fs::write(root_path.join("foreign"), "foreign\n").unwrap();
            })),
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert_eq!(
            fs::read_to_string(root.path().join("foreign")).unwrap(),
            "foreign\n"
        );
        assert!(!detached.join(".agents").exists(), "{result:?}");
        assert!(result.unresolved.contains(&".".to_owned()), "{result:?}");
        assert_eq!(result.final_layout, LayoutClass::Partial);
    }

    #[test]
    fn minimal_final_validation_rejects_each_concurrent_structured_fragment() {
        for (path, directory) in [
            (".agents/memory/SCHEMA.md", false),
            (".agents/memory/INDEX.md", false),
            (".agents/memory/pages", true),
        ] {
            let root = repository();
            let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();
            let fragment = root.path().join(path);
            let hook_fragment = fragment.clone();
            let mut hooks = TestHooks {
                before_final_action: Some(Box::new(move || {
                    if directory {
                        fs::create_dir(&hook_fragment).unwrap();
                    } else {
                        fs::write(&hook_fragment, "foreign structured fragment\n").unwrap();
                    }
                })),
                ..Default::default()
            };

            let result = apply_manifest_with_hooks(&envelope, &mut hooks);

            assert!(!result.ok, "{path}: {result:?}");
            assert!(!result.validated, "{path}: {result:?}");
            assert_eq!(
                result.final_layout,
                LayoutClass::Partial,
                "{path}: {result:?}"
            );
            assert!(fragment.exists(), "{path}: foreign fragment was removed");
            assert!(
                result.unresolved.contains(&path.to_owned()),
                "{path}: {result:?}"
            );
        }
    }

    #[test]
    fn full_final_revalidation_rejects_late_extra_page_membership() {
        let root = repository();
        let envelope = plan_repository(&request(root.path(), InitMode::Full)).unwrap();
        let late = root.path().join(".agents/memory/pages/late-foreign.md");
        let late_for_hook = late.clone();
        let mut hooks = TestHooks {
            during_final_action: Some(Box::new(move || {
                fs::write(late_for_hook, "late foreign page\n").unwrap();
            })),
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert!(!result.validated, "{result:?}");
        assert_eq!(result.final_layout, LayoutClass::Partial);
        assert!(late.exists(), "{result:?}");
    }

    #[test]
    fn recovery_classification_detects_runtime_lock_replacement() {
        let root = repository();
        seed_minimal(root.path());
        let lock = root.path().join(".agents/memory/.write.lock");
        fs::write(&lock, "safe lock\n").unwrap();
        let displaced = root.path().join(".agents/memory/.write.lock-displaced");
        let lock_for_hook = lock.clone();
        let displaced_for_hook = displaced.clone();
        let envelope = full_upgrade(root.path());
        let mut hooks = TestHooks {
            fail_after: Some(0),
            after_failure_action: Some(Box::new(move || {
                fs::rename(&lock_for_hook, &displaced_for_hook).unwrap();
                fs::write(&lock_for_hook, "foreign lock\n").unwrap();
            })),
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert_eq!(result.final_layout, LayoutClass::Partial);
        assert_eq!(fs::read(&lock).unwrap(), b"foreign lock\n");
        assert_eq!(fs::read(&displaced).unwrap(), b"safe lock\n");
        assert!(
            result
                .unresolved
                .contains(&".agents/memory/.write.lock".to_owned()),
            "{result:?}"
        );
    }

    #[test]
    fn unrelated_marker_like_names_do_not_block_matching_minimal_apply() {
        let root = repository();
        seed_minimal(root.path());
        for name in [
            "notes.yams-dir-00000000000000000000000000000000.tmp",
            ".pages.yams-dir-not-32-lower-hex.tmp",
            ".unknown.yams-apply-12-3.tmp",
            ".project-context.md.yams-apply-x-3.tmp",
        ] {
            fs::write(root.path().join(".agents/memory").join(name), "unrelated\n").unwrap();
        }
        commit_all(root.path());
        let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();

        let result = apply_manifest(&envelope);

        assert!(result.ok, "{result:?}");
        assert_eq!(result.final_layout, LayoutClass::Minimal);
    }

    #[test]
    fn stale_exact_directory_and_file_temporaries_prevent_success() {
        for (name, directory) in [
            (".pages.yams-dir-00000000000000000000000000000000.tmp", true),
            (".project-context.md.yams-apply-123-4.tmp", false),
        ] {
            let root = repository();
            seed_minimal(root.path());
            let relative = format!(".agents/memory/{name}");
            let stale = root.path().join(&relative);
            let tracked = if directory {
                fs::create_dir(&stale).unwrap();
                fs::write(stale.join("foreign"), "stale directory temporary\n").unwrap();
                format!("{relative}/foreign")
            } else {
                fs::write(&stale, "stale apply temporary\n").unwrap();
                relative.clone()
            };
            git(root.path(), &["add", "-f", &tracked]);
            commit_all(root.path());
            let envelope = plan_repository(&request(root.path(), InitMode::Minimal)).unwrap();

            let result = apply_manifest(&envelope);

            assert!(!result.ok, "{relative}: {result:?}");
            assert_eq!(result.final_layout, LayoutClass::Partial);
            assert!(
                result.unresolved.contains(&relative),
                "{relative}: {result:?}"
            );
            assert!(stale.exists());
        }
    }

    #[test]
    fn failed_parent_mode_restore_is_retried_during_recovery() {
        use std::os::unix::fs::PermissionsExt;
        let root = repository();
        seed_minimal(root.path());
        fs::set_permissions(
            root.path().join(".agents/memory"),
            fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        let envelope = full_upgrade(root.path());
        let mut hooks = TestHooks {
            fail_parent_restore: true,
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert_eq!(
            fs::metadata(root.path().join(".agents/memory"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o555
        );
        assert!(result.unresolved.is_empty(), "{result:?}");
    }

    #[test]
    fn parent_mode_restore_fsync_failure_is_retried_before_resolved() {
        use std::os::unix::fs::PermissionsExt;
        let root = repository();
        seed_minimal(root.path());
        let memory = root.path().join(".agents/memory");
        fs::set_permissions(&memory, fs::Permissions::from_mode(0o555)).unwrap();
        let envelope = full_upgrade(root.path());
        let mut hooks = TestHooks {
            fail_normal_parent_restore_fsync: true,
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert_eq!(
            fs::metadata(memory).unwrap().permissions().mode() & 0o7777,
            0o555
        );
        assert!(result.unresolved.is_empty(), "{result:?}");
        assert!(
            result.restored.contains(&".agents/memory".to_owned()),
            "{result:?}"
        );
        assert_eq!(result.final_layout, LayoutClass::Minimal);
    }

    #[test]
    fn recovery_parent_mode_restore_fsync_failure_remains_unresolved() {
        use std::os::unix::fs::PermissionsExt;
        let root = repository();
        seed_minimal(root.path());
        let memory = root.path().join(".agents/memory");
        fs::set_permissions(&memory, fs::Permissions::from_mode(0o555)).unwrap();
        let envelope = full_upgrade(root.path());
        let mut hooks = TestHooks {
            fail_normal_parent_restore_fsync: true,
            fail_recovery_parent_restore_fsync: true,
            ..Default::default()
        };

        let result = apply_manifest_with_hooks(&envelope, &mut hooks);

        assert!(!result.ok, "{result:?}");
        assert_eq!(
            fs::metadata(memory).unwrap().permissions().mode() & 0o7777,
            0o555
        );
        assert!(
            result.unresolved.contains(&".agents/memory".to_owned()),
            "{result:?}"
        );
        assert_eq!(result.final_layout, LayoutClass::Partial);
    }

    #[test]
    fn foreign_parent_replacement_after_widening_is_untouched_and_unresolved() {
        use std::os::unix::fs::PermissionsExt;
        let root = repository();
        seed_minimal(root.path());
        let memory = root.path().join(".agents/memory");
        fs::set_permissions(&memory, fs::Permissions::from_mode(0o555)).unwrap();
        let detached = root.path().join(".agents/memory-detached");
        let foreign = memory.join("foreign");
        let envelope = full_upgrade(root.path());
        let mut hooks = TestHooks {
            after_parent_widened_action: Some(Box::new(move || {
                fs::rename(&memory, &detached).unwrap();
                fs::create_dir(&memory).unwrap();
                fs::write(&foreign, "foreign\n").unwrap();
            })),
            ..Default::default()
        };
        let result = apply_manifest_with_hooks(&envelope, &mut hooks);
        assert!(!result.ok);
        assert_eq!(
            fs::read_to_string(root.path().join(".agents/memory/foreign")).unwrap(),
            "foreign\n"
        );
        assert!(
            result.unresolved.contains(&".agents/memory".to_owned()),
            "{result:?}"
        );
    }

    #[test]
    fn incomplete_or_invalid_candidates_are_rejected_before_any_mutation() {
        let root = repository();
        let original = plan_repository(&request(root.path(), InitMode::Full)).unwrap();
        let before_root = fs::metadata(root.path()).unwrap().modified().unwrap();

        let mut missing = original.clone();
        missing
            .manifest
            .operations
            .retain(|operation| operation.path != ".agents/memory/SCHEMA.md");
        missing.manifest.proposal = missing
            .manifest
            .operations
            .iter()
            .map(super::proposal_line)
            .collect::<Vec<_>>()
            .join("\n");
        resign(&mut missing);

        let mut invalid_index = original;
        let index = invalid_index
            .manifest
            .operations
            .iter_mut()
            .find(|operation| operation.path == ".agents/memory/INDEX.md")
            .unwrap();
        index.content = Some("invalid index\n".to_owned());
        index.post_sha256 = Some(sha256(index.content.as_ref().unwrap().as_bytes()));
        let candidate = super::build_candidate(
            &super::capture_repository(root.path()).unwrap(),
            &invalid_index.manifest,
        )
        .unwrap();
        invalid_index.manifest.candidate_sha256 = super::owned_candidate_sha256(&candidate);
        resign(&mut invalid_index);

        for envelope in [missing, invalid_index] {
            let result = apply_manifest(&envelope);
            assert!(!result.ok, "{result:?}");
            assert!(
                result.created.is_empty() && result.changed.is_empty() && result.removed.is_empty()
            );
            assert!(!root.path().join(".agents").exists(), "{result:?}");
            assert!(!root.path().join("AGENTS.md").exists(), "{result:?}");
            assert_eq!(
                fs::metadata(root.path()).unwrap().modified().unwrap(),
                before_root
            );
        }
    }
}
