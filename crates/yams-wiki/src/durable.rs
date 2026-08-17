use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::index::{
    IndexError, IndexPage, adopt_legacy, check_index, compare_index, rebuild_index,
};
use crate::lock::{
    LOCK_TIMEOUT, LockError, LockGuard, LockLease, LockMode, UnisolatedReason,
    acquire_lock_with_timeout,
};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
const TEMP_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
const INDEX_NAME: &str = "INDEX.md";
const PAGES_NAME: &str = "pages";
const TEMP_ATTEMPTS: u64 = 128;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Parameters for a locked canonical-index operation.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReindexOptions {
    /// Compare and report drift without replacing `INDEX.md`.
    pub check_only: bool,
    /// Perform the one-shot exact legacy-layout adoption before rendering.
    pub adopt: bool,
    /// Refuse unless the captured `INDEX.md` bytes have this SHA-256.
    pub expected_sha256: Option<String>,
    /// Bound for acquiring the shared project lock.
    pub lock_timeout: Duration,
}

impl Default for ReindexOptions {
    fn default() -> Self {
        Self {
            check_only: false,
            adopt: false,
            expected_sha256: None,
            lock_timeout: LOCK_TIMEOUT,
        }
    }
}

/// Result of checking or regenerating the derived index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReindexResult {
    pub changed: bool,
    pub diff: Option<String>,
    /// Digest of the bytes left at `INDEX.md` (or judged in check-only mode).
    pub index_sha256: String,
    /// Present only when a provably non-writable corpus could not host a lock.
    pub isolation_note: Option<String>,
}

/// A digest is returned only for the exact bytes proved canonical.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CanonicalDigest {
    pub digest: Option<String>,
    pub isolation_note: Option<String>,
}

#[derive(Debug, Error)]
pub enum DurableError {
    #[error(transparent)]
    Lock(#[from] LockError),

    #[error(transparent)]
    Index(#[from] IndexError),

    #[error("unsafe wiki object at {path}: {detail}")]
    Unsafe { path: PathBuf, detail: String },

    #[error("wiki changed while {operation}: {path}: {detail}")]
    Raced {
        operation: &'static str,
        path: PathBuf,
        detail: String,
    },

    #[error("could not {operation} {path}: {detail}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        detail: String,
    },

    #[error(
        "INDEX.md changed between the canonicality check and regeneration — inspect it rather than regenerating over it"
    )]
    ExpectedIndexChanged,

    #[error("INDEX.md is not valid UTF-8: {0}")]
    InvalidIndexUtf8(String),

    #[error("pages/{page} is not valid UTF-8: {detail}")]
    InvalidPageUtf8 { page: String, detail: String },

    #[error("pages/{page} filename is not valid UTF-8")]
    InvalidPageName { page: String },

    #[error("mutation requires an isolated exclusive wiki lock; corpus is unisolated ({0})")]
    MutationUnisolated(String),

    #[error("INDEX.md was replaced, but directory durability could not be confirmed: {0}")]
    ReplacedNotDurable(String),

    #[error("{original}; cleanup of {path} also failed: {detail}")]
    CleanupFailed {
        original: Box<DurableError>,
        path: PathBuf,
        detail: String,
    },

    #[error(
        "{original}; temporary path {path} was rebound, so the foreign object was left untouched"
    )]
    TemporaryRebound {
        original: Box<DurableError>,
        path: PathBuf,
    },
}

/// Check or durably regenerate the derived index under the appropriate lock.
///
/// Check-only takes a shared lock. Adoption and replacement take an exclusive
/// lock. A filesystem proven unable to take writes may perform check-only with
/// an isolation note; mutation is always refused without a real lock.
pub fn reindex_wiki(
    corpus: &Path,
    options: &ReindexOptions,
) -> Result<ReindexResult, DurableError> {
    let mode = if options.check_only && !options.adopt {
        LockMode::Shared
    } else {
        LockMode::Exclusive
    };
    match acquire_lock_with_timeout(corpus, mode, options.lock_timeout)? {
        LockLease::Isolated(guard) => reindex_locked(&guard, options),
        LockLease::Unisolated(unisolated) => {
            if !options.check_only || options.adopt {
                return Err(DurableError::MutationUnisolated(unisolated_note(
                    unisolated.reason,
                )));
            }
            let note = unisolated_note(unisolated.reason);
            let mut hooks = SystemHooks;
            let result = reindex_from_fd(
                unisolated.corpus_fd(),
                unisolated.corpus.as_path(),
                options,
                &mut hooks,
                || unisolated.revalidate().map_err(DurableError::from),
            )?;
            Ok(ReindexResult {
                isolation_note: Some(note),
                ..result
            })
        }
    }
}

/// Return the digest of the exact `INDEX.md` bytes proved canonical.
pub fn canonical_index_digest(corpus: &Path) -> Result<CanonicalDigest, DurableError> {
    match acquire_lock_with_timeout(corpus, LockMode::Shared, LOCK_TIMEOUT)? {
        LockLease::Isolated(guard) => Ok(CanonicalDigest {
            digest: canonical_digest_locked(&guard)?,
            isolation_note: None,
        }),
        LockLease::Unisolated(unisolated) => {
            let note = unisolated_note(unisolated.reason);
            let mut hooks = SystemHooks;
            let digest = canonical_digest_from_fd(
                unisolated.corpus_fd(),
                unisolated.corpus.as_path(),
                &mut hooks,
                || unisolated.revalidate().map_err(DurableError::from),
            )?;
            Ok(CanonicalDigest {
                digest,
                isolation_note: Some(note),
            })
        }
    }
}

pub(crate) fn reindex_locked(
    guard: &LockGuard,
    options: &ReindexOptions,
) -> Result<ReindexResult, DurableError> {
    let mut hooks = SystemHooks;
    reindex_locked_with_hooks(guard, options, &mut hooks)
}

pub(crate) fn reindex_locked_with_hooks(
    guard: &LockGuard,
    options: &ReindexOptions,
    hooks: &mut impl DurableHooks,
) -> Result<ReindexResult, DurableError> {
    if guard.mode() != LockMode::Exclusive && (!options.check_only || options.adopt) {
        return Err(DurableError::Unsafe {
            path: guard.lock_path().to_path_buf(),
            detail: "mutation requires an exclusive lock guard".to_owned(),
        });
    }
    reindex_from_fd(
        guard.corpus_fd(),
        guard.corpus_path(),
        options,
        hooks,
        || guard.revalidate_before_commit().map_err(DurableError::from),
    )
}

pub(crate) fn canonical_digest_locked(guard: &LockGuard) -> Result<Option<String>, DurableError> {
    let mut hooks = SystemHooks;
    canonical_digest_from_fd(guard.corpus_fd(), guard.corpus_path(), &mut hooks, || {
        guard.revalidate_before_commit().map_err(DurableError::from)
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct NodeState {
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
    pub(crate) fn from_stat(stat: &Stat) -> Self {
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntrySignature {
    name: OsString,
    state: NodeState,
}

#[derive(Debug)]
struct CapturedPage {
    signature: EntrySignature,
    bytes: Vec<u8>,
}

#[derive(Debug)]
struct CapturedIndex {
    state: NodeState,
    bytes: Vec<u8>,
    digest: String,
}

#[derive(Debug)]
struct CapturedInputs {
    pages_fd: OwnedFd,
    pages_state: NodeState,
    entries: Vec<EntrySignature>,
    pages: Vec<CapturedPage>,
    index: CapturedIndex,
}

pub(crate) trait DurableHooks {
    fn after_index_read(&mut self, _corpus: &Path) {}
    fn after_page_read(&mut self, _corpus: &Path, _name: &OsStr) {}
    fn after_pages_captured(&mut self, _corpus: &Path) {}
    fn before_read_only_return(&mut self, _corpus: &Path) {}
    fn after_temp_created(&mut self, _corpus: &Path, _name: &OsStr) {}
    fn before_final_validation(&mut self, _corpus: &Path) {}
    fn before_rename(&mut self, _corpus: &Path, _name: &OsStr) {}
    fn after_rename(&mut self, _corpus: &Path) {}
    fn fail_directory_iteration(&mut self, _path: &Path, _processed: usize) -> bool {
        false
    }

    fn temporary_name(&mut self, pid: u32, sequence: u64) -> OsString {
        OsString::from(format!(".INDEX.md.{pid}.{sequence}.tmp"))
    }

    fn chmod(&mut self, fd: BorrowedFd<'_>, mode: Mode) -> Result<(), Errno> {
        rfs::fchmod(fd, mode)
    }

    fn write(&mut self, fd: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno> {
        rustix::io::write(fd, bytes)
    }

    fn file_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        rfs::fsync(fd)
    }

    fn rename(&mut self, directory: BorrowedFd<'_>, temporary: &OsStr) -> Result<(), Errno> {
        rfs::renameat(directory, temporary, directory, INDEX_NAME)
    }

    fn directory_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        rfs::fsync(fd)
    }

    fn unlink(&mut self, directory: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
        rfs::unlinkat(directory, name, AtFlags::empty())
    }

    fn temporary_descriptor_state(
        &mut self,
        fd: BorrowedFd<'_>,
        path: &Path,
        operation: &'static str,
    ) -> Result<NodeState, DurableError> {
        descriptor_state(fd, path, operation)
    }

    fn temporary_named_state(
        &mut self,
        directory: BorrowedFd<'_>,
        name: &OsStr,
        path: &Path,
        operation: &'static str,
    ) -> Result<NodeState, DurableError> {
        named_state(directory, name, path, operation)
    }
}

struct SystemHooks;

impl DurableHooks for SystemHooks {}

fn reindex_from_fd(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    options: &ReindexOptions,
    hooks: &mut impl DurableHooks,
    mut revalidate_guard: impl FnMut() -> Result<(), DurableError>,
) -> Result<ReindexResult, DurableError> {
    let index_path = corpus_path.join(INDEX_NAME);
    let index = read_index(corpus_fd, &index_path)?;
    hooks.after_index_read(corpus_path);

    if options
        .expected_sha256
        .as_ref()
        .is_some_and(|expected| expected != &index.digest)
    {
        return Err(DurableError::ExpectedIndexChanged);
    }
    let before = std::str::from_utf8(&index.bytes)
        .map_err(|error| DurableError::InvalidIndexUtf8(error.to_string()))?
        .to_owned();
    let inputs = capture_inputs(corpus_fd, corpus_path, index, hooks)?;
    let pages = parse_captured_pages(&inputs.pages)?;
    let source = if options.adopt {
        adopt_legacy(&before)?
    } else {
        before.clone()
    };
    let canonical = rebuild_index(&source, &pages)?;
    let comparison = if options.adopt {
        compare_index(&before, &canonical)
    } else {
        check_index(&before, &pages)?
    };
    let changed = canonical.as_bytes() != inputs.index.bytes;

    if !changed || options.check_only {
        hooks.before_read_only_return(corpus_path);
        revalidate_inputs(corpus_fd, corpus_path, &inputs, hooks)?;
        revalidate_guard()?;
        return Ok(ReindexResult {
            changed,
            diff: comparison.diff,
            index_sha256: inputs.index.digest,
            isolation_note: None,
        });
    }

    let temporary = create_temporary(corpus_fd, corpus_path, hooks)?;
    let temporary_name = temporary.name.clone();
    hooks.after_temp_created(corpus_path, &temporary_name);
    let outcome = write_and_replace(
        corpus_fd,
        corpus_path,
        &inputs,
        canonical.as_bytes(),
        temporary,
        hooks,
        &mut revalidate_guard,
    );
    match outcome {
        Ok(()) => Ok(ReindexResult {
            changed: true,
            diff: comparison.diff,
            index_sha256: sha256(canonical.as_bytes()),
            isolation_note: None,
        }),
        Err(error) => {
            let original = error.error;
            match cleanup_temporary(corpus_fd, corpus_path, &error.temporary, hooks) {
                Cleanup::Removed | Cleanup::Missing => Err(original),
                Cleanup::Rebound(path) => Err(DurableError::TemporaryRebound {
                    original: Box::new(original),
                    path,
                }),
                Cleanup::Failed(path, detail) => Err(DurableError::CleanupFailed {
                    original: Box::new(original),
                    path,
                    detail,
                }),
            }
        }
    }
}

fn canonical_digest_from_fd(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    hooks: &mut impl DurableHooks,
    mut revalidate_guard: impl FnMut() -> Result<(), DurableError>,
) -> Result<Option<String>, DurableError> {
    let index_path = corpus_path.join(INDEX_NAME);
    let index = read_index(corpus_fd, &index_path)?;
    hooks.after_index_read(corpus_path);
    let before = std::str::from_utf8(&index.bytes).ok().map(str::to_owned);
    let inputs = capture_inputs(corpus_fd, corpus_path, index, hooks)?;
    let verdict = (|| {
        let before = before.as_deref()?;
        let pages = parse_captured_pages(&inputs.pages).ok()?;
        let check = check_index(before, &pages).ok()?;
        check.canonical.then(|| inputs.index.digest.clone())
    })();
    hooks.before_read_only_return(corpus_path);
    revalidate_inputs(corpus_fd, corpus_path, &inputs, hooks)?;
    revalidate_guard()?;
    Ok(verdict)
}

fn capture_inputs(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    index: CapturedIndex,
    hooks: &mut impl DurableHooks,
) -> Result<CapturedInputs, DurableError> {
    let pages_path = corpus_path.join(PAGES_NAME);
    let pages_candidate = named_state(corpus_fd, PAGES_NAME, &pages_path, "inspect pages")?;
    require_kind(&pages_path, &pages_candidate, FileType::is_dir, "directory")?;
    let pages_fd = rfs::openat(corpus_fd, PAGES_NAME, DIRECTORY_FLAGS, Mode::empty())
        .map_err(|error| io_error("open without following links", &pages_path, error))?;
    let pages_state = descriptor_state(&pages_fd, &pages_path, "inspect opened pages")?;
    if (pages_state.device, pages_state.inode) != (pages_candidate.device, pages_candidate.inode)
        || !pages_state.kind.is_dir()
    {
        return Err(raced(
            &pages_path,
            "capturing",
            "pages changed while opening",
        ));
    }

    let entries = enumerate_entries(pages_fd.as_fd(), &pages_path, hooks)?;
    let mut pages = Vec::new();
    for signature in &entries {
        if signature.name.as_bytes().ends_with(b".md") {
            let path = pages_path.join(&signature.name);
            require_kind(&path, &signature.state, FileType::is_file, "regular file")?;
            let bytes = read_bound_file(
                pages_fd.as_fd(),
                &signature.name,
                &path,
                signature.state,
                hooks,
                corpus_path,
            )?;
            pages.push(CapturedPage {
                signature: signature.clone(),
                bytes,
            });
        }
    }
    hooks.after_pages_captured(corpus_path);
    verify_directory_binding(
        corpus_fd,
        OsStr::new(PAGES_NAME),
        pages_fd.as_fd(),
        pages_state,
        &pages_path,
    )?;
    if enumerate_entries(pages_fd.as_fd(), &pages_path, hooks)? != entries {
        return Err(raced(
            &pages_path,
            "capturing",
            "directory entry set changed while it was read",
        ));
    }

    Ok(CapturedInputs {
        pages_fd,
        pages_state,
        entries,
        pages,
        index,
    })
}

fn parse_captured_pages(pages: &[CapturedPage]) -> Result<Vec<IndexPage>, DurableError> {
    let mut parsed = Vec::with_capacity(pages.len());
    for page in pages {
        let name = page
            .signature
            .name
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| DurableError::InvalidPageName {
                page: diagnostic_name(&page.signature.name),
            })?;
        let source =
            std::str::from_utf8(&page.bytes).map_err(|error| DurableError::InvalidPageUtf8 {
                page: name.clone(),
                detail: error.to_string(),
            })?;
        parsed.push(crate::index::parse_index_page(&name, source)?);
    }
    Ok(parsed)
}

fn read_index(corpus_fd: BorrowedFd<'_>, path: &Path) -> Result<CapturedIndex, DurableError> {
    let candidate = named_state(corpus_fd, INDEX_NAME, path, "inspect index")?;
    require_kind(path, &candidate, FileType::is_file, "regular file")?;
    let fd = rfs::openat(corpus_fd, INDEX_NAME, FILE_FLAGS, Mode::empty())
        .map_err(|error| io_error("open without following links", path, error))?;
    let opened = descriptor_state(&fd, path, "inspect opened index")?;
    if opened != candidate {
        return Err(raced(path, "reading", "index changed while opening"));
    }
    let bytes = read_exact_descriptor(fd, path)?;
    let after = named_state(corpus_fd, INDEX_NAME, path, "reinspect index")?;
    if after != opened || after.size != bytes.len() as u64 {
        return Err(raced(path, "reading", "index changed while reading"));
    }
    Ok(CapturedIndex {
        state: after,
        digest: sha256(&bytes),
        bytes,
    })
}

fn read_bound_file(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    path: &Path,
    candidate: NodeState,
    hooks: &mut impl DurableHooks,
    corpus_path: &Path,
) -> Result<Vec<u8>, DurableError> {
    let fd = rfs::openat(parent, name, FILE_FLAGS, Mode::empty())
        .map_err(|error| io_error("open page without following links", path, error))?;
    let opened = descriptor_state(&fd, path, "inspect opened page")?;
    if opened != candidate {
        return Err(raced(path, "reading pages", "page changed while opening"));
    }
    let bytes = read_exact_descriptor(fd, path)?;
    hooks.after_page_read(corpus_path, name);
    let after = named_state(parent, name, path, "reinspect page")?;
    if after != opened || after.size != bytes.len() as u64 {
        return Err(raced(path, "reading pages", "page changed while reading"));
    }
    Ok(bytes)
}

fn read_exact_descriptor(fd: OwnedFd, path: &Path) -> Result<Vec<u8>, DurableError> {
    let before = descriptor_state(&fd, path, "inspect descriptor before reading")?;
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| std_io_error("read", path, error))?;
    let after = descriptor_state(&file, path, "inspect descriptor after reading")?;
    if before != after || after.size != bytes.len() as u64 {
        return Err(raced(path, "reading", "descriptor metadata changed"));
    }
    Ok(bytes)
}

fn enumerate_entries(
    directory: BorrowedFd<'_>,
    path: &Path,
    hooks: &mut impl DurableHooks,
) -> Result<Vec<EntrySignature>, DurableError> {
    let mut stream = Dir::read_from(directory)
        .map_err(|error| io_error("open directory stream", path, error))?;
    let mut names = Vec::new();
    for entry in &mut stream {
        let entry = entry.map_err(|error| io_error("read directory entry", path, error))?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." && bytes.ends_with(b".md") {
            names.push(OsString::from_vec(bytes.to_vec()));
            if hooks.fail_directory_iteration(path, names.len()) {
                return Err(DurableError::Io {
                    operation: "read directory entry",
                    path: path.to_path_buf(),
                    detail: "injected partial directory iteration failure".to_owned(),
                });
            }
        }
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let mut signatures = Vec::with_capacity(names.len());
    let mut seen = HashSet::new();
    for name in names {
        if !seen.insert(name.clone()) {
            return Err(raced(
                path,
                "enumerating pages",
                "duplicate directory entry",
            ));
        }
        let entry_path = path.join(&name);
        let state = named_state(directory, &name, &entry_path, "inspect directory entry")?;
        signatures.push(EntrySignature { name, state });
    }
    Ok(signatures)
}

fn revalidate_inputs(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    inputs: &CapturedInputs,
    hooks: &mut impl DurableHooks,
) -> Result<(), DurableError> {
    let pages_path = corpus_path.join(PAGES_NAME);
    verify_directory_binding(
        corpus_fd,
        OsStr::new(PAGES_NAME),
        inputs.pages_fd.as_fd(),
        inputs.pages_state,
        &pages_path,
    )?;
    if enumerate_entries(inputs.pages_fd.as_fd(), &pages_path, hooks)? != inputs.entries {
        return Err(raced(
            &pages_path,
            "validating snapshot",
            "page name or signature set changed",
        ));
    }
    let index_path = corpus_path.join(INDEX_NAME);
    let current = read_index(corpus_fd, &index_path)?;
    if current.state != inputs.index.state || current.digest != inputs.index.digest {
        return Err(raced(
            &index_path,
            "validating snapshot",
            "index identity, metadata, or digest changed",
        ));
    }
    Ok(())
}

fn verify_directory_binding(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    descriptor: BorrowedFd<'_>,
    expected: NodeState,
    path: &Path,
) -> Result<(), DurableError> {
    let opened = descriptor_state(descriptor, path, "reinspect directory descriptor")?;
    let named = named_state(parent, name, path, "reinspect directory pathname")?;
    let expected_identity = (expected.device, expected.inode);
    if (opened.device, opened.inode) != expected_identity
        || (named.device, named.inode) != expected_identity
        || !opened.kind.is_dir()
        || !named.kind.is_dir()
    {
        return Err(raced(
            path,
            "validating directory binding",
            "pathname no longer names the opened directory",
        ));
    }
    Ok(())
}

#[derive(Debug)]
struct Temporary {
    fd: OwnedFd,
    name: OsString,
    identity: Option<(u64, u64)>,
}

fn create_temporary(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    hooks: &mut impl DurableHooks,
) -> Result<Temporary, DurableError> {
    let pid = std::process::id();
    for _ in 0..TEMP_ATTEMPTS {
        let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = hooks.temporary_name(pid, sequence);
        match rfs::openat(corpus_fd, &name, TEMP_FLAGS, Mode::RUSR | Mode::WUSR) {
            Ok(fd) => {
                return Ok(Temporary {
                    fd,
                    name,
                    identity: None,
                });
            }
            Err(Errno::EXIST) => continue,
            Err(error) => {
                return Err(io_error(
                    "create unique index temporary",
                    &corpus_path.join(name),
                    error,
                ));
            }
        }
    }
    Err(DurableError::Unsafe {
        path: corpus_path.to_path_buf(),
        detail: format!("could not find a unique index temporary after {TEMP_ATTEMPTS} attempts"),
    })
}

struct ReplaceFailure {
    error: DurableError,
    temporary: Temporary,
}

// The failure retains the temporary file so callers can clean it up safely.
#[allow(clippy::result_large_err)]
fn write_and_replace(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    inputs: &CapturedInputs,
    bytes: &[u8],
    mut temporary: Temporary,
    hooks: &mut impl DurableHooks,
    revalidate_guard: &mut impl FnMut() -> Result<(), DurableError>,
) -> Result<(), ReplaceFailure> {
    let result = (|| {
        let index_path = corpus_path.join(INDEX_NAME);
        validate_created_temporary(corpus_fd, corpus_path, &mut temporary, hooks)?;
        let mode = Mode::from_bits_retain((inputs.index.state.mode & 0o7777) as _);
        write_all(hooks, temporary.fd.as_fd(), bytes, &index_path)?;
        hooks
            .chmod(temporary.fd.as_fd(), mode)
            .map_err(|error| io_error("apply INDEX.md mode to temporary", &index_path, error))?;
        let applied =
            descriptor_state(&temporary.fd, &index_path, "verify INDEX.md temporary mode")?;
        if applied.mode & 0o7777 != inputs.index.state.mode & 0o7777 {
            return Err(unsafe_error(
                &index_path,
                format!(
                    "temporary mode {:o} does not match target mode {:o}",
                    applied.mode & 0o7777,
                    inputs.index.state.mode & 0o7777
                ),
            ));
        }
        hooks
            .file_fsync(temporary.fd.as_fd())
            .map_err(|error| io_error("fsync index temporary", &index_path, error))?;

        hooks.before_final_validation(corpus_path);
        revalidate_inputs(corpus_fd, corpus_path, inputs, hooks)?;
        revalidate_guard()?;
        verify_temporary(corpus_fd, corpus_path, &temporary)?;

        hooks.before_rename(corpus_path, &temporary.name);
        // This is the final same-UID race boundary: a peer with directory
        // rename authority can still act after validation and before renameat.
        revalidate_inputs(corpus_fd, corpus_path, inputs, hooks)?;
        revalidate_guard()?;
        verify_temporary(corpus_fd, corpus_path, &temporary)?;
        hooks
            .rename(corpus_fd, &temporary.name)
            .map_err(|error| io_error("replace INDEX.md", &index_path, error))?;
        hooks.after_rename(corpus_path);
        hooks
            .directory_fsync(corpus_fd)
            .map_err(|error| DurableError::ReplacedNotDurable(error.to_string()))?;
        Ok(())
    })();
    result.map_err(|error| ReplaceFailure { error, temporary })
}

fn validate_created_temporary(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    temporary: &mut Temporary,
    hooks: &mut impl DurableHooks,
) -> Result<(), DurableError> {
    let path = corpus_path.join(&temporary.name);
    let descriptor = hooks.temporary_descriptor_state(
        temporary.fd.as_fd(),
        &path,
        "inspect created temporary",
    )?;
    temporary.identity = Some((descriptor.device, descriptor.inode));
    if !descriptor.kind.is_file() || descriptor.nlink != 1 {
        return Err(unsafe_error(
            &path,
            "created temporary must be a regular file with one link",
        ));
    }
    let named =
        hooks.temporary_named_state(corpus_fd, &temporary.name, &path, "bind created temporary")?;
    if named != descriptor {
        return Err(raced(&path, "creating temporary", "name was rebound"));
    }
    Ok(())
}

fn write_all(
    hooks: &mut impl DurableHooks,
    fd: BorrowedFd<'_>,
    mut bytes: &[u8],
    path: &Path,
) -> Result<(), DurableError> {
    while !bytes.is_empty() {
        match hooks.write(fd, bytes) {
            Ok(0) => {
                return Err(DurableError::Io {
                    operation: "write index temporary",
                    path: path.to_path_buf(),
                    detail: "write returned zero bytes".to_owned(),
                });
            }
            Ok(written) => bytes = &bytes[written..],
            Err(Errno::INTR) => {}
            Err(error) => return Err(io_error("write index temporary", path, error)),
        }
    }
    Ok(())
}

fn verify_temporary(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    temporary: &Temporary,
) -> Result<(), DurableError> {
    let path = corpus_path.join(&temporary.name);
    let descriptor = descriptor_state(&temporary.fd, &path, "reinspect index temporary")?;
    let named = named_state(corpus_fd, &temporary.name, &path, "rebind index temporary")?;
    let Some(identity) = temporary.identity else {
        return Err(raced(
            &path,
            "validating temporary",
            "created temporary identity was not established",
        ));
    };
    if (descriptor.device, descriptor.inode) != identity
        || (named.device, named.inode) != identity
        || descriptor != named
        || !descriptor.kind.is_file()
        || descriptor.nlink != 1
    {
        return Err(raced(
            &path,
            "validating temporary",
            "temporary name no longer names the created inode",
        ));
    }
    Ok(())
}

enum Cleanup {
    Removed,
    Missing,
    Rebound(PathBuf),
    Failed(PathBuf, String),
}

fn cleanup_temporary(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    temporary: &Temporary,
    hooks: &mut impl DurableHooks,
) -> Cleanup {
    let path = corpus_path.join(&temporary.name);
    let descriptor = match rfs::fstat(&temporary.fd) {
        Ok(stat) => NodeState::from_stat(&stat),
        Err(error) => return Cleanup::Failed(path, error.to_string()),
    };
    if temporary
        .identity
        .is_some_and(|identity| (descriptor.device, descriptor.inode) != identity)
    {
        return Cleanup::Rebound(path);
    }
    let state = match rfs::statat(corpus_fd, &temporary.name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => NodeState::from_stat(&stat),
        Err(Errno::NOENT) => return Cleanup::Missing,
        Err(error) => return Cleanup::Failed(path, error.to_string()),
    };
    if (state.device, state.inode) != (descriptor.device, descriptor.inode) {
        return Cleanup::Rebound(path);
    }
    match hooks.unlink(corpus_fd, &temporary.name) {
        Ok(()) => Cleanup::Removed,
        Err(Errno::NOENT) => Cleanup::Missing,
        Err(error) => Cleanup::Failed(path, error.to_string()),
    }
}

fn named_state(
    parent: BorrowedFd<'_>,
    name: impl rustix::path::Arg,
    path: &Path,
    operation: &'static str,
) -> Result<NodeState, DurableError> {
    rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| NodeState::from_stat(&stat))
        .map_err(|error| io_error(operation, path, error))
}

fn descriptor_state(
    fd: impl AsFd,
    path: &Path,
    operation: &'static str,
) -> Result<NodeState, DurableError> {
    rfs::fstat(fd)
        .map(|stat| NodeState::from_stat(&stat))
        .map_err(|error| io_error(operation, path, error))
}

fn require_kind(
    path: &Path,
    state: &NodeState,
    predicate: impl FnOnce(FileType) -> bool,
    expected: &str,
) -> Result<(), DurableError> {
    if predicate(state.kind) {
        Ok(())
    } else {
        Err(unsafe_error(
            path,
            format!("expected a non-symlink {expected}"),
        ))
    }
}

fn timestamp_ns(seconds: i64, nanoseconds: i64) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn diagnostic_name(name: &OsStr) -> String {
    if let Some(name) = name.to_str() {
        return name.to_owned();
    }
    let mut escaped = String::new();
    for byte in name.as_bytes() {
        if (0x20..=0x7e).contains(byte) && *byte != b'\\' {
            escaped.push(char::from(*byte));
        } else {
            escaped.push_str(&format!("\\x{byte:02x}"));
        }
    }
    escaped
}

fn unisolated_note(reason: UnisolatedReason) -> String {
    match reason {
        UnisolatedReason::ReadOnlyFilesystem => "read-only filesystem".to_owned(),
        UnisolatedReason::UnwritableCorpus => "unwritable corpus".to_owned(),
    }
}

fn io_error(operation: &'static str, path: &Path, error: Errno) -> DurableError {
    DurableError::Io {
        operation,
        path: path.to_path_buf(),
        detail: error.to_string(),
    }
}

fn std_io_error(operation: &'static str, path: &Path, error: std::io::Error) -> DurableError {
    DurableError::Io {
        operation,
        path: path.to_path_buf(),
        detail: error.to_string(),
    }
}

fn unsafe_error(path: &Path, detail: impl Into<String>) -> DurableError {
    DurableError::Unsafe {
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

fn raced(path: &Path, operation: &'static str, detail: impl Into<String>) -> DurableError {
    DurableError::Raced {
        operation,
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use rustix::fs::{Timespec, Timestamps, stat};
    use std::cell::RefCell;
    use std::fs;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    use std::rc::Rc;
    use tempfile::{TempDir, tempdir};

    use super::*;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Phase {
        AfterPageRead,
        AfterPagesCaptured,
        AfterIndexRead,
        BeforeReadOnlyReturn,
        AfterTempCreated,
        BeforeFinalValidation,
        BeforeRename,
    }

    type Action = Box<dyn FnMut(&Path, Option<&OsStr>)>;

    #[derive(Default)]
    struct TestHooks {
        phase: Option<Phase>,
        action: Option<Action>,
        iteration_failure: Option<usize>,
        fixed_temporary: Option<OsString>,
        chmod_error: Option<Errno>,
        chmod_must_follow_write: bool,
        skip_chmod: bool,
        temp_descriptor_error: Option<Errno>,
        temp_named_error: Option<Errno>,
        write_error_after: Option<usize>,
        writes: usize,
        file_fsync_error: Option<Errno>,
        rename_error: Option<Errno>,
        directory_fsync_error: Option<Errno>,
        unlink_error: Option<Errno>,
    }

    impl TestHooks {
        fn at(phase: Phase, action: impl FnMut(&Path, Option<&OsStr>) + 'static) -> Self {
            Self {
                phase: Some(phase),
                action: Some(Box::new(action)),
                ..Self::default()
            }
        }

        fn fire(&mut self, phase: Phase, path: &Path, name: Option<&OsStr>) {
            if self.phase == Some(phase) {
                self.phase = None;
                if let Some(mut action) = self.action.take() {
                    action(path, name);
                }
            }
        }
    }

    impl DurableHooks for TestHooks {
        fn after_index_read(&mut self, corpus: &Path) {
            self.fire(Phase::AfterIndexRead, corpus, None);
        }

        fn after_page_read(&mut self, corpus: &Path, name: &OsStr) {
            self.fire(Phase::AfterPageRead, corpus, Some(name));
        }

        fn after_pages_captured(&mut self, corpus: &Path) {
            self.fire(Phase::AfterPagesCaptured, corpus, None);
        }

        fn before_read_only_return(&mut self, corpus: &Path) {
            self.fire(Phase::BeforeReadOnlyReturn, corpus, None);
        }

        fn after_temp_created(&mut self, corpus: &Path, name: &OsStr) {
            self.fire(Phase::AfterTempCreated, corpus, Some(name));
        }

        fn before_final_validation(&mut self, corpus: &Path) {
            self.fire(Phase::BeforeFinalValidation, corpus, None);
        }

        fn before_rename(&mut self, corpus: &Path, name: &OsStr) {
            self.fire(Phase::BeforeRename, corpus, Some(name));
        }

        fn fail_directory_iteration(&mut self, _path: &Path, processed: usize) -> bool {
            if self.iteration_failure == Some(processed) {
                self.iteration_failure = None;
                true
            } else {
                false
            }
        }

        fn temporary_name(&mut self, pid: u32, sequence: u64) -> OsString {
            self.fixed_temporary
                .clone()
                .unwrap_or_else(|| OsString::from(format!(".INDEX.md.{pid}.{sequence}.tmp")))
        }

        fn chmod(&mut self, fd: BorrowedFd<'_>, mode: Mode) -> Result<(), Errno> {
            if self.chmod_must_follow_write && self.writes == 0 {
                return Err(Errno::PROTO);
            }
            match self.chmod_error.take() {
                Some(error) => Err(error),
                None if self.skip_chmod => Ok(()),
                None => rfs::fchmod(fd, mode),
            }
        }

        fn write(&mut self, fd: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno> {
            self.writes += 1;
            if self.write_error_after == Some(self.writes) {
                return Err(Errno::IO);
            }
            if self.write_error_after.is_some() && self.writes == 1 {
                return rustix::io::write(fd, &bytes[..bytes.len().min(7)]);
            }
            rustix::io::write(fd, bytes)
        }

        fn file_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            match self.file_fsync_error.take() {
                Some(error) => Err(error),
                None => rfs::fsync(fd),
            }
        }

        fn rename(&mut self, directory: BorrowedFd<'_>, temporary: &OsStr) -> Result<(), Errno> {
            match self.rename_error.take() {
                Some(error) => Err(error),
                None => rfs::renameat(directory, temporary, directory, INDEX_NAME),
            }
        }

        fn directory_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            match self.directory_fsync_error.take() {
                Some(error) => Err(error),
                None => rfs::fsync(fd),
            }
        }

        fn unlink(&mut self, directory: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
            match self.unlink_error.take() {
                Some(error) => Err(error),
                None => rfs::unlinkat(directory, name, AtFlags::empty()),
            }
        }

        fn temporary_descriptor_state(
            &mut self,
            fd: BorrowedFd<'_>,
            path: &Path,
            operation: &'static str,
        ) -> Result<NodeState, DurableError> {
            match self.temp_descriptor_error.take() {
                Some(error) => Err(io_error(operation, path, error)),
                None => descriptor_state(fd, path, operation),
            }
        }

        fn temporary_named_state(
            &mut self,
            directory: BorrowedFd<'_>,
            name: &OsStr,
            path: &Path,
            operation: &'static str,
        ) -> Result<NodeState, DurableError> {
            match self.temp_named_error.take() {
                Some(error) => Err(io_error(operation, path, error)),
                None => named_state(directory, name, path, operation),
            }
        }
    }

    fn source(slug: &str, summary: &str) -> String {
        format!("---\nslug: {slug}\ntype: gotcha\nsummary: {summary}\n---\nbody\n")
    }

    fn fixture() -> TempDir {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join(PAGES_NAME)).unwrap();
        fs::write(tmp.path().join("pages/alpha.md"), source("alpha", "first")).unwrap();
        fs::write(
            tmp.path().join(INDEX_NAME),
            format!(
                "preamble\n{}\nold\n{}\n",
                crate::BEGIN_MARKER,
                crate::END_MARKER
            ),
        )
        .unwrap();
        tmp
    }

    fn isolated(corpus: &Path, mode: LockMode) -> LockGuard {
        match acquire_lock_with_timeout(corpus, mode, Duration::from_secs(1)).unwrap() {
            LockLease::Isolated(guard) => guard,
            LockLease::Unisolated(value) => panic!("expected isolation, got {value:?}"),
        }
    }

    fn run_with_hooks(
        corpus: &Path,
        options: &ReindexOptions,
        hooks: &mut TestHooks,
    ) -> Result<ReindexResult, DurableError> {
        let mode = if options.check_only {
            LockMode::Shared
        } else {
            LockMode::Exclusive
        };
        let guard = isolated(corpus, mode);
        reindex_from_fd(
            guard.corpus_fd(),
            guard.corpus_path(),
            options,
            hooks,
            || guard.revalidate_before_commit().map_err(DurableError::from),
        )
    }

    fn temp_names(corpus: &Path) -> Vec<OsString> {
        let mut names = fs::read_dir(corpus)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .filter(|name| {
                name.as_bytes().starts_with(b".INDEX.md.") && name.as_bytes().ends_with(b".tmp")
            })
            .collect::<Vec<_>>();
        names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        names
    }

    // A (device, inode) pair alone is not a safe proxy for "the temporary
    // file's original descriptor is still open": once the file has been
    // unlinked, its inode is free for the filesystem to recycle, and this
    // suite runs its tests concurrently on multiple threads within one
    // process. Some other, unrelated test can legitimately hold an open
    // descriptor whose freshly created file was handed that exact recycled
    // identity, which would otherwise read as a false-positive leak here.
    // Requiring the descriptor's resolved path to also live under this
    // test's own fixture directory rules that out: a genuinely leaked
    // descriptor for our temporary was always opened relative to
    // `fixture_root` and still resolves under it (even once unlinked), while
    // a same-identity descriptor recycled into another test's fixture never
    // does.
    fn descriptor_identity_is_open(identity: (u64, u64), fixture_root: &Path) -> bool {
        let root = fs::canonicalize(fixture_root).unwrap_or_else(|_| fixture_root.to_path_buf());
        let directory = if Path::new("/dev/fd").is_dir() {
            Path::new("/dev/fd")
        } else {
            Path::new("/proc/self/fd")
        };
        fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                fs::metadata(entry.path())
                    .is_ok_and(|metadata| (metadata.dev(), metadata.ino()) == identity)
            })
            .filter_map(|entry| descriptor_target_path(&entry))
            .any(|path| path.starts_with(&root))
    }

    // On Linux, /dev/fd (or /proc/self/fd directly) contains real symlinks
    // that name the path each descriptor was opened against, so a plain
    // readlink resolves it; for an unlinked file the kernel appends
    // " (deleted)" to the final component, which `Path::starts_with` still
    // matches correctly against the (unmodified) fixture root.
    #[cfg(target_os = "linux")]
    fn descriptor_target_path(entry: &fs::DirEntry) -> Option<PathBuf> {
        fs::read_link(entry.path()).ok()
    }

    // macOS's fdescfs does not expose /dev/fd entries as symlinks a plain
    // readlink can resolve. Opening one duplicates the underlying
    // descriptor (a safe, ordinary `open`), and asking the kernel for that
    // duplicate's resolved path via F_GETPATH reports the same path the
    // original descriptor refers to.
    #[cfg(not(target_os = "linux"))]
    fn descriptor_target_path(entry: &fs::DirEntry) -> Option<PathBuf> {
        let duplicate = fs::File::open(entry.path()).ok()?;
        let path = rustix::fs::getpath(&duplicate).ok()?;
        Some(PathBuf::from(OsString::from_vec(path.into_bytes())))
    }

    #[test]
    fn page_binding_is_rechecked_after_the_descriptor_read() {
        let tmp = fixture();
        let page = tmp.path().join("pages/alpha.md");
        let stale = tmp.path().join("pages/stale");
        let mut hooks = TestHooks::at(Phase::AfterPageRead, move |_, _| {
            fs::rename(&page, &stale).unwrap();
            fs::write(&page, source("alpha", "other")).unwrap();
        });
        let before = fs::read(tmp.path().join(INDEX_NAME)).unwrap();
        let error = run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
        assert!(matches!(error, DurableError::Raced { .. }), "{error}");
        assert_eq!(fs::read(tmp.path().join(INDEX_NAME)).unwrap(), before);
    }

    #[test]
    fn stale_expected_digest_precedes_page_enumeration_and_index_decoding() {
        let tmp = fixture();
        fs::create_dir(tmp.path().join("pages/hostile.md")).unwrap();
        fs::write(tmp.path().join(INDEX_NAME), b"\xff\xfe").unwrap();
        let mut hooks = TestHooks {
            iteration_failure: Some(1),
            ..TestHooks::default()
        };
        let error = run_with_hooks(
            tmp.path(),
            &ReindexOptions {
                expected_sha256: Some("0".repeat(64)),
                ..ReindexOptions::default()
            },
            &mut hooks,
        )
        .unwrap_err();
        assert!(
            matches!(error, DurableError::ExpectedIndexChanged),
            "{error}"
        );
    }

    #[test]
    fn unsafe_index_binding_precedes_the_expected_digest_comparison() {
        let tmp = fixture();
        fs::rename(tmp.path().join(INDEX_NAME), tmp.path().join("real-index")).unwrap();
        std::os::unix::fs::symlink("real-index", tmp.path().join(INDEX_NAME)).unwrap();
        let mut hooks = TestHooks::default();
        let error = run_with_hooks(
            tmp.path(),
            &ReindexOptions {
                expected_sha256: Some("0".repeat(64)),
                ..ReindexOptions::default()
            },
            &mut hooks,
        )
        .unwrap_err();
        assert!(matches!(error, DurableError::Unsafe { .. }), "{error}");
    }

    #[test]
    fn durable_page_parsing_never_collapses_non_utf8_names() {
        let state = NodeState::from_stat(&rfs::stat(".").unwrap());
        for byte in [0x80, 0x81] {
            let name = OsString::from_vec(vec![b'b', b'a', b'd', b'-', byte, b'.', b'm', b'd']);
            let error = parse_captured_pages(&[CapturedPage {
                signature: EntrySignature { name, state },
                bytes: source("bad", "summary").into_bytes(),
            }])
            .unwrap_err();
            assert!(
                error.to_string().contains(&format!("bad-\\x{byte:02x}.md")),
                "{error}"
            );
        }
    }

    #[test]
    fn mode_is_applied_after_writing_and_verified_before_replacement() {
        let tmp = fixture();
        let mut hooks = TestHooks {
            chmod_must_follow_write: true,
            ..TestHooks::default()
        };
        run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap();
        assert!(temp_names(tmp.path()).is_empty());

        let tmp = fixture();
        fs::set_permissions(
            tmp.path().join(INDEX_NAME),
            fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let before = fs::read(tmp.path().join(INDEX_NAME)).unwrap();
        let mut hooks = TestHooks {
            skip_chmod: true,
            ..TestHooks::default()
        };
        let error = run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
        assert!(matches!(error, DurableError::Unsafe { .. }), "{error}");
        assert_eq!(fs::read(tmp.path().join(INDEX_NAME)).unwrap(), before);
        assert!(temp_names(tmp.path()).is_empty());
    }

    #[test]
    fn irrelevant_non_markdown_churn_does_not_invalidate_the_page_snapshot() {
        let tmp = fixture();
        let root = tmp.path().to_path_buf();
        let mut hooks = TestHooks::at(Phase::AfterPagesCaptured, move |_, _| {
            fs::write(root.join("pages/irrelevant.txt"), b"noise").unwrap();
        });
        let result = run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap();
        assert!(result.changed);
    }

    #[test]
    fn page_set_and_pages_directory_binding_are_bracketed() {
        for operation in ["add", "delete", "replace-pages"] {
            let tmp = fixture();
            let root = tmp.path().to_path_buf();
            let action = operation.to_owned();
            let mut hooks = TestHooks::at(Phase::AfterPagesCaptured, move |_, _| {
                match action.as_str() {
                    "add" => fs::write(root.join("pages/beta.md"), source("beta", "new")).unwrap(),
                    "delete" => fs::remove_file(root.join("pages/alpha.md")).unwrap(),
                    "replace-pages" => {
                        fs::rename(root.join("pages"), root.join("old-pages")).unwrap();
                        fs::create_dir(root.join("pages")).unwrap();
                    }
                    _ => unreachable!(),
                }
            });
            let before = fs::read(tmp.path().join(INDEX_NAME)).unwrap();
            let error =
                run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
            assert!(
                matches!(error, DurableError::Raced { .. } | DurableError::Io { .. }),
                "{operation}: {error}"
            );
            assert_eq!(fs::read(tmp.path().join(INDEX_NAME)).unwrap(), before);
        }
    }

    #[test]
    fn unchanged_and_check_only_paths_revalidate_the_full_snapshot() {
        for check_only in [false, true] {
            let tmp = fixture();
            reindex_wiki(tmp.path(), &ReindexOptions::default()).unwrap();
            let root = tmp.path().to_path_buf();
            let mut hooks = TestHooks::at(Phase::BeforeReadOnlyReturn, move |_, _| {
                fs::write(root.join("pages/beta.md"), source("beta", "late")).unwrap();
            });
            let options = ReindexOptions {
                check_only,
                ..ReindexOptions::default()
            };
            let error = run_with_hooks(tmp.path(), &options, &mut hooks).unwrap_err();
            assert!(matches!(error, DurableError::Raced { .. }), "{error}");
        }
    }

    #[test]
    fn final_validation_catches_changes_before_both_commit_boundaries() {
        for phase in [Phase::BeforeFinalValidation, Phase::BeforeRename] {
            for operation in ["add", "delete", "replace"] {
                let tmp = fixture();
                let root = tmp.path().to_path_buf();
                let action = operation.to_owned();
                let mut hooks = TestHooks::at(phase, move |_, _| match action.as_str() {
                    "add" => fs::write(root.join("pages/beta.md"), source("beta", "late")).unwrap(),
                    "delete" => fs::remove_file(root.join("pages/alpha.md")).unwrap(),
                    "replace" => {
                        let page = root.join("pages/alpha.md");
                        fs::rename(&page, root.join("pages/old-alpha")).unwrap();
                        fs::write(page, source("alpha", "changed")).unwrap();
                    }
                    _ => unreachable!(),
                });
                let before = fs::read(tmp.path().join(INDEX_NAME)).unwrap();
                let error =
                    run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
                assert!(
                    matches!(error, DurableError::Raced { .. }),
                    "{phase:?}/{operation}: {error}"
                );
                assert_eq!(fs::read(tmp.path().join(INDEX_NAME)).unwrap(), before);
                assert!(temp_names(tmp.path()).is_empty());
            }
        }
    }

    #[test]
    fn ctime_catches_same_size_restored_mtime_and_transient_hardlink() {
        for operation in ["rewrite", "hardlink"] {
            let tmp = fixture();
            let page = tmp.path().join("pages/alpha.md");
            let action = operation.to_owned();
            let mut hooks = TestHooks::at(Phase::BeforeFinalValidation, move |root, _| {
                if action == "rewrite" {
                    let old = stat(&page).unwrap();
                    let bytes = fs::read(&page).unwrap();
                    let mut changed = bytes.clone();
                    let last = changed.len() - 2;
                    changed[last] = if changed[last] == b'x' { b'y' } else { b'x' };
                    fs::write(&page, changed).unwrap();
                    rfs::utimensat(
                        rfs::CWD,
                        &page,
                        &Timestamps {
                            last_access: Timespec {
                                tv_sec: old.st_atime as i64,
                                tv_nsec: old.st_atime_nsec as i64,
                            },
                            last_modification: Timespec {
                                tv_sec: old.st_mtime as i64,
                                tv_nsec: old.st_mtime_nsec as i64,
                            },
                        },
                        AtFlags::empty(),
                    )
                    .unwrap();
                } else {
                    let link = root.join("transient-link");
                    fs::hard_link(&page, &link).unwrap();
                    fs::remove_file(link).unwrap();
                }
            });
            let error =
                run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
            assert!(
                matches!(error, DurableError::Raced { .. }),
                "{operation}: {error}"
            );
        }
    }

    #[test]
    fn index_and_corpus_rebinding_never_commit_over_the_new_names() {
        for operation in ["index", "corpus"] {
            let tmp = fixture();
            let root = tmp.path().to_path_buf();
            let old_root = root.with_file_name(format!(
                "{}-detached",
                root.file_name().unwrap().to_string_lossy()
            ));
            let old_root_for_hook = old_root.clone();
            let action = operation.to_owned();
            let mut hooks = TestHooks::at(Phase::BeforeFinalValidation, move |_, _| {
                if action == "index" {
                    fs::rename(root.join(INDEX_NAME), root.join("old-index")).unwrap();
                    fs::write(root.join(INDEX_NAME), b"attacker").unwrap();
                } else {
                    fs::rename(&root, &old_root_for_hook).unwrap();
                    fs::create_dir(&root).unwrap();
                    fs::create_dir(root.join(PAGES_NAME)).unwrap();
                    fs::write(root.join(INDEX_NAME), b"new corpus").unwrap();
                }
            });
            let error =
                run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
            assert!(
                matches!(error, DurableError::Raced { .. } | DurableError::Lock(_)),
                "{operation}: {error}"
            );
            if operation == "index" {
                assert_eq!(fs::read(tmp.path().join(INDEX_NAME)).unwrap(), b"attacker");
            } else {
                assert_eq!(
                    fs::read(tmp.path().join(INDEX_NAME)).unwrap(),
                    b"new corpus"
                );
                fs::remove_dir_all(tmp.path()).unwrap();
                fs::rename(old_root, tmp.path()).unwrap();
            }
        }
    }

    #[test]
    fn partial_iteration_and_io_faults_preserve_the_original_and_clean_up() {
        let faults = [
            "iteration",
            "chmod",
            "partial-write",
            "file-fsync",
            "rename",
        ];
        for fault in faults {
            let tmp = fixture();
            let before = fs::read(tmp.path().join(INDEX_NAME)).unwrap();
            let mut hooks = TestHooks::default();
            match fault {
                "iteration" => hooks.iteration_failure = Some(1),
                "chmod" => hooks.chmod_error = Some(Errno::IO),
                "partial-write" => hooks.write_error_after = Some(2),
                "file-fsync" => hooks.file_fsync_error = Some(Errno::IO),
                "rename" => hooks.rename_error = Some(Errno::IO),
                _ => unreachable!(),
            }
            assert!(
                run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).is_err(),
                "{fault}"
            );
            assert_eq!(
                fs::read(tmp.path().join(INDEX_NAME)).unwrap(),
                before,
                "{fault}"
            );
            assert!(temp_names(tmp.path()).is_empty(), "{fault}");
        }
    }

    #[test]
    fn temp_collisions_are_never_unlinked_and_rebound_cleanup_is_refused() {
        let tmp = fixture();
        let collision = tmp.path().join(".INDEX.md.fixed.tmp");
        fs::write(&collision, b"preexisting").unwrap();
        let mut hooks = TestHooks {
            fixed_temporary: Some(OsString::from(".INDEX.md.fixed.tmp")),
            ..TestHooks::default()
        };
        assert!(run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).is_err());
        assert_eq!(fs::read(&collision).unwrap(), b"preexisting");

        let tmp = fixture();
        let root = tmp.path().to_path_buf();
        let mut hooks = TestHooks::at(Phase::AfterTempCreated, move |_, name| {
            let name = name.unwrap();
            fs::rename(root.join(name), root.join("created-temp-aside")).unwrap();
            fs::write(root.join(name), b"attacker object").unwrap();
        });
        let error = run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
        assert!(
            matches!(error, DurableError::TemporaryRebound { .. }),
            "{error}"
        );
        assert!(
            temp_names(tmp.path())
                .iter()
                .any(|name| fs::read(tmp.path().join(name)).unwrap() == b"attacker object")
        );
    }

    #[test]
    fn every_post_create_validation_failure_uses_identity_aware_cleanup() {
        for fault in ["descriptor-stat", "named-stat"] {
            let tmp = fixture();
            let before = fs::read(tmp.path().join(INDEX_NAME)).unwrap();
            let mut hooks = TestHooks::default();
            if fault == "descriptor-stat" {
                hooks.temp_descriptor_error = Some(Errno::IO);
            } else {
                hooks.temp_named_error = Some(Errno::IO);
            }
            let error = run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks)
                .expect_err(fault);
            assert_eq!(
                fs::read(tmp.path().join(INDEX_NAME)).unwrap(),
                before,
                "{fault}: {error}"
            );
            assert!(temp_names(tmp.path()).is_empty(), "{fault}: {error}");
        }

        let tmp = fixture();
        let before = fs::read(tmp.path().join(INDEX_NAME)).unwrap();
        let root = tmp.path().to_path_buf();
        let mut hooks = TestHooks::at(Phase::AfterTempCreated, move |_, name| {
            fs::hard_link(root.join(name.unwrap()), root.join("attacker-hardlink")).unwrap();
        });
        let error = run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
        assert!(matches!(error, DurableError::Unsafe { .. }), "{error}");
        assert_eq!(fs::read(tmp.path().join(INDEX_NAME)).unwrap(), before);
        assert!(temp_names(tmp.path()).is_empty(), "{error}");
    }

    #[test]
    fn cleanup_failure_is_reported_and_directory_fsync_failure_means_replaced() {
        let tmp = fixture();
        let before = fs::read(tmp.path().join(INDEX_NAME)).unwrap();
        let mut hooks = TestHooks {
            rename_error: Some(Errno::IO),
            unlink_error: Some(Errno::IO),
            ..TestHooks::default()
        };
        let error = run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
        assert!(
            matches!(error, DurableError::CleanupFailed { .. }),
            "{error}"
        );
        assert_eq!(fs::read(tmp.path().join(INDEX_NAME)).unwrap(), before);
        assert_eq!(temp_names(tmp.path()).len(), 1);

        let tmp = fixture();
        let before = fs::read(tmp.path().join(INDEX_NAME)).unwrap();
        let mut hooks = TestHooks {
            directory_fsync_error: Some(Errno::IO),
            ..TestHooks::default()
        };
        let error = run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).unwrap_err();
        assert!(
            matches!(error, DurableError::ReplacedNotDurable(_)),
            "{error}"
        );
        assert_ne!(fs::read(tmp.path().join(INDEX_NAME)).unwrap(), before);
        assert!(temp_names(tmp.path()).is_empty());
    }

    #[test]
    fn ordinary_page_and_index_hardlinks_are_accepted_and_revalidated() {
        let tmp = fixture();
        let page_link = tmp.path().join("page-link");
        let index_link = tmp.path().join("index-link");
        fs::hard_link(tmp.path().join("pages/alpha.md"), page_link).unwrap();
        fs::hard_link(tmp.path().join(INDEX_NAME), index_link).unwrap();
        let result = reindex_wiki(tmp.path(), &ReindexOptions::default()).unwrap();
        assert!(result.changed);
    }

    #[test]
    fn locked_inner_operation_uses_the_callers_guard_without_reacquiring() {
        let tmp = fixture();
        let guard = isolated(tmp.path(), LockMode::Exclusive);
        let result = reindex_locked(&guard, &ReindexOptions::default()).unwrap();
        assert!(result.changed);
        assert_eq!(guard.mode(), LockMode::Exclusive);
    }

    #[test]
    fn repeated_injected_failures_leave_no_temporary_or_descriptor_residue() {
        for _ in 0..12 {
            let tmp = fixture();
            let identity = Rc::new(RefCell::new(None));
            let captured = Rc::clone(&identity);
            let mut hooks = TestHooks::at(Phase::AfterTempCreated, move |corpus, name| {
                let metadata = fs::metadata(corpus.join(name.unwrap())).unwrap();
                *captured.borrow_mut() = Some((metadata.dev(), metadata.ino()));
            });
            hooks.write_error_after = Some(2);
            assert!(run_with_hooks(tmp.path(), &ReindexOptions::default(), &mut hooks).is_err());
            assert!(temp_names(tmp.path()).is_empty());
            let identity = identity.borrow().expect("temporary identity captured");
            assert!(!descriptor_identity_is_open(identity, tmp.path()));
        }
    }
}
