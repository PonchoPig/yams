use std::fs;
use std::path::Path;
use tempfile::tempdir;
use yams_core::{
    CorpusKind, Discovery, DiscoveryNoteKind, corpora_for, discover_corpora, project_root,
    scan_corpora,
};

#[test]
fn git_root_and_shared_corpus_are_discovered_from_a_child() {
    let tmp = tempdir().unwrap();
    fs::create_dir_all(tmp.path().join(".git")).unwrap();
    fs::create_dir_all(tmp.path().join(".agents/memory")).unwrap();
    let child = tmp.path().join("src/deep");
    fs::create_dir_all(&child).unwrap();
    let root = project_root(None, &child).unwrap();
    let corpora = corpora_for(&root, &Discovery::default()).unwrap();
    assert_eq!(corpora[0].kind(), CorpusKind::Shared);
    assert_eq!(corpora[0].path(), root.join(".agents/memory"));
}

#[test]
fn a_git_file_marks_the_nearest_worktree_root() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join(".git"), "gitdir: elsewhere\n").unwrap();
    let child = tmp.path().join("nested/child");
    fs::create_dir_all(&child).unwrap();

    assert_eq!(
        project_root(None, &child).unwrap(),
        tmp.path().canonicalize().unwrap()
    );
}

#[test]
fn an_explicit_relative_root_is_resolved_from_the_supplied_cwd() {
    let tmp = tempdir().unwrap();
    let cwd = tmp.path().join("working/place");
    let project = tmp.path().join("working/project");
    fs::create_dir_all(&cwd).unwrap();
    fs::create_dir_all(&project).unwrap();

    assert_eq!(
        project_root(Some(Path::new("../project")), &cwd).unwrap(),
        project.canonicalize().unwrap()
    );
}

#[test]
fn no_git_marker_falls_back_to_the_canonical_cwd() {
    let tmp = tempdir().unwrap();
    let cwd = tmp.path().join("not/a/repository");
    fs::create_dir_all(&cwd).unwrap();

    assert_eq!(
        project_root(None, &cwd).unwrap(),
        cwd.canonicalize().unwrap()
    );
}

#[test]
fn private_corpus_uses_claudes_ascii_alphanumeric_spelling() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("dots.dir/space here/~tilde/naïve");
    let home = tmp.path().join("disposable-home");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(home.join(".claude/projects")).unwrap();
    let root = root.canonicalize().unwrap();
    let slug: String = root
        .to_str()
        .unwrap()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let private = home.join(".claude/projects").join(slug).join("memory");
    fs::create_dir_all(&private).unwrap();

    let corpora = corpora_for(
        &root,
        &Discovery {
            home: Some(home),
            override_dirs: Vec::new(),
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert_eq!(corpora.len(), 1);
    assert_eq!(corpora[0].kind(), CorpusKind::Private);
    assert_eq!(corpora[0].path(), private.canonicalize().unwrap());
}

#[test]
fn overrides_replace_defaults_preserve_order_and_deduplicate() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    let first = tmp.path().join("outside-first");
    let second = tmp.path().join("outside-second");
    fs::create_dir_all(root.join(".agents/memory")).unwrap();
    fs::create_dir_all(&home).unwrap();
    fs::create_dir_all(&first).unwrap();
    fs::create_dir_all(&second).unwrap();
    let root = root.canonicalize().unwrap();
    let private_slug: String = root
        .to_str()
        .unwrap()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    fs::create_dir_all(
        home.join(".claude/projects")
            .join(private_slug)
            .join("memory"),
    )
    .unwrap();

    let corpora = corpora_for(
        &root,
        &Discovery {
            home: Some(home),
            override_dirs: vec![second.clone(), first.clone(), second.clone()],
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert_eq!(corpora.len(), 2);
    assert_eq!(corpora[0].path(), second.canonicalize().unwrap());
    assert_eq!(corpora[0].kind(), CorpusKind::Override);
    assert_eq!(corpora[1].path(), first.canonicalize().unwrap());
    assert_eq!(corpora[1].kind(), CorpusKind::Override);
}

#[test]
fn missing_default_surfaces_are_omitted_without_reading_a_real_home() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let home = tmp.path().join("empty-home");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&home).unwrap();

    let corpora = corpora_for(
        &root,
        &Discovery {
            home: Some(home),
            override_dirs: Vec::new(),
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert!(corpora.is_empty());
}

#[test]
fn a_non_directory_shared_default_is_omitted_without_hiding_private_memory() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    fs::create_dir_all(root.join(".agents")).unwrap();
    fs::write(root.join(".agents/memory"), b"not a corpus").unwrap();
    fs::create_dir_all(&home).unwrap();
    let root = root.canonicalize().unwrap();
    let slug: String = root
        .to_str()
        .unwrap()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let private = home.join(".claude/projects").join(slug).join("memory");
    fs::create_dir_all(&private).unwrap();

    let corpora = corpora_for(
        &root,
        &Discovery {
            home: Some(home),
            override_dirs: Vec::new(),
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert_eq!(corpora.len(), 1);
    assert_eq!(corpora[0].kind(), CorpusKind::Private);
    assert_eq!(corpora[0].path(), private.canonicalize().unwrap());
}

#[cfg(unix)]
#[test]
fn a_dangling_default_corpus_symlink_is_omitted() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(root.join(".agents")).unwrap();
    symlink(tmp.path().join("missing"), root.join(".agents/memory")).unwrap();

    let corpora = corpora_for(&root, &Discovery::default()).unwrap();

    assert!(corpora.is_empty());
}

#[test]
fn an_overlong_claude_component_is_an_omitted_missing_surface() {
    let tmp = tempdir().unwrap();
    let mut root = tmp.path().join("project");
    for ordinal in 0..24 {
        root = root.join(format!("segment-{ordinal:02}"));
    }
    let home = tmp.path().join("home");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(home.join(".claude/projects")).unwrap();
    let root = root.canonicalize().unwrap();
    let slug: String = root
        .to_str()
        .unwrap()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    assert!(slug.len() > 255);

    let corpora = corpora_for(
        &root,
        &Discovery {
            home: Some(home),
            override_dirs: Vec::new(),
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert!(corpora.is_empty());
}

#[test]
fn an_override_must_resolve_to_a_real_directory() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let file = tmp.path().join("page.md");
    fs::create_dir_all(&root).unwrap();
    fs::write(&file, "not a corpus").unwrap();

    let report = discover_corpora(
        &root,
        &Discovery {
            home: None,
            override_dirs: vec![file.clone()],
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert!(report.corpora.is_empty());
    assert_eq!(report.notes.len(), 1);
    assert_eq!(report.notes[0].path, file);
    assert_eq!(report.notes[0].kind, DiscoveryNoteKind::NotDirectory);
}

#[cfg(unix)]
#[test]
fn an_escaping_shared_corpus_root_is_rejected() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(root.join(".agents")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, root.join(".agents/memory")).unwrap();
    let root = root.canonicalize().unwrap();

    let report = discover_corpora(&root, &Discovery::default()).unwrap();

    assert!(report.corpora.is_empty());
    assert_eq!(report.notes.len(), 1);
    assert_eq!(report.notes[0].kind, DiscoveryNoteKind::EscapesBase);
}

#[cfg(unix)]
#[test]
fn an_escaping_private_corpus_root_is_rejected() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&outside).unwrap();
    let root = root.canonicalize().unwrap();
    let slug: String = root
        .to_str()
        .unwrap()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let base = home.join(".claude/projects").join(slug);
    fs::create_dir_all(&base).unwrap();
    symlink(&outside, base.join("memory")).unwrap();

    let report = discover_corpora(
        &root,
        &Discovery {
            home: Some(home),
            override_dirs: Vec::new(),
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert!(report.corpora.is_empty());
    assert_eq!(report.notes.len(), 1);
    assert_eq!(report.notes[0].kind, DiscoveryNoteKind::EscapesBase);
}

#[cfg(unix)]
#[test]
fn escaping_shared_does_not_suppress_valid_private_discovery() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let home = tmp.path().join("home");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(root.join(".agents")).unwrap();
    fs::create_dir_all(&outside).unwrap();
    symlink(&outside, root.join(".agents/memory")).unwrap();
    let root = root.canonicalize().unwrap();
    let slug: String = root
        .to_str()
        .unwrap()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let private = home.join(".claude/projects").join(slug).join("memory");
    fs::create_dir_all(&private).unwrap();

    let report = discover_corpora(
        &root,
        &Discovery {
            home: Some(home),
            override_dirs: Vec::new(),
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert_eq!(report.corpora.len(), 1);
    assert_eq!(report.corpora[0].kind(), CorpusKind::Private);
    assert_eq!(report.corpora[0].path(), private.canonicalize().unwrap());
    assert_eq!(report.notes.len(), 1);
    assert_eq!(report.notes[0].kind, DiscoveryNoteKind::EscapesBase);
}

#[test]
fn invalid_override_does_not_suppress_a_later_valid_override() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let invalid = tmp.path().join("not-a-directory");
    let valid = tmp.path().join("valid-override");
    fs::create_dir_all(&root).unwrap();
    fs::write(&invalid, b"not a directory").unwrap();
    fs::create_dir_all(&valid).unwrap();

    let report = discover_corpora(
        &root,
        &Discovery {
            home: None,
            override_dirs: vec![invalid.clone(), valid.clone()],
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert_eq!(report.corpora.len(), 1);
    assert_eq!(report.corpora[0].kind(), CorpusKind::Override);
    assert_eq!(report.corpora[0].path(), valid.canonicalize().unwrap());
    assert_eq!(report.notes.len(), 1);
    assert_eq!(report.notes[0].path, invalid);
    assert_eq!(report.notes[0].kind, DiscoveryNoteKind::NotDirectory);
}

#[test]
fn relative_override_is_reported_and_never_resolved_from_project_root() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(root.join("relative-override")).unwrap();

    let report = discover_corpora(
        &root,
        &Discovery {
            home: None,
            override_dirs: vec![Path::new("relative-override").to_path_buf()],
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert!(report.corpora.is_empty());
    assert_eq!(report.notes.len(), 1);
    assert_eq!(report.notes[0].kind, DiscoveryNoteKind::RelativeOverride);
}

#[cfg(unix)]
#[test]
fn absolute_override_symlink_that_leaves_its_configured_parent_is_refused() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let configured_parent = tmp.path().join("configured");
    let configured = configured_parent.join("memory-link");
    let target = tmp.path().join("elsewhere/actual-memory");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&configured_parent).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("page.md"), b"ordinary page").unwrap();
    symlink(&target, &configured).unwrap();

    let discovered = discover_corpora(
        &root,
        &Discovery {
            home: None,
            override_dirs: vec![configured.clone()],
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert!(discovered.corpora.is_empty(), "{discovered:#?}");
    assert_eq!(discovered.notes.len(), 1, "{discovered:#?}");
    assert_eq!(discovered.notes[0].kind, DiscoveryNoteKind::EscapesBase);
    assert_eq!(discovered.notes[0].path, configured);
}

#[cfg(unix)]
#[test]
fn same_parent_override_symlink_stays_scannable_and_cannot_rebind_outside() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    let parent = tmp.path().join("grant");
    let configured = parent.join("memory-link");
    let target = parent.join("actual-memory");
    fs::create_dir_all(&root).unwrap();
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("page.md"), b"ordinary page").unwrap();
    symlink(&target, &configured).unwrap();

    let discovered = discover_corpora(
        &root,
        &Discovery {
            home: None,
            override_dirs: vec![configured],
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert!(discovered.notes.is_empty(), "{discovered:#?}");
    assert_eq!(discovered.corpora.len(), 1, "{discovered:#?}");
    assert_eq!(discovered.corpora[0].path(), target.canonicalize().unwrap());

    let scanned = scan_corpora(&discovered.corpora);
    assert_eq!(scanned.present.len(), 1, "{scanned:#?}");
    assert_eq!(
        scanned.present[0].path,
        target.canonicalize().unwrap().join("page.md")
    );

    let moved = parent.join("original-memory");
    let outside = tmp.path().join("replacement-outside-base");
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("external.md"), b"must not be scanned").unwrap();
    fs::rename(&target, &moved).unwrap();
    symlink(&outside, &target).unwrap();

    let after_swap = scan_corpora(&discovered.corpora);
    assert!(after_swap.present.is_empty(), "{after_swap:#?}");
    assert!(
        after_swap.unknown.iter().any(|note| matches!(
            note.kind,
            yams_core::ScanNoteKind::Raced | yams_core::ScanNoteKind::Unreadable
        )),
        "{after_swap:#?}"
    );
}

#[test]
fn filesystem_root_is_not_an_override_corpus() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().join("project");
    fs::create_dir_all(&root).unwrap();

    let discovered = discover_corpora(
        &root,
        &Discovery {
            home: None,
            override_dirs: vec![Path::new("/").to_path_buf()],
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert!(discovered.corpora.is_empty(), "{discovered:#?}");
    assert_eq!(discovered.notes[0].kind, DiscoveryNoteKind::EscapesBase);
}

fn claude_slug(root: &Path) -> String {
    root.to_str()
        .unwrap()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[test]
fn sibling_roots_that_share_a_claude_slug_do_not_merge_private_memory() {
    let tmp = tempdir().unwrap();
    let underscore = tmp.path().join("foo_bar");
    let hyphen = tmp.path().join("foo-bar");
    let home = tmp.path().join("home");
    fs::create_dir_all(&underscore).unwrap();
    fs::create_dir_all(&hyphen).unwrap();
    let underscore = underscore.canonicalize().unwrap();
    let hyphen = hyphen.canonicalize().unwrap();
    assert_eq!(claude_slug(&underscore), claude_slug(&hyphen));
    let private = home
        .join(".claude/projects")
        .join(claude_slug(&underscore))
        .join("memory");
    fs::create_dir_all(&private).unwrap();

    let report = discover_corpora(
        &underscore,
        &Discovery {
            home: Some(home),
            override_dirs: Vec::new(),
            known_roots: Vec::new(),
        },
    )
    .unwrap();

    assert!(
        report
            .corpora
            .iter()
            .all(|corpus| corpus.kind() != CorpusKind::Private),
        "{report:#?}"
    );
    assert_eq!(report.notes.len(), 1, "{report:#?}");
    assert_eq!(
        report.notes[0].kind,
        DiscoveryNoteKind::PrivateSlugCollision
    );
    assert_eq!(report.notes[0].path, private);
    assert!(
        report.notes[0]
            .detail
            .contains(&hyphen.display().to_string()),
        "{report:#?}"
    );
}

#[test]
fn known_roots_that_share_a_claude_slug_do_not_merge_private_memory() {
    let tmp = tempdir().unwrap();
    let nested = tmp.path().join("foo/bar");
    let collapsed = tmp.path().join("foo_bar");
    let home = tmp.path().join("home");
    fs::create_dir_all(&nested).unwrap();
    fs::create_dir_all(&collapsed).unwrap();
    let nested = nested.canonicalize().unwrap();
    let collapsed = collapsed.canonicalize().unwrap();
    assert_eq!(claude_slug(&nested), claude_slug(&collapsed));
    let private = home
        .join(".claude/projects")
        .join(claude_slug(&nested))
        .join("memory");
    fs::create_dir_all(&private).unwrap();

    let report = discover_corpora(
        &nested,
        &Discovery {
            home: Some(home),
            override_dirs: Vec::new(),
            known_roots: vec![collapsed.clone()],
        },
    )
    .unwrap();

    assert!(
        report
            .corpora
            .iter()
            .all(|corpus| corpus.kind() != CorpusKind::Private),
        "{report:#?}"
    );
    assert_eq!(report.notes.len(), 1, "{report:#?}");
    assert_eq!(
        report.notes[0].kind,
        DiscoveryNoteKind::PrivateSlugCollision
    );
    assert_eq!(report.notes[0].path, private);
    assert!(
        report.notes[0]
            .detail
            .contains(&collapsed.display().to_string()),
        "{report:#?}"
    );
}
