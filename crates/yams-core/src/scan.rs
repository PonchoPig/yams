use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Corpus, CorpusKind};

mod traversal;

pub use traversal::PageRevision;

pub const MAX_FILE_BYTES: u64 = 1 << 20;

#[derive(Clone)]
pub struct ScannedPage {
    /// Canonical in-corpus spelling of the page.
    pub path: PathBuf,
    pub corpus: CorpusKind,
    pub byte_len: u64,
    pub modified_ns: i128,
    pub device: u64,
    pub inode: u64,
    pub sha256: String,
    content: Arc<[u8]>,
    revision: PageRevision,
    seal: SnapshotSeal,
}

#[derive(Clone)]
struct SnapshotSeal {
    path: PathBuf,
    corpus: CorpusKind,
    byte_len: u64,
    modified_ns: i128,
    device: u64,
    inode: u64,
    sha256: String,
    content: Arc<[u8]>,
}

impl ScannedPage {
    /// Returns the exact bounded byte snapshot used to calculate `sha256`.
    pub fn content_bytes(&self) -> &[u8] {
        &self.content
    }

    /// Returns the opaque filesystem revision captured with this page.
    pub fn revision(&self) -> &PageRevision {
        &self.revision
    }

    /// Validates that public snapshot metadata still matches the exact bytes
    /// and opaque identity captured by the scanner.
    pub fn validate_snapshot(&self) -> Result<(), ScanNote> {
        if self.path != self.seal.path
            || self.corpus != self.seal.corpus
            || self.byte_len != self.seal.byte_len
            || self.modified_ns != self.seal.modified_ns
            || self.device != self.seal.device
            || self.inode != self.seal.inode
            || self.sha256 != self.seal.sha256
            || self.byte_len != self.content.len() as u64
            || !Arc::ptr_eq(&self.content, &self.seal.content)
        {
            return Err(ScanNote {
                path: self.seal.path.clone(),
                kind: ScanNoteKind::Raced,
                detail: "captured page snapshot fields changed after scanning".to_owned(),
            });
        }
        Ok(())
    }

    /// Revalidates the sealed snapshot and source binding without rereading the
    /// source file or retaining a page descriptor.
    pub fn revalidate(&self) -> Result<(), ScanNote> {
        self.validate_snapshot()?;
        self.revision.revalidate()
    }
}

impl fmt::Debug for ScannedPage {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScannedPage")
            .field("path", &self.path)
            .field("corpus", &self.corpus)
            .field("byte_len", &self.byte_len)
            .field("modified_ns", &self.modified_ns)
            .field("device", &self.device)
            .field("inode", &self.inode)
            .field("sha256", &self.sha256)
            .field("revision", &self.revision)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ScannedPage {
    fn eq(&self, other: &Self) -> bool {
        self.path == other.path
            && self.corpus == other.corpus
            && self.byte_len == other.byte_len
            && self.modified_ns == other.modified_ns
            && self.device == other.device
            && self.inode == other.inode
            && self.sha256 == other.sha256
            && self.content == other.content
    }
}

impl Eq for ScannedPage {}

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ScanNoteKind {
    Oversized,
    Raced,
    Unreadable,
    UnsafeSymlink,
    UnsafeDirectory,
    UnsafeHardLink,
    OutsideCorpus,
    UnsafeFileType,
    Navigation,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScanNote {
    /// Logical candidate path beneath its validated corpus root.
    pub path: PathBuf,
    pub kind: ScanNoteKind,
    pub detail: String,
}

#[derive(Clone, Default)]
pub struct ScanReport {
    pub present: Vec<ScannedPage>,
    pub oversized: Vec<ScanNote>,
    /// Stable policy rejections which are positively known not to be pages.
    pub rejected: Vec<ScanNote>,
    /// Incomplete observations which dominate descendant positives and every
    /// overlapping complete corpus scope, regardless of corpus order.
    /// Later success in the same aggregate scan cannot restore that scope.
    pub unknown: Vec<ScanNote>,
    /// Validated corpus roots whose entire namespace was observed completely.
    pub scanned_corpora: Vec<PathBuf>,
    namespace_revisions: Vec<traversal::NamespaceRevision>,
    seal: Option<ReportSnapshotSeal>,
}

#[derive(Clone, Eq, PartialEq)]
struct ReportSnapshotSeal {
    present: Vec<PageSnapshotFields>,
    oversized: Vec<ScanNote>,
    rejected: Vec<ScanNote>,
    unknown: Vec<ScanNote>,
    scanned_corpora: Vec<PathBuf>,
}

#[derive(Clone, Eq, PartialEq)]
struct PageSnapshotFields {
    path: PathBuf,
    corpus: CorpusKind,
    byte_len: u64,
    modified_ns: i128,
    device: u64,
    inode: u64,
    sha256: String,
}

impl ScanReport {
    /// Validates that every public observation and complete namespace scope
    /// still matches the immutable report emitted by the scanner.
    pub fn validate_snapshot(&self) -> Result<(), ScanNote> {
        let actual = ReportSnapshotSeal::from_report(self);
        let valid = self.seal.as_ref().is_some_and(|seal| seal == &actual)
            && self
                .present
                .iter()
                .all(|page| page.validate_snapshot().is_ok());
        if valid {
            return Ok(());
        }
        Err(ScanNote {
            path: self
                .scanned_corpora
                .first()
                .or_else(|| self.present.first().map(|page| &page.path))
                .cloned()
                .unwrap_or_default(),
            kind: ScanNoteKind::Raced,
            detail: "captured scan report fields changed after scanning".to_owned(),
        })
    }

    /// Revalidates every completely observed directory namespace without
    /// rereading any page content.
    pub fn revalidate_namespaces(&self) -> Result<(), ScanNote> {
        self.validate_snapshot()?;
        for revision in &self.namespace_revisions {
            revision.revalidate()?;
        }
        Ok(())
    }

    pub(super) fn reseal(&mut self) {
        self.seal = Some(ReportSnapshotSeal::from_report(self));
    }
}

impl ReportSnapshotSeal {
    fn from_report(report: &ScanReport) -> Self {
        Self {
            present: report
                .present
                .iter()
                .map(PageSnapshotFields::from)
                .collect(),
            oversized: report.oversized.clone(),
            rejected: report.rejected.clone(),
            unknown: report.unknown.clone(),
            scanned_corpora: report.scanned_corpora.clone(),
        }
    }
}

impl From<&ScannedPage> for PageSnapshotFields {
    fn from(page: &ScannedPage) -> Self {
        Self {
            path: page.path.clone(),
            corpus: page.corpus,
            byte_len: page.byte_len,
            modified_ns: page.modified_ns,
            device: page.device,
            inode: page.inode,
            sha256: page.sha256.clone(),
        }
    }
}

impl fmt::Debug for ScanReport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ScanReport")
            .field("present", &self.present)
            .field("oversized", &self.oversized)
            .field("rejected", &self.rejected)
            .field("unknown", &self.unknown)
            .field("scanned_corpora", &self.scanned_corpora)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ScanReport {
    fn eq(&self, other: &Self) -> bool {
        self.present == other.present
            && self.oversized == other.oversized
            && self.rejected == other.rejected
            && self.unknown == other.unknown
            && self.scanned_corpora == other.scanned_corpora
    }
}

impl Eq for ScanReport {}

pub fn scan_corpora(corpora: &[Corpus]) -> ScanReport {
    scan_corpora_with_hooks(corpora, &mut NoopHooks)
}

fn scan_corpora_with_hooks(corpora: &[Corpus], hooks: &mut dyn ScanHooks) -> ScanReport {
    traversal::scan_with_hooks(corpora, hooks)
}

trait ScanHooks {
    fn after_directory_stream_opened(&mut self, _path: &Path) {}
    fn fail_directory_iteration_after_processed_entry(
        &mut self,
        _path: &Path,
        _processed: usize,
    ) -> bool {
        false
    }
    fn before_file_open(&mut self, _path: &Path) {}
    fn fail_file_open(&mut self, _path: &Path) -> bool {
        false
    }
    fn before_file_read(&mut self, _path: &Path) {}
    fn after_file_read(&mut self, _path: &Path) {}
    fn before_snapshot_publish(&mut self, _path: &Path) {}
}

struct NoopHooks;

impl ScanHooks for NoopHooks {}

trait RevisionHooks {
    fn before_directory_open(&mut self, _path: &Path) {}
    fn before_file_open(&mut self, _path: &Path) {}
}

struct NoopRevisionHooks;

impl RevisionHooks for NoopRevisionHooks {}

#[cfg(test)]
mod tests;
