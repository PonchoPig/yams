use std::fs::{self, FileTimes, OpenOptions};
use std::path::{Path, PathBuf};
use tempfile::{TempDir, tempdir};
use yams_core::{
    Corpus, CorpusKind, Discovery, MAX_FILE_BYTES, ScanNoteKind, ScannedPage, corpora_for,
    scan_corpora,
};

fn corpus(path: &Path, kind: CorpusKind) -> Corpus {
    Corpus::validated(path, kind).unwrap()
}

fn present_names(report: &yams_core::ScanReport) -> Vec<String> {
    report
        .present
        .iter()
        .map(|page| page.path.file_name().unwrap().to_str().unwrap().to_owned())
        .collect()
}

fn scanned_page(content: &[u8]) -> (TempDir, PathBuf, ScannedPage) {
    let tmp = tempdir().unwrap();
    let page = tmp.path().join("page.md");
    fs::write(&page, content).unwrap();
    let mut report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert_eq!(report.present.len(), 1, "{report:#?}");
    let scanned = report.present.pop().unwrap();
    (tmp, page, scanned)
}

#[test]
fn navigation_names_are_skipped_before_hashing() {
    let tmp = tempdir().unwrap();
    for name in ["README.md", "MEMORY.md", "INDEX.md", "SCHEMA.md"] {
        fs::write(tmp.path().join(name), b"navigation").unwrap();
    }
    let ordinary = tmp.path().join("ordinary");
    fs::create_dir_all(&ordinary).unwrap();
    fs::write(ordinary.join("readme.md"), b"ordinary").unwrap();

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);

    assert_eq!(present_names(&report), ["readme.md"], "{report:#?}");
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
    let mut skipped: Vec<_> = report
        .rejected
        .iter()
        .map(|note| {
            (
                note.path.file_name().unwrap().to_str().unwrap().to_owned(),
                note.kind,
            )
        })
        .collect();
    skipped.sort();
    assert_eq!(
        skipped,
        [
            ("INDEX.md".to_owned(), ScanNoteKind::Navigation),
            ("MEMORY.md".to_owned(), ScanNoteKind::Navigation),
            ("README.md".to_owned(), ScanNoteKind::Navigation),
            ("SCHEMA.md".to_owned(), ScanNoteKind::Navigation),
        ],
        "{report:#?}"
    );
}

#[test]
fn only_the_case_sensitive_lowercase_markdown_extension_is_accepted() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("accepted.md"), b"yes").unwrap();
    fs::write(tmp.path().join("rejected.MD"), b"no").unwrap();
    fs::write(tmp.path().join("also-rejected.md.txt"), b"no").unwrap();

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);

    assert_eq!(present_names(&report), ["accepted.md"]);
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
}

#[test]
fn a_nested_page_is_present_with_exact_metadata_and_hash() {
    let tmp = tempdir().unwrap();
    let nested = tmp.path().join("topic/deeper");
    fs::create_dir_all(&nested).unwrap();
    let page = nested.join("known.md");
    fs::write(&page, b"abc").unwrap();

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Private)]);

    assert_eq!(report.present.len(), 1);
    let scanned = &report.present[0];
    assert_eq!(scanned.path, page.canonicalize().unwrap());
    assert_eq!(scanned.corpus, CorpusKind::Private);
    assert_eq!(scanned.byte_len, 3);
    assert!(scanned.modified_ns > 0);
    assert!(scanned.device > 0);
    assert!(scanned.inode > 0);
    assert_eq!(
        scanned.sha256,
        concat!(
            "ba7816bf", "8f01cfea", "414140de", "5dae2223", "b00361a3", "96177a9c", "b410ff61",
            "f20015ad",
        )
    );
    assert_eq!(scanned.content_bytes(), b"abc");
    scanned.revalidate().unwrap();
    assert_eq!(report.scanned_corpora, [tmp.path().canonicalize().unwrap()]);
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
}

#[test]
fn retained_content_is_cloneable_comparable_and_redacted_from_debug() {
    let (_tmp, _path, scanned) = scanned_page(b"PRIVATE_CONTENT_MARKER");

    assert_eq!(scanned.content_bytes(), b"PRIVATE_CONTENT_MARKER");
    assert_eq!(scanned, scanned.clone());
    let debug = format!("{scanned:#?}");
    assert!(!debug.contains("PRIVATE_CONTENT_MARKER"), "{debug}");
    assert!(!format!("{:?}", scanned.revision()).contains("PRIVATE_CONTENT_MARKER"));
}

#[test]
fn page_revision_is_nameable_from_the_crate_root() {
    fn assert_nameable(_revision: &yams_core::PageRevision) {}

    let (_tmp, _path, scanned) = scanned_page(b"page");

    assert_nameable(scanned.revision());
}

#[cfg(unix)]
#[test]
fn same_size_rewrite_with_restored_mtime_is_rejected_by_revision_ctime() {
    use std::os::unix::fs::MetadataExt;

    let (_tmp, path, scanned) = scanned_page(b"first");
    let original_mtime = fs::metadata(&path).unwrap().modified().unwrap();
    let original_ctime = {
        let metadata = fs::metadata(&path).unwrap();
        (metadata.ctime(), metadata.ctime_nsec())
    };
    fs::write(&path, b"other").unwrap();
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&path)
        .unwrap()
        .set_times(FileTimes::new().set_modified(original_mtime))
        .unwrap();
    let alias = path.with_extension("alias");
    fs::hard_link(&path, &alias).unwrap();
    fs::remove_file(alias).unwrap();
    let changed = fs::metadata(&path).unwrap();
    assert_ne!(original_ctime, (changed.ctime(), changed.ctime_nsec()));

    let error = scanned.revalidate().unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::Raced, "{error:#?}");
    assert_eq!(scanned.content_bytes(), b"first");
}

#[cfg(unix)]
#[test]
fn revision_rejects_a_file_symlink_swap_without_following_it() {
    use std::os::unix::fs::symlink;

    let (_tmp, path, scanned) = scanned_page(b"first");
    let moved = path.with_extension("original");
    fs::rename(&path, &moved).unwrap();
    symlink(&moved, &path).unwrap();

    let error = scanned.revalidate().unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::UnsafeSymlink, "{error:#?}");
}

#[cfg(unix)]
#[test]
fn revision_rejects_a_fifo_swap_without_blocking() {
    use std::os::unix::fs::FileTypeExt;
    use std::process::Command;

    let (_tmp, path, scanned) = scanned_page(b"first");
    fs::rename(&path, path.with_extension("original")).unwrap();
    let status = Command::new("mkfifo").arg(&path).status().unwrap();
    assert!(status.success());
    assert!(fs::symlink_metadata(&path).unwrap().file_type().is_fifo());

    let error = scanned.revalidate().unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::UnsafeFileType, "{error:#?}");
}

#[test]
fn revision_rejects_a_hard_link_created_after_scanning() {
    let (_tmp, path, scanned) = scanned_page(b"first");
    let alias = path.with_extension("alias");
    if fs::hard_link(&path, &alias).is_err() {
        return;
    }

    let error = scanned.revalidate().unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::UnsafeHardLink, "{error:#?}");
}

#[test]
fn revision_rejects_growth_past_the_bounded_read_cap() {
    let (_tmp, path, scanned) = scanned_page(b"first");
    fs::write(&path, vec![0_u8; MAX_FILE_BYTES as usize + 1]).unwrap();

    let error = scanned.revalidate().unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::Oversized, "{error:#?}");
}

#[test]
fn revision_rejects_a_replaced_nested_directory() {
    let tmp = tempdir().unwrap();
    let nested = tmp.path().join("topic/deeper");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("page.md"), b"first").unwrap();
    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);
    let scanned = &report.present[0];
    let moved = tmp.path().join("topic/original");
    fs::rename(&nested, &moved).unwrap();
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("page.md"), b"second").unwrap();

    let error = scanned.revalidate().unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::Raced, "{error:#?}");
}

#[test]
fn revision_rejects_a_replaced_corpus_root() {
    let outer = tempdir().unwrap();
    let root = outer.path().join("corpus");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("page.md"), b"first").unwrap();
    let report = scan_corpora(&[corpus(&root, CorpusKind::Shared)]);
    let scanned = &report.present[0];
    fs::rename(&root, outer.path().join("original-corpus")).unwrap();
    fs::create_dir(&root).unwrap();
    fs::write(root.join("page.md"), b"second").unwrap();

    let error = scanned.revalidate().unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::Raced, "{error:#?}");
}

#[cfg(unix)]
fn descriptor_count() -> Option<usize> {
    let directory = if Path::new("/dev/fd").is_dir() {
        Path::new("/dev/fd")
    } else {
        Path::new("/proc/self/fd")
    };
    fs::read_dir(directory).ok().map(Iterator::count)
}

#[cfg(unix)]
#[test]
fn many_pages_share_a_small_pinned_descriptor_chain() {
    let tmp = tempdir().unwrap();
    for index in 0..128 {
        fs::write(tmp.path().join(format!("page-{index:03}.md")), b"page").unwrap();
    }
    let validated = corpus(tmp.path(), CorpusKind::Shared);
    let before = descriptor_count().unwrap();

    let report = scan_corpora(&[validated]);
    let after = descriptor_count().unwrap();

    assert_eq!(report.present.len(), 128, "{report:#?}");
    assert!(
        after.saturating_sub(before) < 16,
        "scanner retained {} descriptors for {} pages",
        after.saturating_sub(before),
        report.present.len()
    );
    for page in &report.present {
        page.revalidate().unwrap();
    }
}

#[cfg(unix)]
#[test]
fn a_directory_symlink_is_rejected_and_never_followed() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let corpus_dir = tmp.path().join("corpus");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&corpus_dir).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(outside.join("secret.md"), b"secret").unwrap();
    let link = corpus_dir.join("linked");
    symlink(&outside, &link).unwrap();

    let report = scan_corpora(&[corpus(&corpus_dir, CorpusKind::Shared)]);
    let expected_link = corpus_dir.canonicalize().unwrap().join("linked");

    assert!(report.present.is_empty());
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(
        report
            .rejected
            .iter()
            .any(|note| note.path == expected_link && note.kind == ScanNoteKind::UnsafeSymlink),
        "{report:#?}"
    );
}

#[cfg(unix)]
#[test]
fn final_file_symlinks_inside_and_outside_are_refused() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let corpus_dir = tmp.path().join("corpus");
    let outside = tmp.path().join("outside.md");
    fs::create_dir_all(&corpus_dir).unwrap();
    let target = corpus_dir.join("target.txt");
    fs::write(&target, b"inside").unwrap();
    fs::write(&outside, b"outside").unwrap();
    let inside_link = corpus_dir.join("inside.md");
    let outside_link = corpus_dir.join("outside.md");
    let dangling_link = corpus_dir.join("dangling.md");
    symlink(&target, &inside_link).unwrap();
    symlink(&outside, &outside_link).unwrap();
    symlink(corpus_dir.join("missing"), &dangling_link).unwrap();

    let report = scan_corpora(&[corpus(&corpus_dir, CorpusKind::Shared)]);

    assert!(report.present.is_empty());
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert_eq!(
        report
            .rejected
            .iter()
            .filter(|note| note.kind == ScanNoteKind::UnsafeSymlink)
            .count(),
        3
    );
    assert!(report.oversized.is_empty(), "{report:#?}");
}

#[cfg(unix)]
#[test]
fn a_special_file_named_markdown_is_a_stable_rejection_and_never_opened() {
    use std::os::unix::net::UnixListener;

    let tmp = tempdir().unwrap();
    let _listener = UnixListener::bind(tmp.path().join("socket.md")).unwrap();

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);

    assert!(report.present.is_empty(), "{report:#?}");
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
    assert_eq!(report.rejected.len(), 1, "{report:#?}");
    assert_eq!(report.rejected[0].kind, ScanNoteKind::UnsafeFileType);
}

#[test]
fn exactly_one_mibibyte_is_present_and_hashed() {
    let tmp = tempdir().unwrap();
    let page = tmp.path().join("limit.md");
    fs::write(&page, vec![0_u8; MAX_FILE_BYTES as usize]).unwrap();

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);

    assert!(report.oversized.is_empty());
    assert_eq!(report.present.len(), 1);
    assert_eq!(report.present[0].byte_len, MAX_FILE_BYTES);
    assert_eq!(
        report.present[0].sha256,
        concat!(
            "30e14955", "ebf13522", "66dc2ff8", "067e6810", "4607e750", "abb9d3b3", "6582b8af",
            "909fcb58",
        )
    );
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
}

#[test]
fn one_byte_over_the_cap_is_positive_oversized_knowledge() {
    let tmp = tempdir().unwrap();
    let page = tmp.path().join("large.md");
    fs::write(&page, vec![0_u8; MAX_FILE_BYTES as usize + 1]).unwrap();

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);

    assert!(report.present.is_empty());
    assert!(report.unknown.is_empty());
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert_eq!(report.oversized.len(), 1);
    assert_eq!(report.oversized[0].path, page.canonicalize().unwrap());
    assert_eq!(report.oversized[0].kind, ScanNoteKind::Oversized);
}

#[cfg(unix)]
#[test]
fn an_unreadable_file_is_unknown_when_permissions_are_enforced() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempdir().unwrap();
    let page = tmp.path().join("unreadable.md");
    fs::write(&page, b"private").unwrap();
    let original = fs::metadata(&page).unwrap().permissions();
    fs::set_permissions(&page, fs::Permissions::from_mode(0o0)).unwrap();
    let host_refuses_read = fs::File::open(&page).is_err();

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);
    fs::set_permissions(&page, original).unwrap();

    if host_refuses_read {
        assert!(report.present.is_empty());
        assert!(report.rejected.is_empty(), "{report:#?}");
        assert!(
            report
                .unknown
                .iter()
                .any(|note| note.kind == ScanNoteKind::Unreadable)
        );
    } else {
        assert_eq!(report.present.len(), 1);
    }
}

#[test]
fn a_directory_named_markdown_is_rejected_not_present() {
    let tmp = tempdir().unwrap();
    let directory = tmp.path().join("not-a-page.md");
    fs::create_dir_all(&directory).unwrap();

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);
    let expected_directory = tmp.path().canonicalize().unwrap().join("not-a-page.md");

    assert!(report.present.is_empty());
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(
        report.rejected.iter().any(|note| {
            note.path == expected_directory && note.kind == ScanNoteKind::UnsafeDirectory
        }),
        "{report:#?}"
    );
}

#[test]
fn hard_linked_pages_are_refused_where_supported() {
    let tmp = tempdir().unwrap();
    let first = tmp.path().join("first.md");
    let second = tmp.path().join("second.md");
    fs::write(&first, b"same inode").unwrap();
    if fs::hard_link(&first, &second).is_err() {
        return;
    }

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Shared)]);

    assert!(report.present.is_empty());
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert_eq!(
        report
            .rejected
            .iter()
            .filter(|note| note.kind == ScanNoteKind::UnsafeHardLink)
            .count(),
        2
    );
}

#[test]
fn output_order_is_deterministic_and_keeps_corpus_kind() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("z-last.md"), b"z").unwrap();
    fs::write(tmp.path().join("a-first.md"), b"a").unwrap();

    let report = scan_corpora(&[corpus(tmp.path(), CorpusKind::Override)]);

    assert_eq!(present_names(&report), ["a-first.md", "z-last.md"]);
    assert!(
        report
            .present
            .iter()
            .all(|page| page.corpus == CorpusKind::Override)
    );
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
}

#[test]
fn a_nonexistent_corpus_is_unknown_without_aborting_other_corpora() {
    let tmp = tempdir().unwrap();
    let good = tmp.path().join("good");
    let missing = tmp.path().join("missing");
    fs::create_dir_all(&good).unwrap();
    fs::create_dir_all(&missing).unwrap();
    fs::write(good.join("kept.md"), b"kept").unwrap();
    let missing_canonical = missing.canonicalize().unwrap();
    let missing_corpus = corpus(&missing, CorpusKind::Shared);
    fs::remove_dir(&missing).unwrap();

    let report = scan_corpora(&[missing_corpus, corpus(&good, CorpusKind::Private)]);

    assert_eq!(present_names(&report), ["kept.md"]);
    assert!(
        report
            .unknown
            .iter()
            .any(|note| note.path == missing_canonical && note.kind == ScanNoteKind::Raced)
    );
}

#[test]
fn nested_overrides_emit_each_canonical_page_once() {
    let tmp = tempdir().unwrap();
    let broad = tmp.path().join("broad");
    let nested = broad.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(broad.join("outer.md"), b"outer").unwrap();
    fs::write(nested.join("duplicate.md"), b"duplicate").unwrap();

    let report = scan_corpora(&[
        corpus(&nested, CorpusKind::Override),
        corpus(&broad, CorpusKind::Override),
    ]);

    assert_eq!(present_names(&report), ["duplicate.md", "outer.md"]);
    assert_eq!(report.present.len(), 2);
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
}

#[test]
fn overlap_keeps_first_corpus_provenance() {
    let tmp = tempdir().unwrap();
    let broad = tmp.path().join("broad");
    let nested = broad.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("page.md"), b"same page").unwrap();

    let report = scan_corpora(&[
        corpus(&nested, CorpusKind::Private),
        corpus(&broad, CorpusKind::Override),
    ]);

    assert_eq!(report.present.len(), 1, "{report:#?}");
    assert_eq!(report.present[0].corpus, CorpusKind::Private);
}

#[cfg(unix)]
#[test]
fn a_discovered_root_replaced_by_an_external_symlink_is_incomplete() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let project = tmp.path().join("project");
    let shared = project.join(".agents/memory");
    let moved = project.join(".agents/original-memory");
    let outside = tmp.path().join("outside");
    fs::create_dir_all(&shared).unwrap();
    fs::create_dir_all(&outside).unwrap();
    fs::write(shared.join("safe.md"), b"safe").unwrap();
    fs::write(outside.join("external.md"), b"must not be scanned").unwrap();
    let root = project.canonicalize().unwrap();
    let corpora = corpora_for(&root, &Discovery::default()).unwrap();
    fs::rename(&shared, &moved).unwrap();
    symlink(&outside, &shared).unwrap();

    let report = scan_corpora(&corpora);

    assert!(report.present.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert!(
        report
            .unknown
            .iter()
            .any(|note| note.kind == ScanNoteKind::Raced),
        "{report:#?}"
    );
    assert!(report.scanned_corpora.is_empty(), "{report:#?}");
}

#[cfg(unix)]
#[test]
fn a_discovered_ancestor_replaced_by_an_external_symlink_is_incomplete() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();
    let project = tmp.path().join("project");
    let agents = project.join(".agents");
    let shared = agents.join("memory");
    let moved = project.join("original-agents");
    let outside_agents = tmp.path().join("outside-agents");
    fs::create_dir_all(&shared).unwrap();
    fs::create_dir_all(outside_agents.join("memory")).unwrap();
    fs::write(shared.join("safe.md"), b"safe").unwrap();
    fs::write(
        outside_agents.join("memory/external.md"),
        b"must not be scanned",
    )
    .unwrap();
    let root = project.canonicalize().unwrap();
    let corpora = corpora_for(&root, &Discovery::default()).unwrap();
    fs::rename(&agents, &moved).unwrap();
    symlink(&outside_agents, &agents).unwrap();

    let report = scan_corpora(&corpora);

    assert!(report.present.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert!(
        report
            .unknown
            .iter()
            .any(|note| note.kind == ScanNoteKind::Raced),
        "{report:#?}"
    );
    assert!(report.scanned_corpora.is_empty(), "{report:#?}");
}

#[test]
fn duplicate_corpora_are_reported_complete_once() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("page.md"), b"page").unwrap();
    let validated = corpus(tmp.path(), CorpusKind::Override);

    let report = scan_corpora(&[validated.clone(), validated]);

    assert_eq!(report.present.len(), 1, "{report:#?}");
    assert_eq!(report.scanned_corpora.len(), 1, "{report:#?}");
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
}
