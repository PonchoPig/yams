use std::fs::{self, FileTimes, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Barrier};
use std::time::SystemTime;

use tempfile::{TempDir, tempdir};

use super::*;

fn test_corpus(path: &Path) -> Corpus {
    Corpus::validated(path, CorpusKind::Shared).unwrap()
}

struct ReplaceBeforeOpen {
    page: PathBuf,
    moved: PathBuf,
    fired: bool,
}

impl ScanHooks for ReplaceBeforeOpen {
    fn before_file_open(&mut self, path: &Path) {
        if path == self.page && !self.fired {
            fs::rename(&self.page, &self.moved).unwrap();
            fs::write(&self.page, b"other").unwrap();
            self.fired = true;
        }
    }
}

#[test]
fn replacing_the_candidate_before_open_is_raced_unknown() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let page = root.join("page.md");
    fs::write(&page, b"first").unwrap();
    let mut hooks = ReplaceBeforeOpen {
        page,
        moved: root.join("original.txt"),
        fired: false,
    };

    let report = scan_corpora_with_hooks(&[test_corpus(tmp.path())], &mut hooks);

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
}

struct SwapDirectory {
    target: PathBuf,
    moved: PathBuf,
    fired: bool,
}

impl ScanHooks for SwapDirectory {
    fn after_directory_stream_opened(&mut self, path: &Path) {
        if path == self.target && !self.fired {
            fs::rename(&self.target, &self.moved).unwrap();
            fs::create_dir(&self.target).unwrap();
            self.fired = true;
        }
    }
}

#[test]
fn replacing_an_empty_directory_after_stream_open_is_incomplete() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let target = root.join("pages");
    fs::create_dir_all(&target).unwrap();
    let mut hooks = SwapDirectory {
        target,
        moved: root.join("relocated"),
        fired: false,
    };

    let report = scan_corpora_with_hooks(&[test_corpus(&root)], &mut hooks);

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
fn replacing_a_nonempty_directory_rolls_back_staged_pages() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let target = root.join("pages");
    fs::create_dir_all(&target).unwrap();
    fs::write(target.join("old.md"), b"old namespace").unwrap();
    let mut hooks = SwapDirectory {
        target,
        moved: root.join("relocated"),
        fired: false,
    };

    let report = scan_corpora_with_hooks(&[test_corpus(&root)], &mut hooks);

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

struct GrowBeforeRead {
    page: PathBuf,
    fired: bool,
}

impl ScanHooks for GrowBeforeRead {
    fn before_file_read(&mut self, path: &Path) {
        if path == self.page && !self.fired {
            let mut file = OpenOptions::new().append(true).open(&self.page).unwrap();
            file.write_all(&vec![b'x'; MAX_FILE_BYTES as usize + 1])
                .unwrap();
            self.fired = true;
        }
    }
}

#[test]
fn growth_past_the_cap_after_descriptor_validation_is_oversized() {
    let tmp = tempdir().unwrap();
    let page = tmp.path().canonicalize().unwrap().join("grower.md");
    fs::write(&page, b"small").unwrap();
    let mut hooks = GrowBeforeRead { page, fired: false };

    let report = scan_corpora_with_hooks(&[test_corpus(tmp.path())], &mut hooks);

    assert!(report.present.is_empty(), "{report:#?}");
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert_eq!(report.oversized.len(), 1, "{report:#?}");
    assert_eq!(report.oversized[0].kind, ScanNoteKind::Oversized);
}

struct RebindAfterRead {
    page: PathBuf,
    moved: PathBuf,
    fired: bool,
}

impl ScanHooks for RebindAfterRead {
    fn after_file_read(&mut self, path: &Path) {
        if path == self.page && !self.fired {
            fs::rename(&self.page, &self.moved).unwrap();
            fs::write(&self.page, b"second inode").unwrap();
            self.fired = true;
        }
    }
}

#[test]
fn pathname_rebound_after_read_never_emits_old_inode_under_new_name() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let page = root.join("page.md");
    fs::write(&page, b"first inode").unwrap();
    let mut hooks = RebindAfterRead {
        page,
        moved: root.join("old.txt"),
        fired: false,
    };

    let report = scan_corpora_with_hooks(&[test_corpus(&root)], &mut hooks);

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
}

struct RewriteWithRestoredMtime {
    page: PathBuf,
    modified: SystemTime,
    fired: bool,
}

impl ScanHooks for RewriteWithRestoredMtime {
    fn before_file_read(&mut self, path: &Path) {
        if path == self.page && !self.fired {
            fs::write(&self.page, b"other").unwrap();
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.page)
                .unwrap();
            file.set_times(FileTimes::new().set_modified(self.modified))
                .unwrap();
            self.fired = true;
        }
    }
}

#[test]
fn same_length_rewrite_with_restored_mtime_is_raced_by_ctime() {
    let tmp = tempdir().unwrap();
    let page = tmp.path().canonicalize().unwrap().join("page.md");
    fs::write(&page, b"first").unwrap();
    let modified = fs::metadata(&page).unwrap().modified().unwrap();
    let mut hooks = RewriteWithRestoredMtime {
        page,
        modified,
        fired: false,
    };

    let report = scan_corpora_with_hooks(&[test_corpus(tmp.path())], &mut hooks);

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
}

struct RewriteBeforeSnapshotPublish {
    page: PathBuf,
    fired: bool,
}

impl ScanHooks for RewriteBeforeSnapshotPublish {
    fn before_snapshot_publish(&mut self, path: &Path) {
        if path == self.page && !self.fired {
            fs::write(&self.page, b"other").unwrap();
            self.fired = true;
        }
    }
}

#[test]
fn retained_bytes_and_hash_come_from_the_same_validated_read() {
    let tmp = tempdir().unwrap();
    let page = tmp.path().canonicalize().unwrap().join("page.md");
    fs::write(&page, b"first").unwrap();
    let mut hooks = RewriteBeforeSnapshotPublish { page, fired: false };

    let report = scan_corpora_with_hooks(&[test_corpus(tmp.path())], &mut hooks);

    assert_eq!(report.present.len(), 1, "{report:#?}");
    assert_eq!(report.present[0].content_bytes(), b"first");
    assert_eq!(
        report.present[0].sha256,
        "a7937b64b8caa58f03721bb6bacf5c78cb235febe0e70b1b84cd99541461a08e"
    );
    assert_eq!(
        report.present[0].revalidate().unwrap_err().kind,
        ScanNoteKind::Raced
    );
}

#[test]
fn snapshot_seal_rejects_every_mutable_field_and_private_content_divergence() {
    let tmp = tempdir().unwrap();
    let page_path = tmp.path().canonicalize().unwrap().join("page.md");
    fs::write(&page_path, b"sealed page").unwrap();
    let scanned = scan_corpora(&[test_corpus(tmp.path())])
        .present
        .into_iter()
        .next()
        .unwrap();
    scanned.validate_snapshot().unwrap();

    let mut variants = Vec::new();
    let mut changed = scanned.clone();
    changed.path.set_file_name("forged.md");
    variants.push(changed);
    let mut changed = scanned.clone();
    changed.corpus = CorpusKind::Private;
    variants.push(changed);
    let mut changed = scanned.clone();
    changed.byte_len += 1;
    variants.push(changed);
    let mut changed = scanned.clone();
    changed.modified_ns += 1;
    variants.push(changed);
    let mut changed = scanned.clone();
    changed.device += 1;
    variants.push(changed);
    let mut changed = scanned.clone();
    changed.inode += 1;
    variants.push(changed);
    let mut changed = scanned.clone();
    changed.sha256 = "0".repeat(64);
    variants.push(changed);
    let mut changed = scanned;
    changed.content = Arc::from(&b"forged page"[..]);
    variants.push(changed);

    for changed in variants {
        let note = changed.validate_snapshot().unwrap_err();
        assert_eq!(note.kind, ScanNoteKind::Raced);
        assert_eq!(note.path, page_path);
        assert!(matches!(
            changed.revalidate(),
            Err(ScanNote {
                kind: ScanNoteKind::Raced,
                ..
            })
        ));
    }
}

#[test]
fn complete_namespace_revision_rejects_membership_changes_without_reading_pages() {
    let tmp = tempdir().unwrap();
    let nested = tmp.path().join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("existing.md"), b"sealed page").unwrap();
    let report = scan_corpora(&[test_corpus(tmp.path())]);
    report.validate_snapshot().unwrap();
    report.revalidate_namespaces().unwrap();

    fs::write(nested.join("added.md"), b"new membership").unwrap();

    let note = report.revalidate_namespaces().unwrap_err();
    assert_eq!(note.kind, ScanNoteKind::Raced);
    assert_eq!(note.path, nested.canonicalize().unwrap());
}

#[test]
fn report_seal_rejects_forged_positive_absence_authority() {
    let tmp = tempdir().unwrap();
    fs::write(tmp.path().join("existing.md"), b"sealed page").unwrap();
    let report = scan_corpora(&[test_corpus(tmp.path())]);

    let mut missing_page = report.clone();
    missing_page.present.clear();
    assert!(missing_page.validate_snapshot().is_err());

    let mut forged_scope = report;
    forged_scope.scanned_corpora.clear();
    assert!(forged_scope.validate_snapshot().is_err());
}

struct PauseRevisionBeforeFileOpen {
    barrier: Arc<Barrier>,
}

impl RevisionHooks for PauseRevisionBeforeFileOpen {
    fn before_file_open(&mut self, _path: &Path) {
        self.barrier.wait();
        self.barrier.wait();
    }
}

#[test]
fn revision_rejects_a_file_rebound_between_named_and_descriptor_checks() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let page = root.join("page.md");
    fs::write(&page, b"first").unwrap();
    let mut report = scan_corpora(&[test_corpus(tmp.path())]);
    let revision = report.present.pop().unwrap().revision().clone();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        revision.revalidate_with_hooks(&mut PauseRevisionBeforeFileOpen {
            barrier: worker_barrier,
        })
    });
    barrier.wait();
    fs::rename(&page, root.join("original.txt")).unwrap();
    fs::write(&page, b"other").unwrap();
    barrier.wait();

    let error = worker.join().unwrap().unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::Raced, "{error:#?}");
}

#[test]
fn revision_replays_nested_ancestor_bindings_after_the_final_file_check() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let nested = root.join("topic/deeper");
    fs::create_dir_all(&nested).unwrap();
    let page = nested.join("page.md");
    fs::write(&page, b"first").unwrap();
    let mut report = scan_corpora(&[test_corpus(tmp.path())]);
    let revision = report.present.pop().unwrap().revision().clone();
    let barrier = Arc::new(Barrier::new(2));
    let worker_barrier = Arc::clone(&barrier);
    let worker = std::thread::spawn(move || {
        revision.revalidate_with_hooks(&mut PauseRevisionBeforeFileOpen {
            barrier: worker_barrier,
        })
    });
    barrier.wait();
    fs::rename(&nested, root.join("topic/original-deeper")).unwrap();
    fs::create_dir(&nested).unwrap();
    fs::write(&page, b"other").unwrap();
    barrier.wait();

    let error = worker.join().unwrap().unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::Raced, "{error:#?}");
}

struct SwapDirectoryToSymlinkBeforeOpen {
    target: PathBuf,
    moved: PathBuf,
    fired: bool,
}

impl RevisionHooks for SwapDirectoryToSymlinkBeforeOpen {
    fn before_directory_open(&mut self, path: &Path) {
        if path == self.target && !self.fired {
            std::fs::rename(&self.target, &self.moved).unwrap();
            std::os::unix::fs::symlink(&self.moved, &self.target).unwrap();
            self.fired = true;
        }
    }
}

#[test]
fn revision_classifies_a_descendant_symlink_swap_without_following_it() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let topic = root.join("topic");
    fs::create_dir(&topic).unwrap();
    fs::write(topic.join("page.md"), b"first").unwrap();
    let mut report = scan_corpora(&[test_corpus(tmp.path())]);
    let revision = report.present.pop().unwrap().revision().clone();
    let mut hooks = SwapDirectoryToSymlinkBeforeOpen {
        target: topic,
        moved: root.join("original-topic"),
        fired: false,
    };

    let error = revision.revalidate_with_hooks(&mut hooks).unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::UnsafeSymlink, "{error:#?}");
}

struct SwapFileToSymlinkBeforeOpen {
    target: PathBuf,
    moved: PathBuf,
    fired: bool,
}

impl RevisionHooks for SwapFileToSymlinkBeforeOpen {
    fn before_file_open(&mut self, path: &Path) {
        if path == self.target && !self.fired {
            std::fs::rename(&self.target, &self.moved).unwrap();
            std::os::unix::fs::symlink(&self.moved, &self.target).unwrap();
            self.fired = true;
        }
    }
}

#[test]
fn revision_classifies_a_final_symlink_swap_without_following_it() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let page = root.join("page.md");
    fs::write(&page, b"first").unwrap();
    let mut report = scan_corpora(&[test_corpus(tmp.path())]);
    let revision = report.present.pop().unwrap().revision().clone();
    let mut hooks = SwapFileToSymlinkBeforeOpen {
        target: page,
        moved: root.join("original.txt"),
        fired: false,
    };

    let error = revision.revalidate_with_hooks(&mut hooks).unwrap_err();

    assert_eq!(error.kind, ScanNoteKind::UnsafeSymlink, "{error:#?}");
}

struct TransientHardLink {
    page: PathBuf,
    alias: PathBuf,
    fired: bool,
}

impl ScanHooks for TransientHardLink {
    fn before_file_open(&mut self, path: &Path) {
        if path == self.page && !self.fired {
            fs::hard_link(&self.page, &self.alias).unwrap();
            fs::remove_file(&self.alias).unwrap();
            self.fired = true;
        }
    }
}

#[test]
fn transient_hard_link_is_raced_by_ctime_even_after_link_count_recovers() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let page = root.join("page.md");
    fs::write(&page, b"page").unwrap();
    let mut hooks = TransientHardLink {
        page,
        alias: root.join("alias.txt"),
        fired: false,
    };

    let report = scan_corpora_with_hooks(&[test_corpus(&root)], &mut hooks);

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
}

#[derive(Default)]
struct CountBeforeOpen {
    count: usize,
}

impl ScanHooks for CountBeforeOpen {
    fn before_file_open(&mut self, _path: &Path) {
        self.count += 1;
    }
}

#[test]
fn stat_known_oversized_page_never_reaches_before_open_hook() {
    let tmp = tempdir().unwrap();
    fs::write(
        tmp.path().join("large.md"),
        vec![0_u8; MAX_FILE_BYTES as usize + 1],
    )
    .unwrap();
    let mut hooks = CountBeforeOpen::default();

    let report = scan_corpora_with_hooks(&[test_corpus(tmp.path())], &mut hooks);

    assert_eq!(hooks.count, 0);
    assert!(report.present.is_empty(), "{report:#?}");
    assert!(report.unknown.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert_eq!(report.oversized.len(), 1, "{report:#?}");
}

struct ForceUnreadable {
    page: PathBuf,
}

impl ScanHooks for ForceUnreadable {
    fn fail_file_open(&mut self, path: &Path) -> bool {
        path == self.page
    }
}

#[test]
fn injected_open_failure_is_deterministically_unreadable() {
    let tmp = tempdir().unwrap();
    let page = tmp.path().canonicalize().unwrap().join("page.md");
    fs::write(&page, b"page").unwrap();
    let mut hooks = ForceUnreadable { page };

    let report = scan_corpora_with_hooks(&[test_corpus(tmp.path())], &mut hooks);

    assert!(report.present.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert!(
        report
            .unknown
            .iter()
            .any(|note| note.kind == ScanNoteKind::Unreadable),
        "{report:#?}"
    );
    assert!(report.scanned_corpora.is_empty(), "{report:#?}");
}

struct ReplaceOnSecondScan {
    page: PathBuf,
    moved: PathBuf,
    opens: usize,
}

impl ScanHooks for ReplaceOnSecondScan {
    fn before_file_open(&mut self, path: &Path) {
        if path != self.page {
            return;
        }
        self.opens += 1;
        if self.opens == 2 {
            fs::rename(&self.page, &self.moved).unwrap();
            fs::write(&self.page, b"replacement").unwrap();
        }
    }
}

#[test]
fn overlapping_unknown_observation_dominates_an_earlier_present_page() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let page = root.join("page.md");
    fs::write(&page, b"original").unwrap();
    let validated = test_corpus(&root);
    let mut hooks = ReplaceOnSecondScan {
        page,
        moved: root.join("old.txt"),
        opens: 0,
    };

    let report = scan_corpora_with_hooks(&[validated.clone(), validated], &mut hooks);

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

struct FailPartialDirectory {
    target: PathBuf,
    fail_after: usize,
    require_processed_file: bool,
    processed_file: bool,
    fired: bool,
}

impl ScanHooks for FailPartialDirectory {
    fn before_file_open(&mut self, path: &Path) {
        if path.starts_with(&self.target) {
            self.processed_file = true;
        }
    }

    fn fail_directory_iteration_after_processed_entry(
        &mut self,
        path: &Path,
        processed: usize,
    ) -> bool {
        if path == self.target
            && processed == self.fail_after
            && (!self.require_processed_file || self.processed_file)
            && !self.fired
        {
            self.fired = true;
            true
        } else {
            false
        }
    }
}

#[test]
fn partial_directory_rolls_back_all_positives_but_keeps_a_stable_sibling() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    let partial = root.join("a-partial");
    let stable = root.join("b-stable");
    fs::create_dir_all(&partial).unwrap();
    fs::create_dir_all(&stable).unwrap();
    fs::write(partial.join("present.md"), b"discard me").unwrap();
    fs::write(
        partial.join("oversized.md"),
        vec![0_u8; MAX_FILE_BYTES as usize + 1],
    )
    .unwrap();
    fs::create_dir(partial.join("rejected.md")).unwrap();
    fs::write(stable.join("kept.md"), b"keep me").unwrap();
    let mut hooks = FailPartialDirectory {
        target: partial.clone(),
        fail_after: 3,
        require_processed_file: true,
        processed_file: false,
        fired: false,
    };

    let report = scan_corpora_with_hooks(&[test_corpus(&root)], &mut hooks);

    assert_eq!(report.present.len(), 1, "{report:#?}");
    assert_eq!(report.present[0].path, stable.join("kept.md"));
    assert!(report.oversized.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert_eq!(report.unknown.len(), 1, "{report:#?}");
    assert_eq!(report.unknown[0].path, partial);
    assert_eq!(report.unknown[0].kind, ScanNoteKind::Unreadable);
    assert!(report.scanned_corpora.is_empty(), "{report:#?}");
}

fn overlap_fixture() -> (TempDir, PathBuf, Corpus, Corpus) {
    let tmp = tempdir().unwrap();
    let broad = tmp.path().join("broad");
    let nested = broad.join("nested");
    fs::create_dir_all(&nested).unwrap();
    fs::write(nested.join("present.md"), b"present").unwrap();
    fs::write(
        nested.join("oversized.md"),
        vec![0_u8; MAX_FILE_BYTES as usize + 1],
    )
    .unwrap();
    fs::create_dir(nested.join("rejected.md")).unwrap();
    let broad = broad.canonicalize().unwrap();
    let nested = nested.canonicalize().unwrap();
    let nested_corpus = test_corpus(&nested);
    let broad_corpus = test_corpus(&broad);
    (tmp, broad, nested_corpus, broad_corpus)
}

fn assert_unknown_ancestor_dominates(report: &ScanReport, broad: &Path) {
    assert!(report.present.is_empty(), "{report:#?}");
    assert!(report.oversized.is_empty(), "{report:#?}");
    assert!(report.rejected.is_empty(), "{report:#?}");
    assert_eq!(report.unknown.len(), 1, "{report:#?}");
    assert_eq!(report.unknown[0].path, broad);
    assert_eq!(report.unknown[0].kind, ScanNoteKind::Unreadable);
    assert!(report.scanned_corpora.is_empty(), "{report:#?}");
}

#[test]
fn later_unknown_parent_dominates_an_earlier_successful_nested_scan() {
    let (_tmp, broad, nested, parent) = overlap_fixture();
    let mut hooks = FailPartialDirectory {
        target: broad.clone(),
        fail_after: 1,
        require_processed_file: false,
        processed_file: false,
        fired: false,
    };

    let report = scan_corpora_with_hooks(&[nested, parent], &mut hooks);

    assert_unknown_ancestor_dominates(&report, &broad);
}

#[test]
fn earlier_unknown_parent_dominates_a_later_successful_nested_scan() {
    let (_tmp, broad, nested, parent) = overlap_fixture();
    let mut hooks = FailPartialDirectory {
        target: broad.clone(),
        fail_after: 1,
        require_processed_file: false,
        processed_file: false,
        fired: false,
    };

    let report = scan_corpora_with_hooks(&[parent, nested], &mut hooks);

    assert_unknown_ancestor_dominates(&report, &broad);
}

#[test]
fn dot_directories_are_not_descended() {
    let tmp = tempdir().unwrap();
    let root = tmp.path().canonicalize().unwrap();
    fs::write(root.join("page.md"), b"real").unwrap();
    let obsidian = root.join(".obsidian");
    fs::create_dir_all(&obsidian).unwrap();
    fs::write(obsidian.join("decoy.md"), b"vault settings").unwrap();
    let trash = root.join(".trash");
    fs::create_dir_all(&trash).unwrap();
    fs::write(trash.join("deleted.md"), b"deleted").unwrap();
    // Non-Markdown vault artifacts are never indexed.
    fs::write(root.join("Memory.base"), b"views: []").unwrap();

    let report = scan_corpora(&[test_corpus(&root)]);

    assert_eq!(report.present.len(), 1, "{report:#?}");
    assert!(report.present[0].path.ends_with("page.md"), "{report:#?}");
}
