use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::Regex;
use rustix::fs::{self as rfs, Access, AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::index::{generated_region, rebuild_index};
use crate::lock::{
    LOCK_TIMEOUT, LockError, LockLease, LockMode, UnisolatedReason, acquire_lock_with_timeout,
};
use crate::schema::{is_iso_date, is_python_whitespace, scalar_problem};

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
const INDEX_NAME: &str = "INDEX.md";
const PAGES_NAME: &str = "pages";
const SIZE_NOTE: usize = 12_288;

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapturedPageOutcome {
    Present(Vec<u8>),
    NotRegular,
    Unreadable(String),
    Symlink,
    OutsideCorpus,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CapturedPage {
    /// Exact directory-entry spelling. Non-UTF-8 names remain distinguishable.
    pub name: OsString,
    pub outcome: CapturedPageOutcome,
}

/// All filesystem-dependent input to structural validation, captured once.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WikiSnapshot {
    pub corpus: PathBuf,
    pub index: Vec<u8>,
    pub pages: Vec<CapturedPage>,
    pub isolated: bool,
    pub isolation_note: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WikiCheckReport {
    pub failures: Vec<String>,
    pub notes: Vec<String>,
    pub page_count: usize,
    pub isolated: bool,
}

#[derive(Debug, Error)]
pub enum CheckError {
    #[error(transparent)]
    Lock(#[from] LockError),

    #[error("unsafe wiki object at {path}: {detail}")]
    Unsafe { path: PathBuf, detail: String },

    #[error("wiki changed while capturing {path}: {detail}")]
    Raced { path: PathBuf, detail: String },

    #[error("could not {operation} {path}: {detail}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        detail: String,
    },
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

#[derive(Clone, Debug, Eq, PartialEq)]
struct EntrySignature {
    name: OsString,
    state: NodeState,
}

pub fn capture_wiki(corpus: &Path) -> Result<WikiSnapshot, CheckError> {
    let mut hooks = SystemCaptureHooks;
    capture_wiki_with_timeout(corpus, LOCK_TIMEOUT, &mut hooks)
}

fn capture_wiki_with_timeout(
    corpus: &Path,
    timeout: Duration,
    hooks: &mut impl CaptureHooks,
) -> Result<WikiSnapshot, CheckError> {
    match acquire_lock_with_timeout(corpus, LockMode::Shared, timeout)? {
        LockLease::Isolated(guard) => capture_from_fd(
            guard.corpus_fd(),
            guard.corpus_path(),
            true,
            None,
            hooks,
            || guard.revalidate_before_commit().map_err(CheckError::from),
        ),
        LockLease::Unisolated(unisolated) => {
            let note = isolation_note(unisolated.reason);
            capture_from_fd(
                unisolated.corpus_fd(),
                &unisolated.corpus,
                false,
                Some(note),
                hooks,
                || unisolated.revalidate().map_err(CheckError::from),
            )
        }
    }
}

trait CaptureHooks {
    fn after_pages_stream_opened(&mut self, _path: &Path) {}
    fn fail_directory_iteration(&mut self, _path: &Path, _processed: usize) -> bool {
        false
    }
    fn before_page_open(&mut self, _path: &Path) {}
    fn fail_page_read(&mut self, _path: &Path) -> bool {
        false
    }
    fn after_page_read(&mut self, _path: &Path) {}
    fn after_pages_captured(&mut self, _path: &Path) {}
    fn after_index_read(&mut self, _path: &Path) {}
    fn before_final_validation(&mut self, _path: &Path) {}
}

struct SystemCaptureHooks;

impl CaptureHooks for SystemCaptureHooks {}

/// Check a wiki through one locked snapshot.
///
/// Preflight, lock-contention, unsafe-lock, and known capture failures are
/// structural report failures for CLI compatibility. [`capture_wiki`] remains
/// available to callers that need the operational error distinction.
pub fn check_wiki(corpus: &Path) -> Result<WikiCheckReport, CheckError> {
    let mut preflight = SystemPreflight;
    let mut capture = SystemCaptureHooks;
    Ok(check_wiki_with(
        corpus,
        LOCK_TIMEOUT,
        &mut preflight,
        &mut capture,
    ))
}

trait Preflight {
    fn metadata(&mut self, path: &Path) -> std::io::Result<std::fs::Metadata> {
        std::fs::metadata(path)
    }

    fn accessible(&mut self, path: &Path, access: Access) -> bool {
        rfs::accessat(rfs::CWD, path, access, AtFlags::EACCESS).is_ok()
    }
}

struct SystemPreflight;

impl Preflight for SystemPreflight {}

fn check_wiki_with(
    corpus: &Path,
    timeout: Duration,
    preflight: &mut impl Preflight,
    capture_hooks: &mut impl CaptureHooks,
) -> WikiCheckReport {
    if let Some(failure) = preflight_failure(corpus, preflight) {
        return failed_report(failure);
    }
    let snapshot = match capture_wiki_with_timeout(corpus, timeout, capture_hooks) {
        Ok(snapshot) => snapshot,
        Err(error @ CheckError::Lock(LockError::Busy { .. })) => {
            return failed_report(format!("could not obtain a consistent snapshot: {error}"));
        }
        Err(error @ CheckError::Lock(LockError::Unsafe { .. })) => {
            return failed_report(error.to_string());
        }
        Err(CheckError::Io { ref path, .. })
            if path.file_name() == Some(OsStr::new(INDEX_NAME)) =>
        {
            return failed_report(format!("{} is not readable", path.display()));
        }
        Err(error) => {
            return failed_report(format!("could not obtain a consistent snapshot: {error}"));
        }
    };
    validate_wiki(&snapshot)
}

fn preflight_failure(corpus: &Path, preflight: &mut impl Preflight) -> Option<String> {
    let pages = corpus.join(PAGES_NAME);
    match preflight.metadata(&pages) {
        Ok(metadata) if metadata.is_dir() => {}
        Ok(_) => {
            return Some(format!("{} does not exist", pages.display()));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(format!("{} does not exist", pages.display()));
        }
        Err(_) => return Some(format!("{} is not readable", pages.display())),
    }
    if !preflight.accessible(&pages, Access::READ_OK | Access::EXEC_OK) {
        return Some(format!("{} is not readable", pages.display()));
    }
    let index = corpus.join(INDEX_NAME);
    match preflight.metadata(&index) {
        Ok(metadata) if metadata.is_file() => {}
        Ok(_) => {
            return Some(format!("{} does not exist", index.display()));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Some(format!("{} does not exist", index.display()));
        }
        Err(_) => return Some(format!("{} is not readable", index.display())),
    }
    if !preflight.accessible(&index, Access::READ_OK) {
        return Some(format!("{} is not readable", index.display()));
    }
    None
}

fn failed_report(failure: String) -> WikiCheckReport {
    WikiCheckReport {
        failures: vec![failure],
        notes: Vec::new(),
        page_count: 0,
        isolated: false,
    }
}

/// Extracts the target slug of every in-profile wikilink in `source`.
/// `[[slug|alias]]` and `[[slug#heading]]` resolve to `slug`. Embeds
/// (`![[...]]`) and block references (`[[slug#^id]]`) are not links in the
/// Yams graph and yield no target.
fn wikilink_targets(source: &str) -> Vec<String> {
    let wikilink =
        Regex::new(r"\[\[([a-z0-9-]+)(?:#([^\]|]*))?(?:\|[^\]]*)?\]\]").expect("constant regex");
    let mut targets = Vec::new();
    for captures in wikilink.captures_iter(source) {
        let whole = captures.get(0).expect("capture");
        if whole.start() > 0 && source.as_bytes()[whole.start() - 1] == b'!' {
            continue; // embed, not a link
        }
        if captures
            .get(2)
            .is_some_and(|fragment| fragment.as_str().starts_with('^'))
        {
            continue; // block reference, outside the plain-link graph
        }
        targets.push(captures.get(1).expect("capture").as_str().to_owned());
    }
    targets
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct WikiCompatReport {
    pub violations: Vec<String>,
    pub page_count: usize,
}

/// Reports constructs outside the supported Obsidian-compatible profile
/// without modifying anything. Within-profile constructs (callouts,
/// highlights, inline tags, `%%` comments, alias/heading links) are not
/// flagged.
pub fn compat_wiki(path: &Path) -> Result<WikiCompatReport, CheckError> {
    let snapshot = capture_wiki(path)?;
    Ok(compat_snapshot(&snapshot))
}

pub(crate) fn compat_snapshot(snapshot: &WikiSnapshot) -> WikiCompatReport {
    let embed = Regex::new(r"!\[\[[^\]]+\]\]").expect("constant regex");
    let block_ref =
        Regex::new(r"\[\[[a-z0-9-]+#\^[^\]|]*(?:\|[^\]]*)?\]\]").expect("constant regex");
    let block_id = Regex::new(r"(?m)\s\^[a-zA-Z0-9-]+\s*$").expect("constant regex");
    let any_link = Regex::new(r"\[\[([^\]]+)\]\]").expect("constant regex");
    let slug_shape = Regex::new(r"^[a-z0-9-]+$").expect("constant regex");

    let mut violations = Vec::new();
    let mut page_count = 0;
    for page in &snapshot.pages {
        let Some(name) = page.name.to_str() else {
            continue;
        };
        let CapturedPageOutcome::Present(bytes) = &page.outcome else {
            continue;
        };
        page_count += 1;
        let Ok(source) = std::str::from_utf8(bytes) else {
            violations.push(format!("pages/{name} is not valid UTF-8"));
            continue;
        };
        if let Err(error) = crate::schema::parse_wiki_page(source) {
            violations.push(format!("pages/{name}: {error}"));
        }
        for found in embed.find_iter(source) {
            violations.push(format!(
                "pages/{} uses embed {} — embeds are outside the supported Obsidian profile",
                name,
                found.as_str()
            ));
        }
        for found in block_ref.find_iter(source) {
            violations.push(format!(
                "pages/{} uses block reference {} — block references are outside the supported Obsidian profile",
                name,
                found.as_str()
            ));
        }
        for found in block_id.find_iter(source) {
            violations.push(format!(
                "pages/{} uses block ID {} — block IDs are outside the supported Obsidian profile",
                name,
                found.as_str().trim()
            ));
        }
        for captures in any_link.captures_iter(source) {
            let inner = captures.get(1).expect("capture").as_str();
            let target = inner.split(['|', '#']).next().unwrap_or(inner);
            if !slug_shape.is_match(target) {
                violations.push(format!(
                    "pages/{name} links [[{inner}]] — target is not a Yams slug"
                ));
            }
        }
    }
    deduplicate(&mut violations);
    WikiCompatReport {
        violations,
        page_count,
    }
}

/// Validate only captured values. This function has no filesystem path input
/// and performs no I/O, so the verdict cannot mix states from two snapshots.
pub fn validate_wiki(snapshot: &WikiSnapshot) -> WikiCheckReport {
    let mut failures = Vec::new();
    let mut notes = Vec::new();
    if let Some(note) = &snapshot.isolation_note {
        notes.push(format!(
            "this check ran without isolation ({note}); a concurrent write could not have been excluded"
        ));
    }
    let page_count = snapshot
        .pages
        .iter()
        .filter(|page| !matches!(page.outcome, CapturedPageOutcome::NotRegular))
        .count();
    let Some(index) = std::str::from_utf8(&snapshot.index).ok() else {
        failures.push(format!(
            "{} is not valid UTF-8",
            snapshot.corpus.join(INDEX_NAME).display()
        ));
        return WikiCheckReport {
            failures,
            notes,
            page_count,
            isolated: snapshot.isolated,
        };
    };

    for page in &snapshot.pages {
        let name = diagnostic_name(&page.name);
        if page.name.to_str().is_none() {
            failures.push(format!("pages/{name} filename is not valid UTF-8"));
        } else if matches!(page.outcome, CapturedPageOutcome::NotRegular) {
            failures.push(format!("pages/{name} is not a regular file"));
        }
    }
    let page_files = snapshot
        .pages
        .iter()
        .filter(|page| !matches!(page.outcome, CapturedPageOutcome::NotRegular))
        .filter_map(|page| page.name.to_str())
        .collect::<BTreeSet<_>>();
    let slugs = page_files
        .iter()
        .filter_map(|name| name.strip_suffix(".md"))
        .collect::<BTreeSet<_>>();

    for name in &page_files {
        if !index.contains(&format!("(pages/{name})")) {
            failures.push(format!(
                "pages/{name} is not listed in INDEX.md (orphan page)"
            ));
        }
    }
    let index_link = Regex::new(r"\(pages/([^)]+\.md)\)").expect("constant regex");
    for captures in index_link.captures_iter(index) {
        let name = captures.get(1).expect("capture").as_str();
        if !page_files.contains(name) {
            failures.push(format!("INDEX.md links pages/{name}, which does not exist"));
        }
    }

    let mut linked_from_elsewhere = HashSet::new();
    let mut valid_pages = Vec::new();
    let line_ref = Regex::new(r"[A-Za-z0-9_./-]+\.py:\d+").expect("constant regex");
    for page in &snapshot.pages {
        let Some(name) = page.name.to_str() else {
            continue;
        };
        let Some(slug) = name.strip_suffix(".md") else {
            continue;
        };
        let bytes = match &page.outcome {
            CapturedPageOutcome::NotRegular => continue,
            CapturedPageOutcome::Symlink => {
                failures.push(format!("pages/{name} is a symlink — refused"));
                continue;
            }
            CapturedPageOutcome::OutsideCorpus => {
                failures.push(format!("pages/{name} resolves outside the wiki"));
                continue;
            }
            CapturedPageOutcome::Unreadable(_) => {
                failures.push(format!("pages/{name} is not readable"));
                continue;
            }
            CapturedPageOutcome::Present(bytes) => bytes,
        };
        let Ok(source) = std::str::from_utf8(bytes) else {
            failures.push(format!("pages/{name} is not valid UTF-8"));
            continue;
        };
        if bytes.len() > SIZE_NOTE {
            notes.push(format!(
                "pages/{} is {} bytes; an oversized page becomes many competing weak matches that lower the rank of every real answer",
                name,
                bytes.len()
            ));
        }
        let parsed = yams_core::parse_frontmatter(source);
        if parsed.fields.is_empty() {
            failures.push(format!("pages/{} has no parseable frontmatter block", name));
            continue;
        }
        let fields = &parsed.fields;
        if fields.get("slug").map(String::as_str) != Some(slug) {
            let declared = fields.get("slug").map_or("(missing)", String::as_str);
            failures.push(format!(
                "pages/{} declares slug \"{declared}\" — must match the filename",
                name
            ));
        }
        if fields.get("title").is_none_or(String::is_empty) {
            failures.push(format!("pages/{name} is missing a title"));
        }
        match fields.get("summary") {
            None => failures.push(format!("pages/{name} is missing summary")),
            Some(summary) if summary.is_empty() => {
                failures.push(format!("pages/{name} is missing summary"));
            }
            Some(summary) => {
                if let Some(problem) = scalar_problem(summary) {
                    failures.push(format!("pages/{name}: {problem}"));
                }
            }
        }
        validate_enum_field(
            &mut failures,
            name,
            fields,
            "status",
            &["current", "historical", "in-progress"],
        );
        validate_enum_field(
            &mut failures,
            name,
            fields,
            "type",
            &[
                "decision",
                "feature",
                "gotcha",
                "pattern",
                "project-state",
                "workflow",
            ],
        );
        validate_enum_field(
            &mut failures,
            name,
            fields,
            "owner",
            &["claude", "codex", "shared"],
        );
        for field in ["updated", "verified"] {
            match fields.get(field) {
                None => failures.push(format!("pages/{name} is missing {field}")),
                Some(value) if value.is_empty() => {
                    failures.push(format!("pages/{name} is missing {field}"));
                }
                Some(value) if !is_iso_date(value) => failures.push(format!(
                    "pages/{} has {field}: {value} — expected YYYY-MM-DD",
                    name
                )),
                Some(_) => {}
            }
        }
        if let (Some(updated), Some(verified)) = (fields.get("updated"), fields.get("verified"))
            && is_iso_date(updated)
            && is_iso_date(verified)
            && verified < updated
        {
            failures.push(format!(
                "pages/{} has verified: {verified} before updated: {updated} — editing a page verifies it",
                name
            ));
        }

        let without_exemptions = strip_line_ref_exemptions(source);
        for found in line_ref.find_iter(&without_exemptions) {
            failures.push(format!(
                "pages/{} cites {} — line numbers drift; name the symbol instead",
                name,
                found.as_str()
            ));
        }
        for linked in wikilink_targets(source) {
            if !slugs.contains(linked.as_str()) {
                notes.push(format!(
                    "pages/{} links [[{linked}]], not yet written",
                    name
                ));
            } else if linked != slug {
                linked_from_elsewhere.insert(linked);
            }
        }
        valid_pages.push((name.to_owned(), fields.clone()));
    }

    if page_files.len() > 1 {
        for name in &page_files {
            if let Some(slug) = name.strip_suffix(".md")
                && !linked_from_elsewhere.contains(slug)
            {
                notes.push(format!(
                    "pages/{name}: no other page links [[{slug}]] — unreachable except through INDEX.md"
                ));
            }
        }
    }

    if failures.is_empty() {
        validate_generated_index(index, &valid_pages, &mut failures);
    }
    deduplicate(&mut failures);
    deduplicate(&mut notes);
    WikiCheckReport {
        failures,
        notes,
        page_count,
        isolated: snapshot.isolated,
    }
}

fn capture_from_fd(
    corpus_fd: BorrowedFd<'_>,
    corpus_path: &Path,
    isolated: bool,
    isolation_note: Option<String>,
    hooks: &mut impl CaptureHooks,
    mut revalidate_lease: impl FnMut() -> Result<(), CheckError>,
) -> Result<WikiSnapshot, CheckError> {
    let pages_path = corpus_path.join(PAGES_NAME);
    let pages_candidate = named_state(corpus_fd, PAGES_NAME, &pages_path, "inspect")?;
    if !pages_candidate.kind.is_dir() {
        return Err(unsafe_error(
            &pages_path,
            "expected a non-symlink directory",
        ));
    }
    let pages_fd = rfs::openat(corpus_fd, PAGES_NAME, DIRECTORY_FLAGS, Mode::empty())
        .map_err(|error| io_error("open without following links", &pages_path, error))?;
    let pages_state = descriptor_state(&pages_fd, &pages_path, "inspect opened")?;
    if (pages_state.device, pages_state.inode) != (pages_candidate.device, pages_candidate.inode)
        || !pages_state.kind.is_dir()
    {
        return Err(raced(&pages_path, "directory changed while opening"));
    }
    hooks.after_pages_stream_opened(&pages_path);
    let signatures = enumerate(pages_fd.as_fd(), &pages_path, hooks)?;
    let mut pages = Vec::new();
    for signature in &signatures {
        if !signature.name.as_bytes().ends_with(b".md") {
            continue;
        }
        let path = pages_path.join(&signature.name);
        let outcome = if signature.state.kind.is_symlink() {
            match rfs::statat(pages_fd.as_fd(), &signature.name, AtFlags::empty()) {
                Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_file() => {
                    CapturedPageOutcome::Symlink
                }
                _ => CapturedPageOutcome::NotRegular,
            }
        } else if !signature.state.kind.is_file() {
            CapturedPageOutcome::NotRegular
        } else {
            capture_page(
                pages_fd.as_fd(),
                &signature.name,
                &path,
                signature.state,
                hooks,
            )?
        };
        pages.push(CapturedPage {
            name: signature.name.clone(),
            outcome,
        });
    }
    verify_directory_binding(
        corpus_fd,
        OsStr::new(PAGES_NAME),
        pages_fd.as_fd(),
        pages_state,
        &pages_path,
    )?;
    hooks.after_pages_captured(&pages_path);
    if enumerate(pages_fd.as_fd(), &pages_path, hooks)? != signatures {
        return Err(raced(&pages_path, "entry or signature set changed"));
    }

    let index_path = corpus_path.join(INDEX_NAME);
    let (index, index_state, index_digest) = capture_index(corpus_fd, &index_path)?;
    hooks.after_index_read(&index_path);
    hooks.before_final_validation(corpus_path);
    verify_directory_binding(
        corpus_fd,
        OsStr::new(PAGES_NAME),
        pages_fd.as_fd(),
        pages_state,
        &pages_path,
    )?;
    if enumerate(pages_fd.as_fd(), &pages_path, hooks)? != signatures {
        return Err(raced(&pages_path, "entry or signature set changed"));
    }
    let (final_index, final_state, final_digest) = capture_index(corpus_fd, &index_path)?;
    if final_state != index_state || final_digest != index_digest || final_index != index {
        return Err(raced(&index_path, "identity, metadata, or bytes changed"));
    }
    revalidate_lease()?;
    Ok(WikiSnapshot {
        corpus: corpus_path.to_path_buf(),
        index,
        pages,
        isolated,
        isolation_note,
    })
}

fn capture_page(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    path: &Path,
    candidate: NodeState,
    hooks: &mut impl CaptureHooks,
) -> Result<CapturedPageOutcome, CheckError> {
    filesystem_access();
    hooks.before_page_open(path);
    let fd = match rfs::openat(parent, name, FILE_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(error) => {
            let now = named_state(parent, name, path, "reinspect unreadable")?;
            if now != candidate {
                return Err(raced(path, "page changed while opening"));
            }
            return Ok(CapturedPageOutcome::Unreadable(error.to_string()));
        }
    };
    let opened = descriptor_state(&fd, path, "inspect opened page")?;
    if opened != candidate || !opened.kind.is_file() {
        return Err(raced(path, "page changed while opening"));
    }
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    let read = if hooks.fail_page_read(path) {
        Err(std::io::Error::other("injected page read failure"))
    } else {
        file.read_to_end(&mut bytes).map(|_| ())
    };
    if let Err(error) = read {
        let after = descriptor_state(&file, path, "reinspect unreadable page")?;
        let named = named_state(parent, name, path, "reinspect unreadable page name")?;
        if after != candidate || named != candidate {
            return Err(raced(path, "unreadable page changed"));
        }
        return Ok(CapturedPageOutcome::Unreadable(error.to_string()));
    }
    hooks.after_page_read(path);
    let after = descriptor_state(&file, path, "reinspect read page")?;
    let named = named_state(parent, name, path, "reinspect read page name")?;
    if after != candidate || named != candidate || after.size != bytes.len() as u64 {
        return Err(raced(path, "page changed while reading"));
    }
    Ok(CapturedPageOutcome::Present(bytes))
}

fn capture_index(
    parent: BorrowedFd<'_>,
    path: &Path,
) -> Result<(Vec<u8>, NodeState, String), CheckError> {
    filesystem_access();
    let candidate = named_state(parent, INDEX_NAME, path, "inspect")?;
    if !candidate.kind.is_file() {
        return Err(unsafe_error(path, "expected a non-symlink regular file"));
    }
    let fd = rfs::openat(parent, INDEX_NAME, FILE_FLAGS, Mode::empty())
        .map_err(|error| io_error("open without following links", path, error))?;
    let opened = descriptor_state(&fd, path, "inspect opened")?;
    if opened != candidate {
        return Err(raced(path, "index changed while opening"));
    }
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)
        .map_err(|error| CheckError::Io {
            operation: "read",
            path: path.to_path_buf(),
            detail: error.to_string(),
        })?;
    let after = descriptor_state(&file, path, "reinspect")?;
    let named = named_state(parent, INDEX_NAME, path, "reinspect name")?;
    if after != candidate || named != candidate || after.size != bytes.len() as u64 {
        return Err(raced(path, "index changed while reading"));
    }
    let digest = format!("{:x}", Sha256::digest(&bytes));
    Ok((bytes, after, digest))
}

fn enumerate(
    directory: BorrowedFd<'_>,
    path: &Path,
    hooks: &mut impl CaptureHooks,
) -> Result<Vec<EntrySignature>, CheckError> {
    filesystem_access();
    let mut stream = Dir::read_from(directory)
        .map_err(|error| io_error("open directory stream", path, error))?;
    let mut names = Vec::new();
    for entry in &mut stream {
        let entry = entry.map_err(|error| io_error("read directory entry", path, error))?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." && bytes.ends_with(b".md") {
            names.push(OsString::from_vec(bytes.to_vec()));
            if hooks.fail_directory_iteration(path, names.len()) {
                return Err(CheckError::Io {
                    operation: "read directory entry",
                    path: path.to_path_buf(),
                    detail: "injected partial directory iteration failure".to_owned(),
                });
            }
        }
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    let mut signatures = Vec::with_capacity(names.len());
    for name in names {
        let entry_path = path.join(&name);
        let state = named_state(directory, &name, &entry_path, "inspect entry")?;
        signatures.push(EntrySignature { name, state });
    }
    Ok(signatures)
}

fn verify_directory_binding(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    fd: BorrowedFd<'_>,
    expected: NodeState,
    path: &Path,
) -> Result<(), CheckError> {
    let descriptor = descriptor_state(fd, path, "reinspect descriptor")?;
    let named = named_state(parent, name, path, "reinspect name")?;
    let expected_identity = (expected.device, expected.inode);
    if (descriptor.device, descriptor.inode) != expected_identity
        || (named.device, named.inode) != expected_identity
        || !descriptor.kind.is_dir()
        || !named.kind.is_dir()
    {
        return Err(raced(path, "directory binding changed"));
    }
    Ok(())
}

fn validate_enum_field(
    failures: &mut Vec<String>,
    name: &str,
    fields: &BTreeMap<String, String>,
    field: &str,
    allowed: &[&str],
) {
    match fields.get(field) {
        None => failures.push(format!("pages/{name} is missing {field}")),
        Some(value) if value.is_empty() => {
            failures.push(format!("pages/{name} is missing {field}"));
        }
        Some(value) if !allowed.contains(&value.as_str()) => failures.push(format!(
            "pages/{name} has {field}: {value} — expected one of {}",
            allowed.join(" | ")
        )),
        Some(_) => {}
    }
}

fn validate_generated_index(
    index: &str,
    pages: &[(String, BTreeMap<String, String>)],
    failures: &mut Vec<String>,
) {
    let render_pages = pages
        .iter()
        .filter_map(|(name, fields)| {
            Some(crate::IndexPage {
                slug: name.strip_suffix(".md")?.to_owned(),
                page_type: page_type(fields.get("type")?)?,
                summary: fields.get("summary")?.clone(),
            })
        })
        .collect::<Vec<_>>();
    match rebuild_index(index, &render_pages) {
        Ok(canonical) if canonical != index => failures.push(
            "INDEX.md differs from what catalog would produce — run `yams-wiki catalog .agents/memory`"
                .to_owned(),
        ),
        Err(error) => failures.push(format!("INDEX.md cannot be verified: {error}")),
        Ok(_) => {}
    }

    let region = match generated_region(index) {
        Ok(region) => region,
        Err(error) => {
            failures.push(format!("INDEX.md cannot be verified: {error}"));
            return;
        }
    };
    let expected = pages
        .iter()
        .filter_map(|(name, fields)| {
            Some((
                name.strip_suffix(".md")?.to_owned(),
                (fields.get("type")?.clone(), fields.get("summary")?.clone()),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let mut seen = HashSet::new();
    let mut seen_headings = HashSet::new();
    let expected_headings = expected
        .values()
        .filter_map(|(page_type, _)| literal_heading(page_type))
        .collect::<HashSet<_>>();
    let mut section: Option<&str> = None;
    let mut last_heading_rank = None;
    let mut previous_slug: Option<&str> = None;
    for line in logical_lines(region) {
        if line.is_empty() {
            continue;
        }
        if let Some(heading) = line.strip_prefix("## ") {
            section = Some(heading);
            previous_slug = None;
            let headings = [
                "Gotchas",
                "Patterns",
                "Decisions",
                "Workflow",
                "Project state",
                "Features — architecture pointers",
            ];
            let Some(rank) = headings.iter().position(|candidate| candidate == &heading) else {
                failures.push(format!(
                    "INDEX.md has unknown generated heading {heading:?}"
                ));
                continue;
            };
            if !seen_headings.insert(heading) {
                failures.push(format!("INDEX.md repeats generated heading {heading:?}"));
            }
            if last_heading_rank.is_some_and(|last| rank <= last) {
                failures.push(format!(
                    "INDEX.md generated headings are not in canonical order at {heading:?}"
                ));
            }
            last_heading_rank = Some(rank);
            continue;
        }
        let Some((label, slug, shown)) = parse_generated_entry(line) else {
            failures.push(format!(
                "INDEX.md has non-canonical generated line {line:?}"
            ));
            continue;
        };
        if previous_slug.is_some_and(|previous| slug <= previous) {
            failures.push(format!(
                "INDEX.md entries are not in canonical order at {slug}"
            ));
        }
        previous_slug = Some(slug);
        if !seen.insert(slug.to_owned()) {
            failures.push(format!("INDEX.md lists {slug} more than once"));
        }
        if label != slug {
            failures.push(format!("INDEX.md labels {slug} as {label:?}"));
        }
        if let Some((page_type, summary)) = expected.get(slug) {
            if shown != summary {
                failures.push(format!(
                    "INDEX.md shows a summary for {slug} that its page does not carry"
                ));
            }
            let expected_heading = literal_heading(page_type);
            if section != expected_heading {
                failures.push(format!(
                    "INDEX.md places {slug} under {:?}, expected {:?}",
                    section, expected_heading
                ));
            }
        }
    }
    let expected_slugs = expected.keys().cloned().collect::<HashSet<_>>();
    if seen != expected_slugs {
        let mut difference = seen
            .symmetric_difference(&expected_slugs)
            .cloned()
            .collect::<Vec<_>>();
        difference.sort();
        failures.push(format!(
            "INDEX.md entry set differs from the pages: {difference:?}"
        ));
    }
    if seen_headings != expected_headings {
        let mut difference = seen_headings
            .symmetric_difference(&expected_headings)
            .copied()
            .collect::<Vec<_>>();
        difference.sort_unstable();
        failures.push(format!(
            "INDEX.md generated heading set differs from the page types: {difference:?}"
        ));
    }
}

fn parse_generated_entry(line: &str) -> Option<(&str, &str, &str)> {
    let rest = line.strip_prefix("- [")?;
    let (label, rest) = rest.split_once("](")?;
    let (destination, shown) = rest.split_once(") — ")?;
    let slug = destination.strip_prefix("pages/")?.strip_suffix(".md")?;
    (!label.is_empty() && !slug.is_empty() && !shown.is_empty()).then_some((label, slug, shown))
}

fn literal_heading(page_type: &str) -> Option<&'static str> {
    match page_type {
        "gotcha" => Some("Gotchas"),
        "pattern" => Some("Patterns"),
        "decision" => Some("Decisions"),
        "workflow" => Some("Workflow"),
        "project-state" => Some("Project state"),
        "feature" => Some("Features — architecture pointers"),
        _ => None,
    }
}

fn page_type(value: &str) -> Option<crate::PageType> {
    match value {
        "gotcha" => Some(crate::PageType::Gotcha),
        "pattern" => Some(crate::PageType::Pattern),
        "decision" => Some(crate::PageType::Decision),
        "workflow" => Some(crate::PageType::Workflow),
        "project-state" => Some(crate::PageType::ProjectState),
        "feature" => Some(crate::PageType::Feature),
        _ => None,
    }
}

fn logical_lines(source: &str) -> Vec<&str> {
    source
        .split([
            '\n', '\r', '\u{000b}', '\u{000c}', '\u{001c}', '\u{001d}', '\u{001e}', '\u{0085}',
            '\u{2028}', '\u{2029}',
        ])
        .collect()
}

fn strip_line_ref_exemptions(source: &str) -> String {
    let fence = Regex::new(r"(?s)```.*?```|~~~.*?~~~").expect("constant regex");
    let without_fences = fence.replace_all(source, "");
    let mut output = String::new();
    let mut cursor = 0;
    while cursor < without_fences.len() {
        let rest = &without_fences[cursor..];
        if rest.starts_with("http://") || rest.starts_with("https://") {
            let mut end = cursor;
            for (offset, ch) in rest.char_indices() {
                if is_python_whitespace(ch) {
                    break;
                }
                end = cursor + offset + ch.len_utf8();
            }
            cursor = end;
            continue;
        }
        let ch = rest.chars().next().expect("cursor in source");
        output.push(ch);
        cursor += ch.len_utf8();
    }
    output
}

fn deduplicate(items: &mut Vec<String>) {
    let mut seen = HashSet::new();
    items.retain(|item| seen.insert(item.clone()));
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

fn named_state(
    parent: BorrowedFd<'_>,
    name: impl rustix::path::Arg,
    path: &Path,
    operation: &'static str,
) -> Result<NodeState, CheckError> {
    filesystem_access();
    rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| NodeState::from_stat(&stat))
        .map_err(|error| io_error(operation, path, error))
}

fn descriptor_state(
    fd: impl AsFd,
    path: &Path,
    operation: &'static str,
) -> Result<NodeState, CheckError> {
    filesystem_access();
    rfs::fstat(fd)
        .map(|stat| NodeState::from_stat(&stat))
        .map_err(|error| io_error(operation, path, error))
}

fn timestamp_ns(seconds: i64, nanoseconds: i64) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds)
}

fn isolation_note(reason: UnisolatedReason) -> String {
    match reason {
        UnisolatedReason::ReadOnlyFilesystem => "read-only filesystem".to_owned(),
        UnisolatedReason::UnwritableCorpus => "unwritable corpus".to_owned(),
    }
}

fn io_error(operation: &'static str, path: &Path, error: Errno) -> CheckError {
    CheckError::Io {
        operation,
        path: path.to_path_buf(),
        detail: error.to_string(),
    }
}

fn unsafe_error(path: &Path, detail: impl Into<String>) -> CheckError {
    CheckError::Unsafe {
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

fn raced(path: &Path, detail: impl Into<String>) -> CheckError {
    CheckError::Raced {
        path: path.to_path_buf(),
        detail: detail.into(),
    }
}

#[cfg(not(test))]
fn filesystem_access() {}

#[cfg(test)]
thread_local! {
    static FILESYSTEM_ACCESS_ALLOWED: std::cell::Cell<bool> = const { std::cell::Cell::new(true) };
}

#[cfg(test)]
fn filesystem_access() {
    FILESYSTEM_ACCESS_ALLOWED.with(|allowed| {
        assert!(
            allowed.get(),
            "validation attempted filesystem access after capture"
        );
    });
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::symlink;

    use tempfile::{TempDir, tempdir};

    use super::*;
    use crate::{BEGIN_MARKER, END_MARKER, ReindexOptions, acquire_lock, reindex_wiki};

    struct ForbidFilesystemAccess;

    struct DenyPreflight {
        denied_name: &'static str,
    }

    struct MetadataErrorPreflight {
        target_name: &'static str,
        kind: std::io::ErrorKind,
    }

    impl Preflight for DenyPreflight {
        fn accessible(&mut self, path: &Path, _access: Access) -> bool {
            path.file_name() != Some(OsStr::new(self.denied_name))
        }
    }

    impl Preflight for MetadataErrorPreflight {
        fn metadata(&mut self, path: &Path) -> std::io::Result<std::fs::Metadata> {
            if path.file_name() == Some(OsStr::new(self.target_name)) {
                Err(std::io::Error::new(self.kind, "injected metadata error"))
            } else {
                std::fs::metadata(path)
            }
        }
    }

    impl ForbidFilesystemAccess {
        fn new() -> Self {
            FILESYSTEM_ACCESS_ALLOWED.with(|allowed| {
                assert!(allowed.replace(false));
            });
            Self
        }
    }

    impl Drop for ForbidFilesystemAccess {
        fn drop(&mut self) {
            FILESYSTEM_ACCESS_ALLOWED.with(|allowed| allowed.set(true));
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum Phase {
        AfterPageRead,
        AfterPagesCaptured,
        AfterIndexRead,
        BeforeFinalValidation,
    }

    type Action = Box<dyn FnMut(&Path)>;

    #[derive(Default)]
    struct TestHooks {
        phase: Option<Phase>,
        action: Option<Action>,
        fail_iteration_at: Option<usize>,
        fail_read_name: Option<String>,
    }

    impl TestHooks {
        fn at(phase: Phase, action: impl FnMut(&Path) + 'static) -> Self {
            Self {
                phase: Some(phase),
                action: Some(Box::new(action)),
                ..Self::default()
            }
        }

        fn fire(&mut self, phase: Phase, path: &Path) {
            if self.phase == Some(phase) {
                self.phase = None;
                if let Some(mut action) = self.action.take() {
                    action(path);
                }
            }
        }
    }

    impl CaptureHooks for TestHooks {
        fn fail_directory_iteration(&mut self, _path: &Path, processed: usize) -> bool {
            if self.fail_iteration_at == Some(processed) {
                self.fail_iteration_at = None;
                true
            } else {
                false
            }
        }

        fn fail_page_read(&mut self, path: &Path) -> bool {
            self.fail_read_name
                .as_deref()
                .is_some_and(|name| path.file_name() == Some(OsStr::new(name)))
        }

        fn after_page_read(&mut self, path: &Path) {
            self.fire(Phase::AfterPageRead, path);
        }

        fn after_pages_captured(&mut self, path: &Path) {
            self.fire(Phase::AfterPagesCaptured, path);
        }

        fn after_index_read(&mut self, path: &Path) {
            self.fire(Phase::AfterIndexRead, path);
        }

        fn before_final_validation(&mut self, path: &Path) {
            self.fire(Phase::BeforeFinalValidation, path);
        }
    }

    fn source(slug: &str, summary: &str) -> String {
        format!(
            "---\nslug: {slug}\ntitle: Title\ntype: gotcha\nstatus: current\nowner: shared\nupdated: 2026-08-08\nverified: 2026-08-08\nsummary: {summary}\n---\n\nbody\n"
        )
    }

    fn fixture() -> TempDir {
        let tmp = tempdir().unwrap();
        fs::create_dir(tmp.path().join(PAGES_NAME)).unwrap();
        fs::write(tmp.path().join("pages/alpha.md"), source("alpha", "first")).unwrap();
        fs::write(tmp.path().join("pages/beta.md"), source("beta", "second")).unwrap();
        fs::write(
            tmp.path().join(INDEX_NAME),
            format!("preamble\n{BEGIN_MARKER}\nold\n{END_MARKER}\n"),
        )
        .unwrap();
        reindex_wiki(tmp.path(), &ReindexOptions::default()).unwrap();
        tmp
    }

    fn run_capture(corpus: &Path, hooks: &mut TestHooks) -> Result<WikiSnapshot, CheckError> {
        let guard = isolated_for_check(corpus, LockMode::Shared);
        capture_from_fd(
            guard.corpus_fd(),
            guard.corpus_path(),
            true,
            None,
            hooks,
            || guard.revalidate_before_commit().map_err(CheckError::from),
        )
    }

    fn isolated_for_check(corpus: &Path, mode: LockMode) -> crate::LockGuard {
        match acquire_lock(corpus, mode).unwrap() {
            LockLease::Isolated(guard) => guard,
            LockLease::Unisolated(value) => panic!("expected isolation, got {value:?}"),
        }
    }

    #[test]
    fn partial_iteration_is_an_operational_failure_not_a_short_snapshot() {
        let tmp = fixture();
        let mut hooks = TestHooks {
            fail_iteration_at: Some(1),
            ..TestHooks::default()
        };
        let error = run_capture(tmp.path(), &mut hooks).unwrap_err();
        assert!(matches!(error, CheckError::Io { .. }), "{error}");
    }

    #[test]
    fn unreadable_page_outcome_is_deterministic_and_does_not_abort_siblings() {
        let tmp = fixture();
        let mut hooks = TestHooks {
            fail_read_name: Some("alpha.md".to_owned()),
            ..TestHooks::default()
        };
        let snapshot = run_capture(tmp.path(), &mut hooks).unwrap();
        assert!(matches!(
            snapshot.pages[0].outcome,
            CapturedPageOutcome::Unreadable(_)
        ));
        assert!(matches!(
            snapshot.pages[1].outcome,
            CapturedPageOutcome::Present(_)
        ));
        let report = validate_wiki(&snapshot);
        assert!(
            report
                .failures
                .contains(&"pages/alpha.md is not readable".to_owned())
        );
    }

    #[test]
    fn page_replacement_after_read_aborts_the_snapshot() {
        let tmp = fixture();
        let original = tmp.path().join("pages/alpha.md");
        let stale = tmp.path().join("pages/alpha.old");
        let mut hooks = TestHooks::at(Phase::AfterPageRead, move |path| {
            if path.file_name() == Some(OsStr::new("alpha.md")) {
                fs::rename(&original, &stale).unwrap();
                fs::write(&original, source("alpha", "replaced")).unwrap();
            }
        });
        let error = run_capture(tmp.path(), &mut hooks).unwrap_err();
        assert!(matches!(error, CheckError::Raced { .. }), "{error}");
    }

    #[test]
    fn page_set_and_directory_rebinding_abort_the_snapshot() {
        for operation in ["add", "delete", "replace-directory"] {
            let tmp = fixture();
            let root = tmp.path().to_path_buf();
            let action = operation.to_owned();
            let mut hooks =
                TestHooks::at(Phase::AfterPagesCaptured, move |_| match action.as_str() {
                    "add" => {
                        fs::write(root.join("pages/gamma.md"), source("gamma", "new")).unwrap()
                    }
                    "delete" => fs::remove_file(root.join("pages/alpha.md")).unwrap(),
                    "replace-directory" => {
                        fs::rename(root.join(PAGES_NAME), root.join("old-pages")).unwrap();
                        fs::create_dir(root.join(PAGES_NAME)).unwrap();
                    }
                    _ => unreachable!(),
                });
            let error = run_capture(tmp.path(), &mut hooks).unwrap_err();
            assert!(
                matches!(error, CheckError::Raced { .. } | CheckError::Io { .. }),
                "{operation}: {error}"
            );
        }
    }

    #[test]
    fn irrelevant_non_markdown_churn_does_not_invalidate_capture() {
        for operation in ["add", "delete", "replace"] {
            let tmp = fixture();
            fs::write(tmp.path().join("pages/irrelevant.txt"), b"before").unwrap();
            let root = tmp.path().to_path_buf();
            let action = operation.to_owned();
            let mut hooks = TestHooks::at(Phase::AfterPagesCaptured, move |_| {
                let path = root.join("pages/irrelevant.txt");
                match action.as_str() {
                    "add" => fs::write(root.join("pages/another.txt"), b"noise").unwrap(),
                    "delete" => fs::remove_file(path).unwrap(),
                    "replace" => fs::write(path, b"after!").unwrap(),
                    _ => unreachable!(),
                }
            });
            let snapshot = run_capture(tmp.path(), &mut hooks).unwrap();
            assert_eq!(snapshot.pages.len(), 2, "{operation}");
        }
    }

    #[test]
    fn checker_preflight_readability_runs_before_lock_creation() {
        for denied_name in [PAGES_NAME, INDEX_NAME] {
            let tmp = tempdir().unwrap();
            fs::create_dir(tmp.path().join(PAGES_NAME)).unwrap();
            fs::write(tmp.path().join(INDEX_NAME), b"placeholder").unwrap();
            let mut preflight = DenyPreflight { denied_name };
            let mut capture = SystemCaptureHooks;
            let report = check_wiki_with(
                tmp.path(),
                Duration::from_millis(1),
                &mut preflight,
                &mut capture,
            );
            assert_eq!(
                report.failures,
                [format!(
                    "{} is not readable",
                    tmp.path().join(denied_name).display()
                )]
            );
            assert!(!tmp.path().join(crate::LOCK_NAME).exists());
        }
    }

    #[test]
    fn checker_preflight_metadata_errors_are_not_misreported_as_missing() {
        for target_name in [PAGES_NAME, INDEX_NAME] {
            for kind in [
                std::io::ErrorKind::PermissionDenied,
                std::io::ErrorKind::Other,
            ] {
                let tmp = tempdir().unwrap();
                fs::create_dir(tmp.path().join(PAGES_NAME)).unwrap();
                fs::write(tmp.path().join(INDEX_NAME), b"placeholder").unwrap();
                let mut preflight = MetadataErrorPreflight { target_name, kind };
                let mut capture = SystemCaptureHooks;
                let report = check_wiki_with(
                    tmp.path(),
                    Duration::from_millis(1),
                    &mut preflight,
                    &mut capture,
                );
                assert_eq!(
                    report.failures,
                    [format!(
                        "{} is not readable",
                        tmp.path().join(target_name).display()
                    )],
                    "{target_name}/{kind:?}"
                );
                assert!(
                    !tmp.path().join(crate::LOCK_NAME).exists(),
                    "{target_name}/{kind:?}"
                );
            }
        }
    }

    #[test]
    fn checker_maps_busy_and_unsafe_locks_to_structured_failures() {
        let tmp = fixture();
        let guard = isolated_for_check(tmp.path(), LockMode::Exclusive);
        let mut preflight = SystemPreflight;
        let mut capture = SystemCaptureHooks;
        let busy = check_wiki_with(
            tmp.path(),
            Duration::from_millis(1),
            &mut preflight,
            &mut capture,
        );
        assert_eq!(busy.failures.len(), 1);
        assert!(busy.failures[0].starts_with("could not obtain a consistent snapshot:"));
        drop(guard);

        fs::remove_file(tmp.path().join(crate::LOCK_NAME)).unwrap();
        symlink(INDEX_NAME, tmp.path().join(crate::LOCK_NAME)).unwrap();
        let unsafe_report = check_wiki(tmp.path()).unwrap();
        assert_eq!(unsafe_report.failures.len(), 1);
        assert!(unsafe_report.failures[0].starts_with("unsafe wiki lock at"));
    }

    #[test]
    fn checker_maps_known_capture_failures_to_a_report() {
        let tmp = fixture();
        let root = tmp.path().to_path_buf();
        let mut preflight = SystemPreflight;
        let mut hooks = TestHooks::at(Phase::AfterIndexRead, move |index| {
            fs::rename(index, root.join("old-index")).unwrap();
            fs::write(index, b"replacement").unwrap();
        });
        let report = check_wiki_with(
            tmp.path(),
            Duration::from_millis(10),
            &mut preflight,
            &mut hooks,
        );
        assert_eq!(report.failures.len(), 1);
        assert!(
            report.failures[0].starts_with("could not obtain a consistent snapshot:"),
            "{report:?}"
        );
    }

    #[test]
    fn index_symlink_and_corpus_rebinding_abort_the_snapshot() {
        for operation in ["index-symlink", "corpus"] {
            let tmp = fixture();
            let root = tmp.path().to_path_buf();
            let old_root = root.with_file_name(format!(
                "{}-check-detached",
                root.file_name().unwrap().to_string_lossy()
            ));
            let old_for_hook = old_root.clone();
            let action = operation.to_owned();
            let phase = if operation == "index-symlink" {
                Phase::AfterIndexRead
            } else {
                Phase::BeforeFinalValidation
            };
            let mut hooks = TestHooks::at(phase, move |path| {
                if action == "index-symlink" {
                    let index = path.to_path_buf();
                    fs::rename(&index, root.join("old-index")).unwrap();
                    symlink(root.join("old-index"), index).unwrap();
                } else {
                    fs::rename(&root, &old_for_hook).unwrap();
                    fs::create_dir(&root).unwrap();
                    fs::create_dir(root.join(PAGES_NAME)).unwrap();
                    fs::write(root.join(INDEX_NAME), b"new").unwrap();
                }
            });
            let error = run_capture(tmp.path(), &mut hooks).unwrap_err();
            assert!(
                matches!(
                    error,
                    CheckError::Unsafe { .. } | CheckError::Raced { .. } | CheckError::Lock(_)
                ),
                "{operation}: {error}"
            );
            if operation == "corpus" {
                fs::remove_dir_all(tmp.path()).unwrap();
                fs::rename(old_root, tmp.path()).unwrap();
            }
        }
    }

    #[test]
    fn failure_and_note_order_is_stable_and_duplicates_are_removed() {
        let snapshot = WikiSnapshot {
            corpus: PathBuf::from("/must/not/be/read"),
            index: format!("{BEGIN_MARKER}\n\n{END_MARKER}\n").into_bytes(),
            pages: vec![
                CapturedPage {
                    name: OsString::from("alpha.md"),
                    outcome: CapturedPageOutcome::Unreadable("denied".to_owned()),
                },
                CapturedPage {
                    name: OsString::from("beta.md"),
                    outcome: CapturedPageOutcome::Symlink,
                },
            ],
            isolated: false,
            isolation_note: Some("read-only filesystem".to_owned()),
        };
        let report = validate_wiki(&snapshot);
        assert_eq!(
            report.failures,
            [
                "pages/alpha.md is not listed in INDEX.md (orphan page)",
                "pages/beta.md is not listed in INDEX.md (orphan page)",
                "pages/alpha.md is not readable",
                "pages/beta.md is a symlink — refused",
            ]
        );
        assert_eq!(report.notes.len(), 3);
        let unique = report.notes.iter().collect::<HashSet<_>>();
        assert_eq!(unique.len(), report.notes.len());
        assert!(report.notes[0].contains("without isolation"));
    }

    #[test]
    fn validation_runs_while_every_injected_filesystem_access_panics() {
        let tmp = fixture();
        let snapshot = capture_wiki(tmp.path()).unwrap();
        let _forbid = ForbidFilesystemAccess::new();
        let report = validate_wiki(&snapshot);
        assert!(report.failures.is_empty(), "{report:?}");
    }
}
