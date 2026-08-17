use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rustix::fs::{self, AtFlags, Dir, FileType, Mode, OFlags, Stat};
use sha2::{Digest, Sha256};

use super::{
    MAX_FILE_BYTES, NoopRevisionHooks, RevisionHooks, ScanHooks, ScanNote, ScanNoteKind,
    ScanReport, ScannedPage, SnapshotSeal,
};
use crate::corpus::NodeIdentity;
use crate::{Corpus, CorpusKind};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);

pub(super) fn scan_with_hooks(corpora: &[Corpus], hooks: &mut dyn ScanHooks) -> ScanReport {
    let mut report = ScanReport::default();
    let mut claimed_pages = HashSet::new();
    let mut unknown_scopes = HashSet::new();
    let mut seen_notes = HashSet::new();
    let mut seen_corpora = HashSet::new();

    for corpus in corpora {
        let mut local = scan_corpus(corpus, hooks);
        merge_report(
            &mut report,
            &mut local,
            &mut claimed_pages,
            &mut unknown_scopes,
            &mut seen_notes,
            &mut seen_corpora,
        );
    }

    report
        .present
        .sort_by(|left, right| left.path.cmp(&right.path));
    sort_notes(&mut report.oversized);
    sort_notes(&mut report.rejected);
    sort_notes(&mut report.unknown);
    report.scanned_corpora.retain(|root| {
        !report
            .unknown
            .iter()
            .any(|note| scopes_overlap(&note.path, root))
    });
    report.namespace_revisions.retain(|revision| {
        report
            .scanned_corpora
            .iter()
            .any(|root| root == revision.root_path())
    });
    report.reseal();
    report
}

fn scan_corpus(corpus: &Corpus, hooks: &mut dyn ScanHooks) -> ScanReport {
    let mut report = ScanReport::default();
    let pinned = match PinnedRoot::open(corpus) {
        Ok(pinned) => Arc::new(pinned),
        Err(note) => {
            report.unknown.push(note);
            return report;
        }
    };

    let root_fd = match rustix::io::dup(pinned.root_fd()) {
        Ok(fd) => fd,
        Err(error) => {
            unreadable(
                &mut report,
                corpus.path.clone(),
                format!("cannot duplicate pinned corpus descriptor: {error}"),
            );
            return report;
        }
    };
    let root_state = match node_state(&root_fd) {
        Ok(state) => state,
        Err(error) => {
            unreadable(
                &mut report,
                corpus.path.clone(),
                format!("cannot inspect pinned corpus descriptor: {error}"),
            );
            return report;
        }
    };
    let root_frame = make_frame(
        root_fd,
        corpus.path.clone(),
        FrameRevision {
            name_in_parent: None,
            start: root_state,
            root: Arc::clone(&pinned),
            directories: Arc::from([]),
        },
        Checkpoint::at(&report),
        &mut report,
        hooks,
    );
    let mut stack = vec![root_frame];
    let mut completed_directories = Vec::new();

    while !stack.is_empty() {
        let step = {
            let frame = stack.last_mut().expect("nonempty stack");
            next_directory_entry(frame)
        };

        match step {
            DirectoryStep::Entry(name) => {
                let action = {
                    let frame = stack.last().expect("nonempty stack");
                    inspect_entry(frame, &name, corpus.kind, &mut report, hooks)
                };
                let injected_failure = {
                    let frame = stack.last_mut().expect("nonempty stack");
                    frame.processed_entries += 1;
                    hooks.fail_directory_iteration_after_processed_entry(
                        &frame.path,
                        frame.processed_entries,
                    )
                };
                if injected_failure {
                    let path = {
                        let frame = stack.last_mut().expect("nonempty stack");
                        frame.stream = None;
                        frame.own_complete = false;
                        frame.subtree_complete = false;
                        frame.path.clone()
                    };
                    unreadable(
                        &mut report,
                        path,
                        "injected directory iteration failure".to_owned(),
                    );
                }
                if let EntryAction::Descend {
                    fd,
                    path,
                    state,
                    name,
                } = action
                {
                    let (root, directories) = {
                        let parent = stack.last().expect("child has parent");
                        let mut directories = parent.directories.to_vec();
                        directories.push(DirectorySnapshot {
                            name: name.clone(),
                            state,
                        });
                        (Arc::clone(&parent.root), Arc::from(directories))
                    };
                    let frame = make_frame(
                        fd,
                        path,
                        FrameRevision {
                            name_in_parent: Some(name),
                            start: state,
                            root,
                            directories,
                        },
                        Checkpoint::at(&report),
                        &mut report,
                        hooks,
                    );
                    stack.push(frame);
                }
                continue;
            }
            DirectoryStep::Failed(error) => {
                let path = stack.last().expect("nonempty stack").path.clone();
                unreadable(
                    &mut report,
                    path,
                    format!("cannot read a directory entry: {error}"),
                );
                continue;
            }
            DirectoryStep::Exhausted => {}
        }

        let last = stack.len() - 1;
        let stable = if last == 0 {
            let frame = &stack[last];
            descriptor_matches(&frame.fd, &frame.start)
        } else {
            let (parents, children) = stack.split_at(last);
            let parent = parents.last().expect("child has parent");
            let child = &children[0];
            directory_binding_matches(parent, child)
        };

        let own_complete = stack.last().expect("nonempty stack").own_complete;
        if !stable || !own_complete {
            let frame = stack.last_mut().expect("nonempty stack");
            frame.checkpoint.rollback(&mut report);
            frame.subtree_complete = false;
        }
        if !stable {
            let frame = stack.last().expect("nonempty stack");
            raced(
                &mut report,
                frame.path.clone(),
                "directory namespace changed while it was being scanned".to_owned(),
            );
        }

        if stable && own_complete {
            let frame = stack.last().expect("nonempty stack");
            completed_directories.push(NamespaceDirectory {
                path: frame.path.clone(),
                directories: Arc::clone(&frame.directories),
                state: frame.start,
            });
        }
        let completed = stack.pop().expect("nonempty stack").subtree_complete;
        if !completed && let Some(parent) = stack.last_mut() {
            parent.subtree_complete = false;
        }
    }

    if !pinned.revalidate() {
        report.present.clear();
        report.oversized.clear();
        report.rejected.clear();
        raced(
            &mut report,
            corpus.path.clone(),
            "validated corpus binding changed while it was being scanned".to_owned(),
        );
    }

    if report.unknown.is_empty() {
        report.scanned_corpora.push(corpus.path.clone());
        let root_state = completed_directories
            .iter()
            .find(|directory| directory.directories.is_empty())
            .map(|directory| directory.state);
        report.namespace_revisions.push(NamespaceRevision {
            root_path: corpus.path.clone(),
            root: Arc::clone(&pinned),
            root_state,
            directories: completed_directories,
        });
    }
    report
}

enum EntryAction {
    Done,
    Descend {
        fd: OwnedFd,
        path: PathBuf,
        state: NodeState,
        name: OsString,
    },
}

fn inspect_entry(
    frame: &DirectoryFrame,
    name: &OsStr,
    corpus: CorpusKind,
    report: &mut ScanReport,
    hooks: &mut dyn ScanHooks,
) -> EntryAction {
    let path = frame.path.join(name);
    let candidate = match fs::statat(&frame.fd, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => NodeState::from_stat(&stat),
        Err(error) => {
            entry_error(report, path, error, "cannot inspect directory entry");
            return EntryAction::Done;
        }
    };

    if candidate.kind.is_symlink() {
        rejected(
            report,
            path,
            ScanNoteKind::UnsafeSymlink,
            "symlinks are not followed".to_owned(),
        );
        return EntryAction::Done;
    }
    if candidate.kind.is_dir() {
        if name.as_bytes().first() == Some(&b'.') {
            return EntryAction::Done;
        }
        if has_markdown_extension(&path) {
            rejected(
                report,
                path,
                ScanNoteKind::UnsafeDirectory,
                "a directory named like a Markdown page is not scanned".to_owned(),
            );
            return EntryAction::Done;
        }
        return open_directory(frame, name, path, candidate, report);
    }
    if !has_markdown_extension(&path) {
        return EntryAction::Done;
    }
    if is_navigation_page(&path) {
        rejected(
            report,
            path,
            ScanNoteKind::Navigation,
            "wiki navigation filename is not indexed".to_owned(),
        );
        return EntryAction::Done;
    }
    if !candidate.kind.is_file() {
        rejected(
            report,
            path,
            ScanNoteKind::UnsafeFileType,
            "Markdown candidate is not a regular file".to_owned(),
        );
        return EntryAction::Done;
    }
    scan_file(frame, name, path, candidate, corpus, report, hooks);
    EntryAction::Done
}

fn open_directory(
    parent: &DirectoryFrame,
    name: &OsStr,
    path: PathBuf,
    candidate: NodeState,
    report: &mut ScanReport,
) -> EntryAction {
    let fd = match fs::openat(&parent.fd, name, DIRECTORY_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(error) => {
            binding_open_error(
                report,
                &parent.fd,
                name,
                &path,
                &candidate,
                error,
                "cannot safely open directory",
            );
            return EntryAction::Done;
        }
    };
    let opened = match node_state(&fd) {
        Ok(state) => state,
        Err(error) => {
            unreadable(
                report,
                path,
                format!("cannot inspect opened directory: {error}"),
            );
            return EntryAction::Done;
        }
    };
    if opened != candidate || !opened.kind.is_dir() {
        raced(
            report,
            path,
            "directory changed between inspection and open".to_owned(),
        );
        return EntryAction::Done;
    }
    EntryAction::Descend {
        fd,
        path,
        state: opened,
        name: name.to_os_string(),
    }
}

fn scan_file(
    parent: &DirectoryFrame,
    name: &OsStr,
    path: PathBuf,
    candidate: NodeState,
    corpus: CorpusKind,
    report: &mut ScanReport,
    hooks: &mut dyn ScanHooks,
) {
    if candidate.nlink > 1 {
        rejected(
            report,
            path,
            ScanNoteKind::UnsafeHardLink,
            format!("page has {} hard links", candidate.nlink),
        );
        return;
    }
    if candidate.size > MAX_FILE_BYTES {
        oversized(
            report,
            path,
            format!(
                "page is {} bytes, above the {} byte cap",
                candidate.size, MAX_FILE_BYTES
            ),
        );
        return;
    }

    hooks.before_file_open(&path);
    if hooks.fail_file_open(&path) {
        unreadable(report, path, "injected safe-open failure".to_owned());
        return;
    }
    let fd = match fs::openat(&parent.fd, name, FILE_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(error) => {
            binding_open_error(
                report,
                &parent.fd,
                name,
                &path,
                &candidate,
                error,
                "cannot safely open page",
            );
            return;
        }
    };
    let opened = match node_state(&fd) {
        Ok(state) => state,
        Err(error) => {
            unreadable(report, path, format!("cannot inspect opened page: {error}"));
            return;
        }
    };
    if opened.device != candidate.device || opened.inode != candidate.inode {
        raced(
            report,
            path,
            "page was replaced between inspection and open".to_owned(),
        );
        return;
    }
    if !opened.kind.is_file() {
        raced(
            report,
            path,
            "opened descriptor is no longer a regular file".to_owned(),
        );
        return;
    }
    if opened.size > MAX_FILE_BYTES {
        oversized(
            report,
            path,
            format!(
                "opened page grew to {} bytes, above the {} byte cap",
                opened.size, MAX_FILE_BYTES
            ),
        );
        return;
    }
    if opened != candidate {
        raced(
            report,
            path,
            "page metadata changed between inspection and open".to_owned(),
        );
        return;
    }

    hooks.before_file_read(&path);
    let before_read = match coherent_file_binding(&parent.fd, name, &fd, &opened) {
        Ok(state) => state,
        Err(detail) => {
            raced(report, path, detail);
            return;
        }
    };
    if before_read.size > MAX_FILE_BYTES {
        oversized(
            report,
            path,
            format!(
                "page grew to {} bytes, above the {} byte cap before reading",
                before_read.size, MAX_FILE_BYTES
            ),
        );
        return;
    }
    if before_read != opened {
        raced(
            report,
            path,
            "page metadata changed before reading".to_owned(),
        );
        return;
    }

    let mut file = File::from(fd);
    let mut bytes = Vec::with_capacity(before_read.size as usize + 1);
    if let Err(error) = Read::by_ref(&mut file)
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
    {
        unreadable(report, path, format!("cannot read page: {error}"));
        return;
    }
    hooks.after_file_read(&path);

    let after_read = match coherent_file_binding(&parent.fd, name, &file, &before_read) {
        Ok(state) => state,
        Err(detail) => {
            raced(report, path, detail);
            return;
        }
    };
    if bytes.len() as u64 > MAX_FILE_BYTES || after_read.size > MAX_FILE_BYTES {
        oversized(
            report,
            path,
            format!(
                "page grew beyond the {} byte cap while reading",
                MAX_FILE_BYTES
            ),
        );
        return;
    }
    if after_read != before_read || after_read.size != bytes.len() as u64 {
        raced(
            report,
            path,
            "page metadata changed while reading".to_owned(),
        );
        return;
    }

    hooks.before_snapshot_publish(&path);
    let sha256 = format!("{:x}", Sha256::digest(&bytes));
    let content: Arc<[u8]> = Arc::from(bytes.into_boxed_slice());
    let revision_path = path.clone();
    let seal = SnapshotSeal {
        path: path.clone(),
        corpus,
        byte_len: content.len() as u64,
        modified_ns: after_read.modified_ns,
        device: after_read.device,
        inode: after_read.inode,
        sha256: sha256.clone(),
        content: Arc::clone(&content),
    };
    report.present.push(ScannedPage {
        path,
        corpus,
        byte_len: content.len() as u64,
        modified_ns: after_read.modified_ns,
        device: after_read.device,
        inode: after_read.inode,
        sha256,
        content,
        revision: PageRevision {
            root: Arc::clone(&parent.root),
            directories: Arc::clone(&parent.directories),
            name: name.to_os_string(),
            path: revision_path,
            state: after_read,
        },
        seal,
    });
}

fn coherent_file_binding(
    parent: &OwnedFd,
    name: &OsStr,
    descriptor: &impl std::os::fd::AsFd,
    expected: &NodeState,
) -> Result<NodeState, String> {
    let opened = node_state(descriptor)
        .map_err(|error| format!("cannot re-check opened page descriptor: {error}"))?;
    let named = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| NodeState::from_stat(&stat))
        .map_err(|error| format!("cannot re-check page pathname: {error}"))?;
    if opened.device != expected.device
        || opened.inode != expected.inode
        || named.device != opened.device
        || named.inode != opened.inode
        || !named.kind.is_file()
        || !opened.kind.is_file()
    {
        return Err("page pathname no longer names the opened regular file".to_owned());
    }
    if named != opened {
        return Err("page pathname metadata differs from its opened descriptor".to_owned());
    }
    Ok(opened)
}

struct DirectoryFrame {
    fd: OwnedFd,
    path: PathBuf,
    name_in_parent: Option<OsString>,
    start: NodeState,
    root: Arc<PinnedRoot>,
    directories: Arc<[DirectorySnapshot]>,
    stream: Option<Dir>,
    processed_entries: usize,
    checkpoint: Checkpoint,
    own_complete: bool,
    subtree_complete: bool,
}

enum DirectoryStep {
    Entry(OsString),
    Failed(rustix::io::Errno),
    Exhausted,
}

fn next_directory_entry(frame: &mut DirectoryFrame) -> DirectoryStep {
    loop {
        let Some(stream) = frame.stream.as_mut() else {
            return DirectoryStep::Exhausted;
        };
        match stream.next() {
            Some(Ok(entry)) => {
                let bytes = entry.file_name().to_bytes();
                if bytes != b"." && bytes != b".." {
                    return DirectoryStep::Entry(OsString::from_vec(bytes.to_vec()));
                }
            }
            Some(Err(error)) => {
                frame.stream = None;
                frame.own_complete = false;
                frame.subtree_complete = false;
                return DirectoryStep::Failed(error);
            }
            None => {
                frame.stream = None;
                return DirectoryStep::Exhausted;
            }
        }
    }
}

fn make_frame(
    fd: OwnedFd,
    path: PathBuf,
    revision: FrameRevision,
    checkpoint: Checkpoint,
    report: &mut ScanReport,
    hooks: &mut dyn ScanHooks,
) -> DirectoryFrame {
    let (stream, own_complete) = match Dir::read_from(&fd) {
        Ok(stream) => {
            hooks.after_directory_stream_opened(&path);
            (Some(stream), true)
        }
        Err(error) => {
            unreadable(
                report,
                path.clone(),
                format!("cannot open directory stream: {error}"),
            );
            (None, false)
        }
    };
    DirectoryFrame {
        fd,
        path,
        name_in_parent: revision.name_in_parent,
        start: revision.start,
        root: revision.root,
        directories: revision.directories,
        stream,
        processed_entries: 0,
        checkpoint,
        own_complete,
        subtree_complete: own_complete,
    }
}

struct FrameRevision {
    name_in_parent: Option<OsString>,
    start: NodeState,
    root: Arc<PinnedRoot>,
    directories: Arc<[DirectorySnapshot]>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct DirectorySnapshot {
    name: OsString,
    state: NodeState,
}

#[derive(Clone)]
struct NamespaceDirectory {
    path: PathBuf,
    directories: Arc<[DirectorySnapshot]>,
    state: NodeState,
}

/// Opaque proof that every directory in a completed corpus namespace retains
/// the binding and membership metadata observed by the scanner.
#[derive(Clone)]
pub(super) struct NamespaceRevision {
    root_path: PathBuf,
    root: Arc<PinnedRoot>,
    root_state: Option<NodeState>,
    directories: Vec<NamespaceDirectory>,
}

impl NamespaceRevision {
    pub(super) fn root_path(&self) -> &Path {
        &self.root_path
    }

    pub(super) fn revalidate(&self) -> Result<(), ScanNote> {
        if !self.root.revalidate_binding() {
            return Err(self.note(
                &self.root_path,
                ScanNoteKind::Raced,
                "pinned corpus binding changed after scanning".to_owned(),
            ));
        }

        for directory in &self.directories {
            self.revalidate_directory(directory)?;
        }

        if !self.root.revalidate_binding() {
            return Err(self.note(
                &self.root_path,
                ScanNoteKind::Raced,
                "pinned corpus binding changed during namespace revalidation".to_owned(),
            ));
        }
        Ok(())
    }

    fn revalidate_directory(&self, revision: &NamespaceDirectory) -> Result<(), ScanNote> {
        let root = rustix::io::dup(self.root.root_fd()).map_err(|error| {
            self.note(
                &revision.path,
                ScanNoteKind::Unreadable,
                format!("cannot duplicate pinned corpus descriptor: {error}"),
            )
        })?;
        if let Some(root_state) = self.root_state {
            let current_root = node_state(&root).map_err(|error| {
                self.note(
                    &revision.path,
                    ScanNoteKind::Unreadable,
                    format!("cannot inspect pinned corpus descriptor: {error}"),
                )
            })?;
            if current_root != root_state {
                return Err(self.note(
                    &revision.path,
                    ScanNoteKind::Raced,
                    "corpus directory membership changed after scanning".to_owned(),
                ));
            }
        }

        let mut opened = root;
        for directory in revision.directories.iter() {
            let named = fs::statat(&opened, &directory.name, AtFlags::SYMLINK_NOFOLLOW)
                .map(|stat| NodeState::from_stat(&stat))
                .map_err(|error| {
                    self.note(
                        &revision.path,
                        errno_note_kind(error),
                        format!("cannot inspect scanned directory binding: {error}"),
                    )
                })?;
            if !named.kind.is_dir() || named.kind.is_symlink() {
                return Err(self.note(
                    &revision.path,
                    ScanNoteKind::Raced,
                    "scanned directory binding is no longer a directory".to_owned(),
                ));
            }
            let next = fs::openat(&opened, &directory.name, DIRECTORY_FLAGS, Mode::empty())
                .map_err(|error| {
                    self.note(
                        &revision.path,
                        errno_note_kind(error),
                        format!("cannot safely reopen scanned directory: {error}"),
                    )
                })?;
            let opened_state = node_state(&next).map_err(|error| {
                self.note(
                    &revision.path,
                    ScanNoteKind::Unreadable,
                    format!("cannot inspect reopened directory descriptor: {error}"),
                )
            })?;
            if named != opened_state || opened_state != directory.state {
                return Err(self.note(
                    &revision.path,
                    ScanNoteKind::Raced,
                    "directory binding or membership changed after scanning".to_owned(),
                ));
            }
            opened = next;
        }
        if node_state(&opened).map_err(|error| {
            self.note(
                &revision.path,
                ScanNoteKind::Unreadable,
                format!("cannot replay directory descriptor: {error}"),
            )
        })? != revision.state
        {
            return Err(self.note(
                &revision.path,
                ScanNoteKind::Raced,
                "directory membership metadata changed after scanning".to_owned(),
            ));
        }
        Ok(())
    }

    fn note(&self, path: &Path, kind: ScanNoteKind, detail: String) -> ScanNote {
        ScanNote {
            path: path.to_path_buf(),
            kind,
            detail,
        }
    }
}

/// Opaque, cloneable proof of the namespace and metadata observed for a page.
#[derive(Clone)]
pub struct PageRevision {
    root: Arc<PinnedRoot>,
    directories: Arc<[DirectorySnapshot]>,
    name: OsString,
    path: PathBuf,
    state: NodeState,
}

impl PageRevision {
    /// Revalidates the pinned corpus, directory walk, and final page binding.
    pub fn revalidate(&self) -> Result<(), ScanNote> {
        self.revalidate_with_hooks(&mut NoopRevisionHooks)
    }

    pub(super) fn revalidate_with_hooks(
        &self,
        hooks: &mut dyn RevisionHooks,
    ) -> Result<(), ScanNote> {
        self.revalidate_once(hooks)?;
        self.revalidate_root_metadata()
    }

    fn revalidate_once(&self, hooks: &mut dyn RevisionHooks) -> Result<(), ScanNote> {
        if !self.root.revalidate_binding() {
            return Err(self.note(
                ScanNoteKind::Raced,
                "pinned corpus binding changed after scanning".to_owned(),
            ));
        }

        let root = rustix::io::dup(self.root.root_fd()).map_err(|error| {
            self.note(
                ScanNoteKind::Unreadable,
                format!("cannot duplicate pinned corpus descriptor: {error}"),
            )
        })?;
        let mut opened_directories = vec![root];
        let mut logical_directory = self
            .path
            .ancestors()
            .nth(self.directories.len() + 1)
            .unwrap_or_else(|| Path::new("/"))
            .to_path_buf();
        for directory in self.directories.iter() {
            let Some(parent) = opened_directories.last() else {
                return Err(self.note(
                    ScanNoteKind::Unreadable,
                    "revalidation lost its transient corpus descriptor".to_owned(),
                ));
            };
            logical_directory.push(&directory.name);
            let named = fs::statat(parent, &directory.name, AtFlags::SYMLINK_NOFOLLOW)
                .map(|stat| NodeState::from_stat(&stat))
                .map_err(|error| {
                    self.note(
                        errno_note_kind(error),
                        format!("cannot inspect traversed directory: {error}"),
                    )
                })?;
            if named.kind.is_symlink() {
                return Err(self.note(
                    ScanNoteKind::UnsafeSymlink,
                    "traversed directory became a symlink".to_owned(),
                ));
            }
            if !named.kind.is_dir() {
                return Err(self.note(
                    ScanNoteKind::UnsafeFileType,
                    "traversed directory is no longer a directory".to_owned(),
                ));
            }
            hooks.before_directory_open(&logical_directory);
            let opened = fs::openat(parent, &directory.name, DIRECTORY_FLAGS, Mode::empty())
                .map_err(|error| {
                    self.binding_open_note(
                        parent,
                        &directory.name,
                        &named,
                        error,
                        "cannot safely open traversed directory",
                    )
                })?;
            let opened_state = node_state(&opened).map_err(|error| {
                self.note(
                    ScanNoteKind::Unreadable,
                    format!("cannot inspect traversed directory descriptor: {error}"),
                )
            })?;
            if named != opened_state || opened_state != directory.state {
                return Err(self.note(
                    ScanNoteKind::Raced,
                    "traversed directory binding or metadata changed after scanning".to_owned(),
                ));
            }
            opened_directories.push(opened);
        }

        let Some(parent) = opened_directories.last() else {
            return Err(self.note(
                ScanNoteKind::Unreadable,
                "revalidation lost its transient page-parent descriptor".to_owned(),
            ));
        };
        let named = fs::statat(parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)
            .map(|stat| NodeState::from_stat(&stat))
            .map_err(|error| {
                self.note(
                    errno_note_kind(error),
                    format!("cannot inspect scanned page name: {error}"),
                )
            })?;
        self.validate_file_policy(&named)?;
        hooks.before_file_open(&self.path);
        let opened =
            fs::openat(parent, &self.name, FILE_FLAGS, Mode::empty()).map_err(|error| {
                self.binding_open_note(
                    parent,
                    &self.name,
                    &named,
                    error,
                    "cannot safely open scanned page descriptor",
                )
            })?;
        let opened_state = node_state(&opened).map_err(|error| {
            self.note(
                ScanNoteKind::Unreadable,
                format!("cannot inspect scanned page descriptor: {error}"),
            )
        })?;
        self.validate_file_policy(&opened_state)?;
        if named != opened_state || opened_state != self.state {
            return Err(self.note(
                ScanNoteKind::Raced,
                "scanned page binding or metadata changed after reading".to_owned(),
            ));
        }
        self.replay_bindings(&opened_directories, &opened)
    }

    fn replay_bindings(
        &self,
        opened_directories: &[OwnedFd],
        opened_file: &OwnedFd,
    ) -> Result<(), ScanNote> {
        if !self.root.revalidate_binding() {
            return Err(self.note(
                ScanNoteKind::Raced,
                "pinned corpus binding changed during revalidation".to_owned(),
            ));
        }
        let Some(parent) = opened_directories.last() else {
            return Err(self.note(
                ScanNoteKind::Unreadable,
                "revalidation lost its final transient directory descriptor".to_owned(),
            ));
        };
        let named = fs::statat(parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)
            .map(|stat| NodeState::from_stat(&stat))
            .map_err(|error| {
                self.note(
                    errno_note_kind(error),
                    format!("cannot replay scanned page name: {error}"),
                )
            })?;
        self.validate_file_policy(&named)?;
        let opened_state = node_state(opened_file).map_err(|error| {
            self.note(
                ScanNoteKind::Unreadable,
                format!("cannot replay scanned page descriptor: {error}"),
            )
        })?;
        self.validate_file_policy(&opened_state)?;
        if named != opened_state || opened_state != self.state {
            return Err(self.note(
                ScanNoteKind::Raced,
                "scanned page binding changed during revalidation".to_owned(),
            ));
        }

        for (index, directory) in self.directories.iter().enumerate().rev() {
            let (Some(parent), Some(opened)) = (
                opened_directories.get(index),
                opened_directories.get(index + 1),
            ) else {
                return Err(self.note(
                    ScanNoteKind::Unreadable,
                    "revalidation lost a transient directory descriptor".to_owned(),
                ));
            };
            let named = fs::statat(parent, &directory.name, AtFlags::SYMLINK_NOFOLLOW)
                .map(|stat| NodeState::from_stat(&stat))
                .map_err(|error| {
                    self.note(
                        errno_note_kind(error),
                        format!("cannot replay traversed directory binding: {error}"),
                    )
                })?;
            if named.kind.is_symlink() {
                return Err(self.note(
                    ScanNoteKind::UnsafeSymlink,
                    "traversed directory became a symlink during revalidation".to_owned(),
                ));
            }
            let opened_state = node_state(opened).map_err(|error| {
                self.note(
                    ScanNoteKind::Unreadable,
                    format!("cannot replay traversed directory descriptor: {error}"),
                )
            })?;
            if named != opened_state || opened_state != directory.state {
                return Err(self.note(
                    ScanNoteKind::Raced,
                    "traversed directory binding changed during revalidation".to_owned(),
                ));
            }
        }
        Ok(())
    }

    fn revalidate_root_metadata(&self) -> Result<(), ScanNote> {
        if self.root.revalidate() {
            Ok(())
        } else {
            Err(self.note(
                ScanNoteKind::Raced,
                "pinned corpus metadata changed after scanning".to_owned(),
            ))
        }
    }

    fn binding_open_note(
        &self,
        parent: &OwnedFd,
        name: &OsStr,
        observed: &NodeState,
        error: rustix::io::Errno,
        context: &str,
    ) -> ScanNote {
        let current = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
            .map(|stat| NodeState::from_stat(&stat));
        match current {
            Ok(current) if current.kind.is_symlink() => self.note(
                ScanNoteKind::UnsafeSymlink,
                format!("{context}: name became a symlink: {error}"),
            ),
            Ok(current) if current != *observed => self.note(
                ScanNoteKind::Raced,
                format!("{context}: name changed before open: {error}"),
            ),
            Ok(_) if matches!(error, rustix::io::Errno::LOOP | rustix::io::Errno::NOTDIR) => self
                .note(
                    ScanNoteKind::Raced,
                    format!("{context}: binding could not be reopened safely: {error}"),
                ),
            Ok(_) => self.note(errno_note_kind(error), format!("{context}: {error}")),
            Err(_) => self.note(
                ScanNoteKind::Raced,
                format!("{context}: name could not be rechecked after failure: {error}"),
            ),
        }
    }

    fn validate_file_policy(&self, state: &NodeState) -> Result<(), ScanNote> {
        if state.kind.is_symlink() {
            return Err(self.note(
                ScanNoteKind::UnsafeSymlink,
                "scanned page became a symlink".to_owned(),
            ));
        }
        if !state.kind.is_file() {
            return Err(self.note(
                ScanNoteKind::UnsafeFileType,
                "scanned page is no longer a regular file".to_owned(),
            ));
        }
        if state.nlink > 1 {
            return Err(self.note(
                ScanNoteKind::UnsafeHardLink,
                format!("scanned page has {} hard links", state.nlink),
            ));
        }
        if state.size > MAX_FILE_BYTES {
            return Err(self.note(
                ScanNoteKind::Oversized,
                format!(
                    "scanned page is {} bytes, above the {} byte cap",
                    state.size, MAX_FILE_BYTES
                ),
            ));
        }
        Ok(())
    }

    fn note(&self, kind: ScanNoteKind, detail: String) -> ScanNote {
        ScanNote {
            path: self.path.clone(),
            kind,
            detail,
        }
    }
}

impl std::fmt::Debug for PageRevision {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PageRevision")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

fn directory_binding_matches(parent: &DirectoryFrame, child: &DirectoryFrame) -> bool {
    let Some(name) = child.name_in_parent.as_deref() else {
        return false;
    };
    let Ok(opened) = node_state(&child.fd) else {
        return false;
    };
    let Ok(named) = fs::statat(&parent.fd, name, AtFlags::SYMLINK_NOFOLLOW) else {
        return false;
    };
    opened == child.start && NodeState::from_stat(&named) == child.start
}

fn descriptor_matches(fd: &OwnedFd, expected: &NodeState) -> bool {
    node_state(fd).is_ok_and(|current| current == *expected)
}

struct PinnedComponent {
    fd: OwnedFd,
    name_in_parent: Option<OsString>,
    start: NodeState,
}

struct PinnedRoot {
    base_path: PathBuf,
    components: Vec<PinnedComponent>,
}

impl PinnedRoot {
    fn open(corpus: &Corpus) -> Result<Self, ScanNote> {
        let base_fd =
            fs::open(&corpus.validation.base, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
                root_open_note(corpus, error, "cannot safely open confinement base")
            })?;
        let base_state = node_state(&base_fd).map_err(|error| ScanNote {
            path: corpus.path.clone(),
            kind: ScanNoteKind::Unreadable,
            detail: format!("cannot inspect confinement base descriptor: {error}"),
        })?;
        if !base_state.kind.is_dir()
            || !matches_identity(&base_state, corpus.validation.expected_base)
        {
            return Err(ScanNote {
                path: corpus.path.clone(),
                kind: ScanNoteKind::Raced,
                detail: "confinement base no longer has its discovered identity".to_owned(),
            });
        }

        let mut components = vec![PinnedComponent {
            fd: base_fd,
            name_in_parent: None,
            start: base_state,
        }];
        for name in corpus.validation.relative.components() {
            let std::path::Component::Normal(name) = name else {
                return Err(ScanNote {
                    path: corpus.path.clone(),
                    kind: ScanNoteKind::Raced,
                    detail: "validated corpus contains an unsafe path component".to_owned(),
                });
            };
            let parent = &components.last().expect("base component").fd;
            let named = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                .map(|stat| NodeState::from_stat(&stat))
                .map_err(|error| {
                    root_open_note(corpus, error, "cannot inspect validated corpus component")
                })?;
            if !named.kind.is_dir() {
                return Err(ScanNote {
                    path: corpus.path.clone(),
                    kind: ScanNoteKind::Raced,
                    detail: "validated corpus component is no longer a directory".to_owned(),
                });
            }
            let fd = fs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
                root_open_note(
                    corpus,
                    error,
                    "cannot safely open validated corpus component",
                )
            })?;
            let opened = node_state(&fd).map_err(|error| ScanNote {
                path: corpus.path.clone(),
                kind: ScanNoteKind::Unreadable,
                detail: format!("cannot inspect validated corpus component: {error}"),
            })?;
            if opened != named {
                return Err(ScanNote {
                    path: corpus.path.clone(),
                    kind: ScanNoteKind::Raced,
                    detail: "validated corpus component changed while opening it".to_owned(),
                });
            }
            components.push(PinnedComponent {
                fd,
                name_in_parent: Some(name.to_os_string()),
                start: opened,
            });
        }

        let root = &components.last().expect("base component").start;
        if !matches_identity(root, corpus.validation.expected_root) {
            return Err(ScanNote {
                path: corpus.path.clone(),
                kind: ScanNoteKind::Raced,
                detail: "corpus root no longer has its discovered identity".to_owned(),
            });
        }
        Ok(Self {
            base_path: corpus.validation.base.clone(),
            components,
        })
    }

    fn root_fd(&self) -> &OwnedFd {
        &self.components.last().expect("base component").fd
    }

    fn revalidate(&self) -> bool {
        if !self.revalidate_binding() {
            return false;
        }
        self.components
            .iter()
            .enumerate()
            .all(|(index, component)| {
                node_state(&component.fd).is_ok_and(|current| {
                    if index == 0 {
                        same_identity_and_type(&current, &component.start)
                    } else {
                        current == component.start
                    }
                })
            })
    }

    fn revalidate_binding(&self) -> bool {
        let Ok(reopened_base) = fs::open(&self.base_path, DIRECTORY_FLAGS, Mode::empty()) else {
            return false;
        };
        let Ok(reopened_base_state) = node_state(&reopened_base) else {
            return false;
        };
        if !same_identity_and_type(&reopened_base_state, &self.components[0].start) {
            return false;
        }
        for (index, component) in self.components.iter().enumerate() {
            let Ok(current) = node_state(&component.fd) else {
                return false;
            };
            if !same_identity_and_type(&current, &component.start) {
                return false;
            }
            if index == 0 {
                continue;
            }
            let parent = &self.components[index - 1].fd;
            let Some(name) = component.name_in_parent.as_deref() else {
                return false;
            };
            let Ok(named) = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) else {
                return false;
            };
            if !same_identity_and_type(&NodeState::from_stat(&named), &component.start) {
                return false;
            }
        }
        true
    }
}

fn same_identity_and_type(left: &NodeState, right: &NodeState) -> bool {
    left.device == right.device && left.inode == right.inode && left.kind == right.kind
}

fn root_open_note(corpus: &Corpus, error: rustix::io::Errno, context: &str) -> ScanNote {
    ScanNote {
        path: corpus.path.clone(),
        kind: errno_note_kind(error),
        detail: format!("{context}: {error}"),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NodeState {
    device: u64,
    inode: u64,
    mode: u32,
    kind: FileType,
    nlink: u64,
    size: u64,
    modified_ns: i128,
    changed_ns: i128,
}

impl NodeState {
    // rustix exposes different Stat field widths across supported targets.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            mode: stat.st_mode as u32,
            kind: FileType::from_raw_mode(stat.st_mode),
            nlink: stat.st_nlink as u64,
            size: u64::try_from(stat.st_size).unwrap_or(0),
            modified_ns: timestamp_ns(stat.st_mtime as i64, stat.st_mtime_nsec as i64),
            changed_ns: timestamp_ns(stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        }
    }
}

fn node_state(fd: &impl std::os::fd::AsFd) -> rustix::io::Result<NodeState> {
    fs::fstat(fd).map(|stat| NodeState::from_stat(&stat))
}

fn matches_identity(state: &NodeState, expected: NodeIdentity) -> bool {
    state.device == expected.device && state.inode == expected.inode
}

fn timestamp_ns(seconds: i64, nanoseconds: i64) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds)
}

#[derive(Clone, Copy)]
struct Checkpoint {
    present: usize,
    oversized: usize,
    rejected: usize,
}

impl Checkpoint {
    fn at(report: &ScanReport) -> Self {
        Self {
            present: report.present.len(),
            oversized: report.oversized.len(),
            rejected: report.rejected.len(),
        }
    }

    fn rollback(self, report: &mut ScanReport) {
        report.present.truncate(self.present);
        report.oversized.truncate(self.oversized);
        report.rejected.truncate(self.rejected);
    }
}

fn binding_open_error(
    report: &mut ScanReport,
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
    candidate: &NodeState,
    error: rustix::io::Errno,
    context: &str,
) {
    let unchanged = fs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| NodeState::from_stat(&stat))
        .is_ok_and(|current| current == *candidate);
    if unchanged {
        unreadable(report, path.to_path_buf(), format!("{context}: {error}"));
    } else {
        raced(
            report,
            path.to_path_buf(),
            format!("{context}; pathname changed or could not be rebound: {error}"),
        );
    }
}

fn entry_error(report: &mut ScanReport, path: PathBuf, error: rustix::io::Errno, context: &str) {
    let kind = errno_note_kind(error);
    report.unknown.push(ScanNote {
        path,
        kind,
        detail: format!("{context}: {error}"),
    });
}

fn errno_note_kind(error: rustix::io::Errno) -> ScanNoteKind {
    if matches!(error, rustix::io::Errno::ACCESS | rustix::io::Errno::PERM) {
        ScanNoteKind::Unreadable
    } else if error == rustix::io::Errno::NOENT {
        ScanNoteKind::Raced
    } else {
        ScanNoteKind::Unreadable
    }
}

fn has_markdown_extension(path: &Path) -> bool {
    path.extension() == Some(OsStr::new("md"))
}

fn is_navigation_page(path: &Path) -> bool {
    matches!(
        path.file_name().and_then(OsStr::to_str),
        Some("README.md" | "MEMORY.md" | "INDEX.md" | "SCHEMA.md")
    )
}

fn oversized(report: &mut ScanReport, path: PathBuf, detail: String) {
    report.oversized.push(ScanNote {
        path,
        kind: ScanNoteKind::Oversized,
        detail,
    });
}

fn rejected(report: &mut ScanReport, path: PathBuf, kind: ScanNoteKind, detail: String) {
    report.rejected.push(ScanNote { path, kind, detail });
}

fn raced(report: &mut ScanReport, path: PathBuf, detail: String) {
    report.unknown.push(ScanNote {
        path,
        kind: ScanNoteKind::Raced,
        detail,
    });
}

fn unreadable(report: &mut ScanReport, path: PathBuf, detail: String) {
    report.unknown.push(ScanNote {
        path,
        kind: ScanNoteKind::Unreadable,
        detail,
    });
}

fn merge_report(
    target: &mut ScanReport,
    source: &mut ScanReport,
    claimed_pages: &mut HashSet<PathBuf>,
    unknown_scopes: &mut HashSet<PathBuf>,
    seen_notes: &mut HashSet<(PathBuf, ScanNoteKind, String)>,
    seen_corpora: &mut HashSet<PathBuf>,
) {
    for note in source.unknown.drain(..) {
        if unknown_scopes.insert(note.path.clone()) {
            target
                .present
                .retain(|page| !page.path.starts_with(&note.path));
            target
                .oversized
                .retain(|prior| !prior.path.starts_with(&note.path));
            target
                .rejected
                .retain(|prior| !prior.path.starts_with(&note.path));
            target
                .scanned_corpora
                .retain(|root| !scopes_overlap(&note.path, root));
            target
                .namespace_revisions
                .retain(|revision| !scopes_overlap(&note.path, revision.root_path()));
            claimed_pages.insert(note.path.clone());
        }
        merge_distinct_note(&mut target.unknown, note, seen_notes);
    }
    for note in source.rejected.drain(..) {
        if !under_unknown(&note.path, unknown_scopes) && claimed_pages.insert(note.path.clone()) {
            merge_distinct_note(&mut target.rejected, note, seen_notes);
        }
    }
    for note in source.oversized.drain(..) {
        if !under_unknown(&note.path, unknown_scopes) && claimed_pages.insert(note.path.clone()) {
            merge_distinct_note(&mut target.oversized, note, seen_notes);
        }
    }
    for page in source.present.drain(..) {
        if !under_unknown(&page.path, unknown_scopes) && claimed_pages.insert(page.path.clone()) {
            target.present.push(page);
        }
    }
    for path in source.scanned_corpora.drain(..) {
        if !unknown_scopes
            .iter()
            .any(|unknown| scopes_overlap(unknown, &path))
            && seen_corpora.insert(path.clone())
        {
            if let Some(index) = source
                .namespace_revisions
                .iter()
                .position(|revision| revision.root_path() == path)
            {
                target
                    .namespace_revisions
                    .push(source.namespace_revisions.swap_remove(index));
            }
            target.scanned_corpora.push(path);
        }
    }
}

fn under_unknown(path: &Path, unknown_scopes: &HashSet<PathBuf>) -> bool {
    unknown_scopes
        .iter()
        .any(|unknown| path.starts_with(unknown))
}

fn scopes_overlap(left: &Path, right: &Path) -> bool {
    left.starts_with(right) || right.starts_with(left)
}

fn merge_distinct_note(
    destination: &mut Vec<ScanNote>,
    note: ScanNote,
    seen_notes: &mut HashSet<(PathBuf, ScanNoteKind, String)>,
) {
    let key = (note.path.clone(), note.kind, note.detail.clone());
    if seen_notes.insert(key) {
        destination.push(note);
    }
}

fn sort_notes(notes: &mut [ScanNote]) {
    notes.sort_by(|left, right| {
        left.path
            .cmp(&right.path)
            .then_with(|| left.kind.cmp(&right.kind))
            .then_with(|| left.detail.cmp(&right.detail))
    });
}
