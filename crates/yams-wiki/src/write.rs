use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use serde::de::{Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value, json};
use yams_core::{ExitCode, MAX_FILE_BYTES};

use crate::ReindexOptions;
use crate::durable::{DurableError, canonical_digest_locked, reindex_locked};
use crate::lock::{LockError, LockGuard, LockLease, LockMode, acquire_lock};
use crate::schema::{
    CreateRequest, UpdateRequest, render_create, render_update, validate_today_input,
    validate_update_request,
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
const PAGES_NAME: &str = "pages";
const INDEX_NAME: &str = "INDEX.md";
const TEMP_ATTEMPTS: u64 = 128;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// One complete machine-readable answer from the write library boundary.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WriteResult {
    pub exit_code: ExitCode,
    pub body: Value,
}

trait WriteHooks {
    fn before_create_link(&mut self, _pages: &Path, _target: &str) {}
    fn before_update_commit(&mut self, _pages: &Path, _target: &str) {}
    fn after_page_temp_created(&mut self, _pages: &Path, _name: &OsStr) {}
    fn after_forward_page_enumeration(&mut self, _pages: &Path) {}
    fn after_reindex(&mut self, _corpus: &Path) {}

    fn acquire_lock(&mut self, corpus: &Path, mode: LockMode) -> Result<LockLease, LockError> {
        acquire_lock(corpus, mode)
    }

    fn open_temporary(
        &mut self,
        directory: BorrowedFd<'_>,
        name: &OsStr,
        mode: Mode,
    ) -> Result<OwnedFd, Errno> {
        rfs::openat(directory, name, TEMP_FLAGS, mode)
    }

    fn write(&mut self, fd: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno> {
        rustix::io::write(fd, bytes)
    }

    fn chmod(&mut self, fd: BorrowedFd<'_>, mode: Mode) -> Result<(), Errno> {
        rfs::fchmod(fd, mode)
    }

    fn file_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        rfs::fsync(fd)
    }

    fn link(
        &mut self,
        directory: BorrowedFd<'_>,
        temporary: &OsStr,
        target: &str,
    ) -> Result<(), Errno> {
        rfs::linkat(directory, temporary, directory, target, AtFlags::empty())
    }

    fn unlink(&mut self, directory: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
        rfs::unlinkat(directory, name, AtFlags::empty())
    }

    fn rename(
        &mut self,
        directory: BorrowedFd<'_>,
        temporary: &OsStr,
        target: &str,
    ) -> Result<(), Errno> {
        rfs::renameat(directory, temporary, directory, target)
    }

    fn directory_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        rfs::fsync(fd)
    }

    fn cleanup_temporary_named_state(
        &mut self,
        directory: BorrowedFd<'_>,
        name: &OsStr,
    ) -> Result<NodeState, Errno> {
        rfs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
            .map(|stat| NodeState::from_stat(&stat))
    }

    fn reindex(
        &mut self,
        guard: &LockGuard,
        options: &ReindexOptions,
    ) -> Result<crate::ReindexResult, DurableError> {
        reindex_locked(guard, options)
    }
}

struct SystemHooks;

impl WriteHooks for SystemHooks {}

impl WriteResult {
    fn new(exit_code: ExitCode, body: Value) -> Self {
        Self { exit_code, body }
    }

    fn refusal(error: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(
            ExitCode::Usage,
            json!({"ok": false, "exit": 2, "error": error.into(), "hint": hint.into()}),
        )
    }

    fn operational(error: impl Into<String>, hint: impl Into<String>) -> Self {
        Self::new(
            ExitCode::Operational,
            json!({"ok": false, "exit": 4, "error": error.into(), "hint": hint.into()}),
        )
    }
}

enum Request {
    Create {
        request: CreateRequest,
        rendered: String,
    },
    Update(UpdateRequest),
}

/// Parse, validate, lock, and durably write one shared-memory page.
///
/// `today` is injected so the eventual CLI can own local-date policy without
/// making transaction tests depend on the wall clock.
pub fn write_json(corpus: &Path, input: &[u8], today: &str) -> WriteResult {
    write_json_with_hooks(corpus, input, today, &mut SystemHooks)
}

fn write_json_with_hooks(
    corpus: &Path,
    input: &[u8],
    today: &str,
    hooks: &mut impl WriteHooks,
) -> WriteResult {
    let value = match parse_input(input) {
        Ok(value) => value,
        Err(result) => return result,
    };
    let Some(object) = value.as_object() else {
        return WriteResult::refusal("payload is not a JSON object", "send one JSON object");
    };
    if let Err(error) = validate_today_input(today) {
        return WriteResult::operational(error.to_string(), "supply a valid local date");
    }

    let request = if routes_to_update(object) {
        match serde_json::from_value::<UpdateRequest>(value) {
            Ok(request) => {
                if let Err(error) = validate_update_request(&request, today) {
                    return WriteResult::refusal(error.to_string(), "fix the request and retry");
                }
                Request::Update(request)
            }
            Err(error) => {
                return WriteResult::refusal(error.to_string(), "fix the request and retry");
            }
        }
    } else {
        match serde_json::from_value::<CreateRequest>(value) {
            Ok(request) => match render_create(&request, today) {
                Ok(rendered) => Request::Create { request, rendered },
                Err(error) => {
                    return WriteResult::refusal(error.to_string(), "fix the request and retry");
                }
            },
            Err(error) => {
                return WriteResult::refusal(error.to_string(), "fix the request and retry");
            }
        }
    };

    if corpus.to_str().is_none() {
        return WriteResult::operational(
            "corpus path is not valid UTF-8",
            "use a UTF-8 corpus path",
        );
    }

    let guard = match hooks.acquire_lock(corpus, LockMode::Exclusive) {
        Ok(LockLease::Isolated(guard)) => guard,
        Ok(LockLease::Unisolated(unisolated)) => {
            return WriteResult::operational(
                format!("the corpus is not writable: {:?}", unisolated.reason),
                "make the memory directory writable and retry",
            );
        }
        Err(error) => {
            return WriteResult::operational(error.to_string(), "inspect the lock path and retry");
        }
    };

    let digest = match canonical_digest_locked(&guard) {
        Ok(Some(digest)) => digest,
        Ok(None) => {
            return WriteResult::refusal(
                "INDEX.md is not canonical, so a write is refused",
                "run `yams-wiki catalog .agents/memory` and retry",
            );
        }
        Err(error) => {
            return WriteResult::operational(error.to_string(), "inspect the wiki and retry");
        }
    };

    let pages = match PinnedPages::open(&guard) {
        Ok(pages) => pages,
        Err(error) => return WriteResult::operational(error, "inspect the pages directory"),
    };
    let existing = match pages.page_slugs(&guard, hooks) {
        Ok(existing) => existing,
        Err(error) => return WriteResult::operational(error, "inspect the pages directory"),
    };

    match request {
        Request::Create { request, rendered } => locked_create(
            corpus, &guard, &pages, existing, &digest, &request, &rendered, hooks,
        ),
        Request::Update(request) => locked_update(
            corpus, &guard, &pages, existing, &digest, &request, today, hooks,
        ),
    }
}

fn parse_input(input: &[u8]) -> Result<Value, WriteResult> {
    if input.len() as u64 > MAX_FILE_BYTES {
        return Err(WriteResult::refusal(
            format!("stdin exceeds MAX_FILE_BYTES ({MAX_FILE_BYTES} bytes)"),
            "send one smaller JSON object",
        ));
    }
    let source = std::str::from_utf8(input).map_err(|error| {
        WriteResult::refusal(
            format!("stdin is not valid UTF-8: {error}"),
            "send one UTF-8 JSON object",
        )
    })?;
    let mut deserializer = serde_json::Deserializer::from_str(source);
    let StrictValue(value) = StrictValue::deserialize(&mut deserializer).map_err(|error| {
        WriteResult::refusal(
            format!("stdin is not valid JSON: {error}"),
            "send one JSON object",
        )
    })?;
    deserializer.end().map_err(|error| {
        WriteResult::refusal(
            format!("stdin is not valid JSON: {error}"),
            "send one JSON object",
        )
    })?;
    Ok(value)
}

fn routes_to_update(object: &Map<String, Value>) -> bool {
    object.contains_key("target") || object.get("update").is_some_and(json_is_python_truthy)
}

fn json_is_python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => {
            value.as_i64().is_some_and(|value| value != 0)
                || value.as_u64().is_some_and(|value| value != 0)
                || value.as_f64().is_some_and(|value| value != 0.0)
        }
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

// Keep capability-bearing inputs explicit at this durability boundary.
#[allow(clippy::too_many_arguments)]
fn locked_create(
    response_corpus: &Path,
    guard: &LockGuard,
    pages: &PinnedPages,
    mut existing: HashSet<String>,
    digest: &str,
    request: &CreateRequest,
    rendered: &str,
    hooks: &mut impl WriteHooks,
) -> WriteResult {
    let slug = crate::slugify(&request.title).expect("validated create slug");
    let name = format!("{slug}.md");
    let page_path = response_corpus.join(PAGES_NAME).join(&name);
    match pages.named_state(&name) {
        Ok(Some(_)) => return collision(&slug),
        Ok(None) => {}
        Err(error) => return WriteResult::operational(error, "inspect the target page"),
    }
    existing.insert(slug.clone());
    let forward_refs = forward_refs(&request.related, &existing);

    match pages.install_create(guard, &name, rendered.as_bytes(), hooks) {
        Ok(()) => finish_index(
            response_corpus,
            guard,
            digest,
            &slug,
            &page_path,
            forward_refs,
            hooks,
        ),
        Err(InstallFailure::Collision) => collision(&slug),
        Err(error) if error.page_visible() => {
            page_failure(&slug, &page_path, error.describe(&slug))
        }
        Err(error) => WriteResult::operational(
            error.describe(&slug),
            "make the memory directory writable and retry",
        ),
    }
}

// Keep capability-bearing inputs explicit at this durability boundary.
#[allow(clippy::too_many_arguments)]
fn locked_update(
    response_corpus: &Path,
    guard: &LockGuard,
    pages: &PinnedPages,
    mut existing: HashSet<String>,
    digest: &str,
    request: &UpdateRequest,
    today: &str,
    hooks: &mut impl WriteHooks,
) -> WriteResult {
    let slug = request.target.clone();
    let name = format!("{slug}.md");
    let page_path = response_corpus.join(PAGES_NAME).join(&name);
    let current = match pages.read_update_target(&name) {
        Ok(Some(current)) => current,
        Ok(None) => {
            return WriteResult::new(
                ExitCode::Usage,
                json!({
                    "ok": false,
                    "exit": 2,
                    "error": format!("pages/{slug}.md does not exist"),
                    "hint": "drop \"update\" to create it",
                    "slug": slug,
                }),
            );
        }
        Err(error) => return WriteResult::operational(error, "inspect the target page"),
    };

    let source = match std::str::from_utf8(&current.bytes) {
        Ok(source) => source,
        Err(error) => {
            return WriteResult::operational(
                format!("pages/{slug}.md could not be read: {error}"),
                "inspect the page",
            );
        }
    };
    let rendered = match render_update(request, source, today) {
        Ok(rendered) => rendered,
        Err(error) => {
            return WriteResult::new(
                ExitCode::Usage,
                json!({
                    "ok": false,
                    "exit": 2,
                    "error": error.to_string(),
                    "hint": "correct the page and retry",
                    "slug": slug,
                }),
            );
        }
    };
    existing.insert(slug.clone());
    let forward_refs = forward_refs(&request.related, &existing);
    match pages.install_update(guard, &name, rendered.page.as_bytes(), current.state, hooks) {
        Ok(()) => finish_index(
            response_corpus,
            guard,
            digest,
            &slug,
            &page_path,
            forward_refs,
            hooks,
        ),
        Err(InstallFailure::Collision) => unreachable!("updates do not link a new target"),
        Err(error) if error.page_visible() => {
            page_failure(&slug, &page_path, error.describe(&slug))
        }
        Err(error) => WriteResult::operational(
            error.describe(&slug),
            "make the memory directory writable and retry",
        ),
    }
}

fn finish_index(
    response_corpus: &Path,
    guard: &LockGuard,
    digest: &str,
    slug: &str,
    page_path: &Path,
    forward_refs: Vec<String>,
    hooks: &mut impl WriteHooks,
) -> WriteResult {
    let options = ReindexOptions {
        expected_sha256: Some(digest.to_owned()),
        ..ReindexOptions::default()
    };
    match hooks.reindex(guard, &options) {
        Ok(result) => {
            hooks.after_reindex(guard.corpus_path());
            let mut paths = vec![path_value(page_path)];
            if result.changed {
                paths.push(path_value(&response_corpus.join(INDEX_NAME)));
            }
            WriteResult::new(
                ExitCode::Ok,
                json!({
                    "ok": true,
                    "slug": slug,
                    "paths": paths,
                    "index_regenerated": result.changed,
                    "forward_refs": forward_refs,
                }),
            )
        }
        Err(error) if index_was_replaced(&error) => WriteResult::new(
            ExitCode::Operational,
            json!({
                "ok": false,
                "exit": 4,
                "error": error.to_string(),
                "hint": "confirm the corpus with yams-wiki catalog --check",
                "slug": slug,
                "paths": [path_value(page_path), path_value(&response_corpus.join(INDEX_NAME))],
                "index_regenerated": true,
            }),
        ),
        Err(error) => WriteResult::new(
            ExitCode::Operational,
            json!({
                "ok": false,
                "exit": 4,
                "error": error.to_string(),
                "hint": "inspect INDEX.md, then run yams-wiki catalog",
                "slug": slug,
                "paths": [path_value(page_path)],
                "index_regenerated": false,
            }),
        ),
    }
}

fn index_was_replaced(error: &DurableError) -> bool {
    match error {
        DurableError::ReplacedNotDurable(_) => true,
        DurableError::CleanupFailed { original, .. }
        | DurableError::TemporaryRebound { original, .. } => index_was_replaced(original),
        _ => false,
    }
}

fn collision(slug: &str) -> WriteResult {
    WriteResult::new(
        ExitCode::Usage,
        json!({
            "ok": false,
            "exit": 2,
            "error": format!("pages/{slug}.md already exists"),
            "hint": "pass \"update\": true to replace it",
            "slug": slug,
        }),
    )
}

fn page_failure(slug: &str, page_path: &Path, error: String) -> WriteResult {
    WriteResult::new(
        ExitCode::Operational,
        json!({
            "ok": false,
            "exit": 4,
            "error": error,
            "hint": "confirm the page, then run yams-wiki catalog",
            "slug": slug,
            "paths": [path_value(page_path)],
            "index_regenerated": false,
        }),
    )
}

fn path_value(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn forward_refs(related: &[String], existing: &HashSet<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    related
        .iter()
        .filter(|slug| seen.insert(slug.as_str()) && !existing.contains(slug.as_str()))
        .cloned()
        .collect()
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

struct PinnedPages {
    fd: OwnedFd,
    state: NodeState,
    path: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct PageEntry {
    name: OsString,
    state: NodeState,
}

impl PinnedPages {
    fn open(guard: &LockGuard) -> Result<Self, String> {
        let path = guard.corpus_path().join(PAGES_NAME);
        let candidate = named_state(guard.corpus_fd(), PAGES_NAME, &path, "inspect pages")?;
        if !candidate.kind.is_dir() {
            return Err(format!(
                "unsafe wiki object at {}: expected a non-symlink directory",
                path.display()
            ));
        }
        let fd = rfs::openat(
            guard.corpus_fd(),
            PAGES_NAME,
            DIRECTORY_FLAGS,
            Mode::empty(),
        )
        .map_err(|error| io_error("open pages without following links", &path, error))?;
        let opened = descriptor_state(&fd, &path, "inspect opened pages")?;
        if opened != candidate || !opened.kind.is_dir() {
            return Err(format!(
                "wiki changed while opening pages: {}: pages changed while opening",
                path.display()
            ));
        }
        Ok(Self {
            fd,
            state: opened,
            path,
        })
    }

    fn page_slugs(
        &self,
        guard: &LockGuard,
        hooks: &mut impl WriteHooks,
    ) -> Result<HashSet<String>, String> {
        let entries = self.enumerate_pages()?;
        hooks.after_forward_page_enumeration(&self.path);
        self.verify_binding(guard)?;
        if self.enumerate_pages()? != entries {
            return Err(format!(
                "wiki changed while capturing {}: page name or metadata set changed",
                self.path.display()
            ));
        }
        let mut slugs = HashSet::new();
        for entry in entries {
            let bytes = entry.name.as_bytes();
            let Some(stem) = bytes.strip_suffix(b".md") else {
                continue;
            };
            let slug = std::str::from_utf8(stem).map_err(|_| {
                format!(
                    "pages directory contains a non-UTF-8 markdown name: {}",
                    diagnostic_name(bytes)
                )
            })?;
            slugs.insert(slug.to_owned());
        }
        Ok(slugs)
    }

    fn enumerate_pages(&self) -> Result<Vec<PageEntry>, String> {
        let mut stream = Dir::read_from(self.fd.as_fd())
            .map_err(|error| io_error("open directory stream", &self.path, error))?;
        let mut names = Vec::new();
        for entry in &mut stream {
            let entry =
                entry.map_err(|error| io_error("read directory entry", &self.path, error))?;
            let bytes = entry.file_name().to_bytes();
            if bytes != b"." && bytes != b".." && bytes.ends_with(b".md") {
                names.push(OsString::from_vec(bytes.to_vec()));
            }
        }
        names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
        names
            .into_iter()
            .map(|name| {
                let path = self.path.join(&name);
                let state = named_state(self.fd.as_fd(), &name, &path, "inspect page entry")?;
                if !state.kind.is_file() {
                    return Err(format!(
                        "unsafe wiki object at {}: expected a non-symlink regular file",
                        path.display()
                    ));
                }
                Ok(PageEntry { name, state })
            })
            .collect()
    }

    fn named_state(&self, name: &str) -> Result<Option<NodeState>, String> {
        let path = self.path.join(name);
        match rfs::statat(self.fd.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(NodeState::from_stat(&stat))),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(io_error("inspect page", &path, error)),
        }
    }

    fn read_update_target(&self, name: &str) -> Result<Option<CapturedTarget>, String> {
        let path = self.path.join(name);
        let Some(candidate) = self.named_state(name)? else {
            return Ok(None);
        };
        if !candidate.kind.is_file() || candidate.nlink != 1 {
            return Err(format!(
                "unsafe wiki object at {}: update target must be a regular file with one link",
                path.display()
            ));
        }
        let fd = rfs::openat(self.fd.as_fd(), name, FILE_FLAGS, Mode::empty())
            .map_err(|error| io_error("open page without following links", &path, error))?;
        let opened = descriptor_state(&fd, &path, "inspect opened page")?;
        if opened != candidate || !opened.kind.is_file() || opened.nlink != 1 {
            return Err(format!(
                "wiki changed while reading page: {}: target changed while opening",
                path.display()
            ));
        }
        let mut file = File::from(fd);
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| format!("could not read {}: {error}", path.display()))?;
        let descriptor = descriptor_state(&file, &path, "reinspect page descriptor")?;
        let named = self.named_state(name)?.ok_or_else(|| {
            format!(
                "wiki changed while reading page: {}: target disappeared",
                path.display()
            )
        })?;
        if descriptor != opened || named != opened || opened.size != bytes.len() as u64 {
            return Err(format!(
                "wiki changed while reading page: {}: target changed while reading",
                path.display()
            ));
        }
        Ok(Some(CapturedTarget {
            state: opened,
            bytes,
        }))
    }

    fn install_create(
        &self,
        guard: &LockGuard,
        target: &str,
        bytes: &[u8],
        hooks: &mut impl WriteHooks,
    ) -> Result<(), InstallFailure> {
        let ordinary = Mode::RUSR | Mode::WUSR | Mode::RGRP | Mode::WGRP | Mode::ROTH | Mode::WOTH;
        let mut temporary = self.create_temporary(hooks, ordinary)?;
        let result = (|| {
            self.validate_temporary(&mut temporary)?;
            write_all(hooks, temporary.fd.as_fd(), bytes, &self.path.join(target))?;
            hooks.file_fsync(temporary.fd.as_fd()).map_err(|error| {
                InstallFailure::Before(io_error(
                    "fsync page temporary",
                    &self.path.join(target),
                    error,
                ))
            })?;
            guard
                .revalidate_before_commit()
                .map_err(|error| InstallFailure::Before(error.to_string()))?;
            self.verify_binding(guard).map_err(InstallFailure::Before)?;
            if self
                .named_state(target)
                .map_err(InstallFailure::Before)?
                .is_some()
            {
                return Err(InstallFailure::Collision);
            }
            self.verify_temporary(&temporary)
                .map_err(InstallFailure::Before)?;
            hooks.before_create_link(&self.path, target);
            match hooks.link(self.fd.as_fd(), &temporary.name, target) {
                Ok(()) => {}
                Err(Errno::EXIST) => return Err(InstallFailure::Collision),
                Err(error) => {
                    return Err(InstallFailure::Before(io_error(
                        "link page temporary",
                        &self.path.join(target),
                        error,
                    )));
                }
            }
            self.unlink_temporary(&temporary, hooks)
                .map_err(InstallFailure::After)?;
            temporary.unlinked = true;
            hooks.directory_fsync(self.fd.as_fd()).map_err(|error| {
                InstallFailure::After(format!(
                    "pages/{target} was written but its directory could not be flushed, so it may not survive a crash: {error}"
                ))
            })?;
            Ok(())
        })();
        let cleanup = self.cleanup_temporary(&temporary, hooks);
        compose_cleanup(result, cleanup)
    }

    fn install_update(
        &self,
        guard: &LockGuard,
        target: &str,
        bytes: &[u8],
        expected: NodeState,
        hooks: &mut impl WriteHooks,
    ) -> Result<(), InstallFailure> {
        let private = Mode::RUSR | Mode::WUSR;
        let mut temporary = self.create_temporary(hooks, private)?;
        let result = (|| {
            self.validate_temporary(&mut temporary)?;
            write_all(hooks, temporary.fd.as_fd(), bytes, &self.path.join(target))?;
            let mode = Mode::from_bits_retain((expected.mode & 0o7777) as _);
            hooks.chmod(temporary.fd.as_fd(), mode).map_err(|error| {
                InstallFailure::Before(io_error(
                    "apply target page mode to temporary",
                    &self.path.join(target),
                    error,
                ))
            })?;
            let applied = descriptor_state(
                &temporary.fd,
                &self.path.join(target),
                "verify page temporary mode",
            )
            .map_err(InstallFailure::Before)?;
            if applied.mode & 0o7777 != expected.mode & 0o7777 {
                return Err(InstallFailure::Before(format!(
                    "unsafe wiki object at {}: temporary mode does not match target mode",
                    self.path.join(target).display()
                )));
            }
            hooks.file_fsync(temporary.fd.as_fd()).map_err(|error| {
                InstallFailure::Before(io_error(
                    "fsync page temporary",
                    &self.path.join(target),
                    error,
                ))
            })?;
            guard
                .revalidate_before_commit()
                .map_err(|error| InstallFailure::Before(error.to_string()))?;
            self.verify_binding(guard).map_err(InstallFailure::Before)?;
            hooks.before_update_commit(&self.path, target);
            let current = self
                .named_state(target)
                .map_err(InstallFailure::Before)?
                .ok_or_else(|| {
                    InstallFailure::Before(format!(
                        "wiki changed while updating {}: target disappeared",
                        self.path.join(target).display()
                    ))
                })?;
            if current != expected || !current.kind.is_file() || current.nlink != 1 {
                return Err(InstallFailure::Before(format!(
                    "wiki changed while updating {}: target identity or metadata changed",
                    self.path.join(target).display()
                )));
            }
            self.verify_temporary(&temporary)
                .map_err(InstallFailure::Before)?;
            hooks
                .rename(self.fd.as_fd(), &temporary.name, target)
                .map_err(|error| {
                    InstallFailure::Before(io_error(
                        "replace target page",
                        &self.path.join(target),
                        error,
                    ))
                })?;
            temporary.unlinked = true;
            hooks.directory_fsync(self.fd.as_fd()).map_err(|error| {
                InstallFailure::After(format!(
                    "pages/{target} was written but its directory could not be flushed, so it may not survive a crash: {error}"
                ))
            })?;
            Ok(())
        })();
        let cleanup = self.cleanup_temporary(&temporary, hooks);
        compose_cleanup(result, cleanup)
    }

    fn create_temporary(
        &self,
        hooks: &mut impl WriteHooks,
        mode: Mode,
    ) -> Result<PageTemporary, InstallFailure> {
        let pid = std::process::id();
        for _ in 0..TEMP_ATTEMPTS {
            let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let name = OsString::from(format!(".memory-write.{pid}.{sequence}.tmp"));
            match hooks.open_temporary(self.fd.as_fd(), &name, mode) {
                Ok(fd) => {
                    hooks.after_page_temp_created(&self.path, &name);
                    return Ok(PageTemporary {
                        fd,
                        name,
                        identity: None,
                        unlinked: false,
                    });
                }
                Err(Errno::EXIST) => continue,
                Err(error) => {
                    return Err(InstallFailure::Before(io_error(
                        "create unique page temporary",
                        &self.path.join(name),
                        error,
                    )));
                }
            }
        }
        Err(InstallFailure::Before(format!(
            "unsafe wiki object at {}: could not find a unique page temporary",
            self.path.display()
        )))
    }

    fn validate_temporary(&self, temporary: &mut PageTemporary) -> Result<(), InstallFailure> {
        let path = self.path.join(&temporary.name);
        let descriptor = descriptor_state(&temporary.fd, &path, "inspect page temporary")
            .map_err(InstallFailure::Before)?;
        temporary.identity = Some((descriptor.device, descriptor.inode));
        if !descriptor.kind.is_file() || descriptor.nlink != 1 {
            return Err(InstallFailure::Before(format!(
                "unsafe wiki object at {}: page temporary must be a regular file with one link",
                path.display()
            )));
        }
        let named = named_state(
            self.fd.as_fd(),
            &temporary.name,
            &path,
            "bind page temporary",
        )
        .map_err(InstallFailure::Before)?;
        if named != descriptor {
            return Err(InstallFailure::Before(format!(
                "wiki changed while creating page temporary: {}: name was rebound",
                path.display()
            )));
        }
        Ok(())
    }

    fn verify_temporary(&self, temporary: &PageTemporary) -> Result<(), String> {
        let path = self.path.join(&temporary.name);
        let descriptor = descriptor_state(&temporary.fd, &path, "reinspect page temporary")?;
        let named = named_state(
            self.fd.as_fd(),
            &temporary.name,
            &path,
            "rebind page temporary",
        )?;
        let Some(identity) = temporary.identity else {
            return Err(format!(
                "wiki changed while validating page temporary: {}: identity was not established",
                path.display()
            ));
        };
        if (descriptor.device, descriptor.inode) != identity
            || named != descriptor
            || !descriptor.kind.is_file()
            || descriptor.nlink != 1
        {
            return Err(format!(
                "wiki changed while validating page temporary: {}: name no longer binds the created inode",
                path.display()
            ));
        }
        Ok(())
    }

    fn unlink_temporary(
        &self,
        temporary: &PageTemporary,
        hooks: &mut impl WriteHooks,
    ) -> Result<(), String> {
        hooks
            .unlink(self.fd.as_fd(), &temporary.name)
            .map_err(|error| {
                io_error(
                    "unlink page temporary",
                    &self.path.join(&temporary.name),
                    error,
                )
            })
    }

    fn cleanup_temporary(
        &self,
        temporary: &PageTemporary,
        hooks: &mut impl WriteHooks,
    ) -> TemporaryCleanup {
        if temporary.unlinked {
            return TemporaryCleanup::Missing;
        }
        let path = self.path.join(&temporary.name);
        let descriptor = match rfs::fstat(&temporary.fd) {
            Ok(stat) => NodeState::from_stat(&stat),
            Err(error) => return TemporaryCleanup::Failed(path, error.to_string()),
        };
        if temporary
            .identity
            .is_some_and(|identity| (descriptor.device, descriptor.inode) != identity)
        {
            return TemporaryCleanup::Rebound(path);
        }
        let named = match hooks.cleanup_temporary_named_state(self.fd.as_fd(), &temporary.name) {
            Ok(state) => state,
            Err(Errno::NOENT) => return TemporaryCleanup::Missing,
            Err(error) => return TemporaryCleanup::Failed(path, error.to_string()),
        };
        if (named.device, named.inode) != (descriptor.device, descriptor.inode) {
            return TemporaryCleanup::Rebound(path);
        }
        match hooks.unlink(self.fd.as_fd(), &temporary.name) {
            Ok(()) => TemporaryCleanup::Removed,
            Err(Errno::NOENT) => TemporaryCleanup::Missing,
            Err(error) => TemporaryCleanup::Failed(path, error.to_string()),
        }
    }

    fn verify_binding(&self, guard: &LockGuard) -> Result<(), String> {
        let descriptor = descriptor_state(&self.fd, &self.path, "reinspect pages descriptor")?;
        let named = named_state(
            guard.corpus_fd(),
            PAGES_NAME,
            &self.path,
            "reinspect named pages directory",
        )?;
        if (descriptor.device, descriptor.inode) != (self.state.device, self.state.inode)
            || (named.device, named.inode) != (self.state.device, self.state.inode)
            || !descriptor.kind.is_dir()
            || !named.kind.is_dir()
        {
            return Err(format!(
                "wiki changed while validating pages: {}: descriptor identity changed",
                self.path.display()
            ));
        }
        Ok(())
    }
}

struct CapturedTarget {
    state: NodeState,
    bytes: Vec<u8>,
}

struct PageTemporary {
    fd: OwnedFd,
    name: OsString,
    identity: Option<(u64, u64)>,
    unlinked: bool,
}

enum InstallFailure {
    Collision,
    Before(String),
    After(String),
    CleanupFailed {
        original: Box<InstallFailure>,
        path: PathBuf,
        detail: String,
    },
    TemporaryRebound {
        original: Box<InstallFailure>,
        path: PathBuf,
    },
}

impl InstallFailure {
    fn page_visible(&self) -> bool {
        match self {
            Self::After(_) => true,
            Self::CleanupFailed { original, .. } | Self::TemporaryRebound { original, .. } => {
                original.page_visible()
            }
            Self::Collision | Self::Before(_) => false,
        }
    }

    fn describe(&self, slug: &str) -> String {
        match self {
            Self::Collision => format!("pages/{slug}.md already exists"),
            Self::Before(error) | Self::After(error) => error.clone(),
            Self::CleanupFailed {
                original,
                path,
                detail,
            } => format!(
                "{}; cleanup of {} also failed: {detail}",
                original.describe(slug),
                path.display()
            ),
            Self::TemporaryRebound { original, path } => format!(
                "{}; temporary path {} was rebound, so the foreign object was left untouched",
                original.describe(slug),
                path.display()
            ),
        }
    }
}

enum TemporaryCleanup {
    Removed,
    Missing,
    Rebound(PathBuf),
    Failed(PathBuf, String),
}

fn compose_cleanup(
    result: Result<(), InstallFailure>,
    cleanup: TemporaryCleanup,
) -> Result<(), InstallFailure> {
    match result {
        Ok(()) => Ok(()),
        Err(original) => match cleanup {
            TemporaryCleanup::Removed | TemporaryCleanup::Missing => Err(original),
            TemporaryCleanup::Rebound(path) => Err(InstallFailure::TemporaryRebound {
                original: Box::new(original),
                path,
            }),
            TemporaryCleanup::Failed(path, detail) => Err(InstallFailure::CleanupFailed {
                original: Box::new(original),
                path,
                detail,
            }),
        },
    }
}

fn write_all(
    hooks: &mut impl WriteHooks,
    fd: BorrowedFd<'_>,
    mut bytes: &[u8],
    path: &Path,
) -> Result<(), InstallFailure> {
    while !bytes.is_empty() {
        match hooks.write(fd, bytes) {
            Ok(0) => {
                return Err(InstallFailure::Before(format!(
                    "could not write page temporary {}: write returned zero bytes",
                    path.display()
                )));
            }
            Ok(written) => bytes = &bytes[written..],
            Err(Errno::INTR) => {}
            Err(error) => {
                return Err(InstallFailure::Before(io_error(
                    "write page temporary",
                    path,
                    error,
                )));
            }
        }
    }
    Ok(())
}

fn named_state(
    parent: BorrowedFd<'_>,
    name: impl rustix::path::Arg,
    path: &Path,
    operation: &'static str,
) -> Result<NodeState, String> {
    rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| NodeState::from_stat(&stat))
        .map_err(|error| io_error(operation, path, error))
}

fn descriptor_state(
    fd: impl AsFd,
    path: &Path,
    operation: &'static str,
) -> Result<NodeState, String> {
    rfs::fstat(fd)
        .map(|stat| NodeState::from_stat(&stat))
        .map_err(|error| io_error(operation, path, error))
}

fn timestamp_ns(seconds: i64, nanoseconds: i64) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds)
}

fn io_error(operation: &'static str, path: &Path, error: Errno) -> String {
    format!("could not {operation} {}: {error}", path.display())
}

fn diagnostic_name(bytes: &[u8]) -> String {
    let mut escaped = String::new();
    for byte in bytes {
        if (0x20..=0x7e).contains(byte) && *byte != b'\\' {
            escaped.push(char::from(*byte));
        } else {
            escaped.push_str(&format!("\\x{byte:02x}"));
        }
    }
    escaped
}

struct StrictValue(Value);

impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(StrictValueVisitor)
    }
}

struct StrictValueVisitor;

impl<'de> Visitor<'de> for StrictValueVisitor {
    type Value = StrictValue;

    fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Bool(value)))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Number(value.into())))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: serde::de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(Value::Number)
            .map(StrictValue)
            .ok_or_else(|| E::custom("non-finite number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value.to_owned())))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::String(value)))
    }

    fn visit_none<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(StrictValue(Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(StrictValue(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(StrictValue(Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut values = Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if values.contains_key(&key) {
                return Err(serde::de::Error::custom(format!(
                    "duplicate JSON key: {key}"
                )));
            }
            let StrictValue(value) = object.next_value()?;
            values.insert(key, value);
        }
        Ok(StrictValue(Value::Object(values)))
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc::{self, Receiver, Sender};
    use std::thread;
    use std::time::Duration;

    use tempfile::tempdir;

    use super::*;

    const PAGE: &str = "---\nslug: alpha\ntitle: Alpha\ntype: gotcha\nstatus: current\nowner: shared\nupdated: 2026-08-01\nverified: 2026-08-01\nsummary: summary\n---\n\nbody\n";

    fn isolated(corpus: &Path) -> LockGuard {
        match acquire_lock(corpus, LockMode::Exclusive).unwrap() {
            LockLease::Isolated(guard) => guard,
            LockLease::Unisolated(_) => panic!("writable fixture must be isolated"),
        }
    }

    #[test]
    fn replacing_the_named_pages_directory_invalidates_the_pinned_binding() {
        let fixture = tempdir().unwrap();
        fs::create_dir(fixture.path().join(PAGES_NAME)).unwrap();
        fs::write(fixture.path().join("pages/alpha.md"), PAGE).unwrap();
        fs::write(
            fixture.path().join(INDEX_NAME),
            format!("{}\n{}\n", crate::BEGIN_MARKER, crate::END_MARKER),
        )
        .unwrap();
        let guard = isolated(fixture.path());
        let pages = PinnedPages::open(&guard).unwrap();

        fs::rename(
            fixture.path().join(PAGES_NAME),
            fixture.path().join("detached-pages"),
        )
        .unwrap();
        fs::create_dir(fixture.path().join(PAGES_NAME)).unwrap();

        assert!(pages.verify_binding(&guard).is_err());
    }

    struct LinkWinner;

    impl WriteHooks for LinkWinner {
        fn before_create_link(&mut self, pages: &Path, target: &str) {
            fs::write(pages.join(target), b"peer won\n").unwrap();
        }
    }

    #[test]
    fn a_link_time_winner_is_never_overwritten_or_left_with_a_writer_temporary() {
        let fixture = tempdir().unwrap();
        fs::create_dir(fixture.path().join(PAGES_NAME)).unwrap();
        fs::write(fixture.path().join("pages/alpha.md"), PAGE).unwrap();
        let initial = crate::rebuild_index(
            &format!("{}\n{}\n", crate::BEGIN_MARKER, crate::END_MARKER),
            &[crate::IndexPage {
                slug: "alpha".to_owned(),
                page_type: crate::PageType::Gotcha,
                summary: "summary".to_owned(),
            }],
        )
        .unwrap();
        fs::write(fixture.path().join(INDEX_NAME), initial).unwrap();
        let input = serde_json::to_vec(&serde_json::json!({
            "title": "Peer Winner",
            "type": "gotcha",
            "owner": "codex",
            "fact": "fact",
            "why": "why",
            "how_to_apply": "how",
            "falsified_by": "counterexample",
            "summary": "summary",
            "related": [],
        }))
        .unwrap();

        let result = write_json_with_hooks(fixture.path(), &input, "2026-08-07", &mut LinkWinner);

        assert_eq!(result.exit_code, ExitCode::Usage, "{}", result.body);
        assert_eq!(
            fs::read(fixture.path().join("pages/peer-winner.md")).unwrap(),
            b"peer won\n"
        );
        assert!(
            fs::read_dir(fixture.path().join(PAGES_NAME))
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .contains("memory-write"))
        );
    }

    #[derive(Clone, Copy, Debug)]
    enum FaultPhase {
        Temporary,
        Write,
        FileFsync,
        Link,
        Unlink,
        DirectoryFsync,
        Rename,
    }

    struct FaultHooks {
        phase: FaultPhase,
        unlink_failed: bool,
    }

    impl FaultHooks {
        fn at(phase: FaultPhase) -> Self {
            Self {
                phase,
                unlink_failed: false,
            }
        }
    }

    impl WriteHooks for FaultHooks {
        fn open_temporary(
            &mut self,
            directory: BorrowedFd<'_>,
            name: &OsStr,
            mode: Mode,
        ) -> Result<OwnedFd, Errno> {
            if matches!(self.phase, FaultPhase::Temporary) {
                Err(Errno::IO)
            } else {
                rfs::openat(directory, name, TEMP_FLAGS, mode)
            }
        }

        fn write(&mut self, fd: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno> {
            if matches!(self.phase, FaultPhase::Write) {
                Err(Errno::IO)
            } else {
                rustix::io::write(fd, bytes)
            }
        }

        fn file_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            if matches!(self.phase, FaultPhase::FileFsync) {
                Err(Errno::IO)
            } else {
                rfs::fsync(fd)
            }
        }

        fn link(
            &mut self,
            directory: BorrowedFd<'_>,
            temporary: &OsStr,
            target: &str,
        ) -> Result<(), Errno> {
            if matches!(self.phase, FaultPhase::Link) {
                Err(Errno::IO)
            } else {
                rfs::linkat(directory, temporary, directory, target, AtFlags::empty())
            }
        }

        fn unlink(&mut self, directory: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
            if matches!(self.phase, FaultPhase::Unlink) && !self.unlink_failed {
                self.unlink_failed = true;
                Err(Errno::IO)
            } else {
                rfs::unlinkat(directory, name, AtFlags::empty())
            }
        }

        fn rename(
            &mut self,
            directory: BorrowedFd<'_>,
            temporary: &OsStr,
            target: &str,
        ) -> Result<(), Errno> {
            if matches!(self.phase, FaultPhase::Rename) {
                Err(Errno::IO)
            } else {
                rfs::renameat(directory, temporary, directory, target)
            }
        }

        fn directory_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            if matches!(self.phase, FaultPhase::DirectoryFsync) {
                Err(Errno::IO)
            } else {
                rfs::fsync(fd)
            }
        }
    }

    fn fixture_with_canonical_index() -> tempfile::TempDir {
        let fixture = tempdir().unwrap();
        fs::create_dir(fixture.path().join(PAGES_NAME)).unwrap();
        fs::write(fixture.path().join("pages/alpha.md"), PAGE).unwrap();
        let initial = crate::rebuild_index(
            &format!("{}\n{}\n", crate::BEGIN_MARKER, crate::END_MARKER),
            &[crate::IndexPage {
                slug: "alpha".to_owned(),
                page_type: crate::PageType::Gotcha,
                summary: "summary".to_owned(),
            }],
        )
        .unwrap();
        fs::write(fixture.path().join(INDEX_NAME), initial).unwrap();
        fixture
    }

    fn create_input() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "title": "Fault Probe",
            "type": "gotcha",
            "owner": "codex",
            "fact": "fact",
            "why": "why",
            "how_to_apply": "how",
            "falsified_by": "counterexample",
            "summary": "fault summary",
            "related": [],
        }))
        .unwrap()
    }

    struct ImmediateLockProbe {
        attempts: usize,
    }

    impl WriteHooks for ImmediateLockProbe {
        fn acquire_lock(
            &mut self,
            corpus: &Path,
            mode: LockMode,
        ) -> Result<LockLease, crate::LockError> {
            self.attempts += 1;
            crate::acquire_lock_with_timeout(corpus, mode, Duration::ZERO)
        }
    }

    fn preflight_cases() -> Vec<(&'static str, Vec<u8>, &'static str, ExitCode)> {
        let create: Value = serde_json::from_slice(&create_input()).unwrap();
        let update = serde_json::json!({
            "title": "Alpha",
            "type": "gotcha",
            "fact": "changed",
            "why": "why",
            "how_to_apply": "how",
            "falsified_by": "counterexample",
            "summary": "summary",
            "related": [],
            "update": true,
            "target": "alpha",
        });
        let encode = |value: Value| serde_json::to_vec(&value).unwrap();
        let mut cases = vec![
            (
                "stdin cap",
                vec![b' '; MAX_FILE_BYTES as usize + 1],
                "2026-08-07",
                ExitCode::Usage,
            ),
            ("invalid UTF-8", vec![0xff], "2026-08-07", ExitCode::Usage),
            (
                "malformed JSON",
                b"{not json".to_vec(),
                "2026-08-07",
                ExitCode::Usage,
            ),
            (
                "non-object JSON",
                b"[]".to_vec(),
                "2026-08-07",
                ExitCode::Usage,
            ),
            (
                "duplicate JSON key",
                br#"{"title":"first","title":"second"}"#.to_vec(),
                "2026-08-07",
                ExitCode::Usage,
            ),
            (
                "trailing JSON value",
                b"{} true".to_vec(),
                "2026-08-07",
                ExitCode::Usage,
            ),
        ];
        let mut push_create = |label, field: &str, value: Option<Value>| {
            let mut request = create.clone();
            if let Some(value) = value {
                request[field] = value;
            } else {
                request.as_object_mut().unwrap().remove(field);
            }
            cases.push((label, encode(request), "2026-08-07", ExitCode::Usage));
        };
        push_create("unknown field", "unexpected", Some(json!(true)));
        push_create("missing required field", "fact", None);
        push_create("invalid page type", "type", Some(json!("note")));
        push_create("invalid owner", "owner", Some(json!("nobody")));
        push_create(
            "frontmatter scalar hazard",
            "title",
            Some(json!("bad\nvalue")),
        );
        push_create(
            "frontmatter fence hazard",
            "summary",
            Some(json!("bad ``` value")),
        );
        push_create(
            "frontmatter quote-only hazard",
            "summary",
            Some(json!("'''")),
        );
        push_create("empty slug", "title", Some(json!("日本語")));
        push_create("invalid related shape", "related", Some(json!(7)));
        push_create(
            "invalid related slug",
            "related",
            Some(json!(["Not-A-Slug"])),
        );
        push_create("self relation", "related", Some(json!(["fault-probe"])));
        push_create(
            "drifting line reference",
            "why",
            Some(json!("module.py:123")),
        );
        for (label, update_flag) in [
            ("truthy update routing", json!(1)),
            ("falsey create routing", json!(false)),
        ] {
            let mut routed = create.clone();
            routed["update"] = update_flag;
            cases.push((label, encode(routed), "2026-08-07", ExitCode::Usage));
        }

        let mut push_update = |label, field: &str, value: Option<Value>| {
            let mut request = update.clone();
            if let Some(value) = value {
                request[field] = value;
            } else {
                request.as_object_mut().unwrap().remove(field);
            }
            cases.push((label, encode(request), "2026-08-07", ExitCode::Usage));
        };
        push_update("update refused owner", "owner", Some(json!("shared")));
        push_update("update refused status", "status", Some(json!("current")));
        push_update("update missing required field", "fact", None);
        push_update("update invalid page type", "type", Some(json!("note")));
        push_update(
            "update scalar hazard",
            "summary",
            Some(json!("bad ``` value")),
        );
        push_update("update related required", "related", Some(Value::Null));
        push_update("update flag must be true", "update", Some(json!(false)));
        push_update(
            "invalid expected digest",
            "expected_sha256",
            Some(json!("abc")),
        );
        push_update("update rename guard", "title", Some(json!("Renamed")));
        cases.push((
            "invalid injected today",
            create_input(),
            "August 7",
            ExitCode::Operational,
        ));
        cases
    }

    #[test]
    fn every_preflight_refusal_precedes_both_a_missing_and_busy_corpus() {
        let fixture = fixture_with_canonical_index();
        let holder = isolated(fixture.path());
        let missing_root = tempdir().unwrap();
        let missing = missing_root.path().join("missing-corpus");

        for (label, input, today, exit_code) in preflight_cases() {
            let mut missing_probe = ImmediateLockProbe { attempts: 0 };
            let missing_result = write_json_with_hooks(&missing, &input, today, &mut missing_probe);
            assert_eq!(missing_result.exit_code, exit_code, "missing {label}");
            assert_eq!(missing_probe.attempts, 0, "missing {label}");
            assert!(
                missing_result.body.get("paths").is_none(),
                "missing {label}"
            );
            assert!(!missing.exists(), "missing {label}");

            let mut busy_probe = ImmediateLockProbe { attempts: 0 };
            let busy_result = write_json_with_hooks(fixture.path(), &input, today, &mut busy_probe);
            assert_eq!(busy_probe.attempts, 0, "busy {label}");
            assert_eq!(busy_result, missing_result, "busy {label}");
        }

        let mut valid_probe = ImmediateLockProbe { attempts: 0 };
        let valid = write_json_with_hooks(
            fixture.path(),
            &create_input(),
            "2026-08-07",
            &mut valid_probe,
        );
        assert_eq!(valid_probe.attempts, 1);
        assert_eq!(valid.exit_code, ExitCode::Operational, "{}", valid.body);
        assert!(
            valid.body["error"]
                .as_str()
                .unwrap()
                .contains("stayed busy")
        );
        drop(holder);
    }

    struct FirstForcedWriter {
        held: Sender<()>,
        second_attempted: Receiver<()>,
        second_entered: Receiver<()>,
        overlapped: bool,
    }

    impl WriteHooks for FirstForcedWriter {
        fn after_forward_page_enumeration(&mut self, _pages: &Path) {
            self.held.send(()).unwrap();
            self.second_attempted.recv().unwrap();
            self.overlapped = self
                .second_entered
                .recv_timeout(Duration::from_millis(250))
                .is_ok();
        }
    }

    struct SecondForcedWriter {
        attempted: Sender<()>,
        entered: Sender<()>,
    }

    impl WriteHooks for SecondForcedWriter {
        fn acquire_lock(
            &mut self,
            corpus: &Path,
            mode: LockMode,
        ) -> Result<LockLease, crate::LockError> {
            self.attempted.send(()).unwrap();
            acquire_lock(corpus, mode)
        }

        fn after_forward_page_enumeration(&mut self, _pages: &Path) {
            let _ = self.entered.send(());
        }
    }

    fn force_two_writers(second_input: Vec<u8>) -> (WriteResult, WriteResult, bool) {
        let fixture = fixture_with_canonical_index();
        let corpus = fixture.path().to_path_buf();
        let (held_tx, held_rx) = mpsc::channel();
        let (attempted_tx, attempted_rx) = mpsc::channel();
        let (entered_tx, entered_rx) = mpsc::channel();
        let first_corpus = corpus.clone();
        let first = thread::spawn(move || {
            let mut hooks = FirstForcedWriter {
                held: held_tx,
                second_attempted: attempted_rx,
                second_entered: entered_rx,
                overlapped: false,
            };
            let result =
                write_json_with_hooks(&first_corpus, &create_input(), "2026-08-07", &mut hooks);
            (result, hooks.overlapped)
        });
        held_rx.recv().unwrap();

        let second_corpus = corpus.clone();
        let second = thread::spawn(move || {
            write_json_with_hooks(
                &second_corpus,
                &second_input,
                "2026-08-07",
                &mut SecondForcedWriter {
                    attempted: attempted_tx,
                    entered: entered_tx,
                },
            )
        });
        let (first_result, overlapped) = first.join().unwrap();
        let second_result = second.join().unwrap();
        (first_result, second_result, overlapped)
    }

    #[test]
    fn forced_same_and_different_slug_writers_never_overlap_the_mutation_window() {
        let (first, second, overlapped) = force_two_writers(create_input());
        assert!(!overlapped, "same-slug writers held the lock concurrently");
        assert_eq!(first.exit_code, ExitCode::Ok, "{}", first.body);
        assert_eq!(second.exit_code, ExitCode::Usage, "{}", second.body);

        let mut different: Value = serde_json::from_slice(&create_input()).unwrap();
        different["title"] = json!("Different Fault Probe");
        let (first, second, overlapped) =
            force_two_writers(serde_json::to_vec(&different).unwrap());
        assert!(
            !overlapped,
            "different-slug writers held the lock concurrently"
        );
        assert_eq!(first.exit_code, ExitCode::Ok, "{}", first.body);
        assert_eq!(second.exit_code, ExitCode::Ok, "{}", second.body);
    }

    #[derive(Default)]
    struct UpdateTemporaryModeProbe {
        requested: Option<u32>,
        named_before_write: Option<u32>,
        descriptor_at_first_write: Option<u32>,
        requested_target_mode: Option<u32>,
        descriptor_at_file_fsync: Option<u32>,
    }

    impl WriteHooks for UpdateTemporaryModeProbe {
        fn open_temporary(
            &mut self,
            directory: BorrowedFd<'_>,
            name: &OsStr,
            mode: Mode,
        ) -> Result<OwnedFd, Errno> {
            self.requested = Some(mode.bits() as u32 & 0o777);
            rfs::openat(directory, name, TEMP_FLAGS, mode)
        }

        fn after_page_temp_created(&mut self, pages: &Path, name: &OsStr) {
            self.named_before_write = Some(
                fs::symlink_metadata(pages.join(name))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
            );
        }

        fn write(&mut self, fd: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno> {
            if self.descriptor_at_first_write.is_none() {
                self.descriptor_at_first_write = Some(rfs::fstat(fd)?.st_mode as u32 & 0o777);
            }
            rustix::io::write(fd, bytes)
        }

        fn chmod(&mut self, fd: BorrowedFd<'_>, mode: Mode) -> Result<(), Errno> {
            self.requested_target_mode = Some(mode.bits() as u32 & 0o777);
            rfs::fchmod(fd, mode)
        }

        fn file_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            self.descriptor_at_file_fsync = Some(rfs::fstat(fd)?.st_mode as u32 & 0o777);
            rfs::fsync(fd)
        }
    }

    fn alpha_update_input() -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "title": "Alpha",
            "type": "gotcha",
            "fact": "changed",
            "why": "why",
            "how_to_apply": "how",
            "falsified_by": "counterexample",
            "summary": "summary",
            "related": [],
            "update": true,
            "target": "alpha",
        }))
        .unwrap()
    }

    #[test]
    fn update_temporary_is_private_before_its_first_write() {
        let fixture = fixture_with_canonical_index();
        fs::set_permissions(
            fixture.path().join("pages/alpha.md"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        let mut probe = UpdateTemporaryModeProbe::default();

        let result = write_json_with_hooks(
            fixture.path(),
            &alpha_update_input(),
            "2026-08-07",
            &mut probe,
        );

        assert_eq!(result.exit_code, ExitCode::Ok, "{}", result.body);
        assert_eq!(probe.requested, Some(0o600));
        assert_eq!(probe.named_before_write, Some(0o600));
        assert_eq!(probe.descriptor_at_first_write, Some(0o600));
        assert_eq!(probe.requested_target_mode, Some(0o600));
        assert_eq!(probe.descriptor_at_file_fsync, Some(0o600));
        assert_eq!(
            fs::metadata(fixture.path().join("pages/alpha.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }

    #[test]
    fn update_applies_a_broader_target_mode_only_after_writing_and_before_fsync() {
        let fixture = fixture_with_canonical_index();
        fs::set_permissions(
            fixture.path().join("pages/alpha.md"),
            fs::Permissions::from_mode(0o640),
        )
        .unwrap();
        let mut probe = UpdateTemporaryModeProbe::default();

        let result = write_json_with_hooks(
            fixture.path(),
            &alpha_update_input(),
            "2026-08-07",
            &mut probe,
        );

        assert_eq!(result.exit_code, ExitCode::Ok, "{}", result.body);
        assert_eq!(probe.requested, Some(0o600));
        assert_eq!(probe.named_before_write, Some(0o600));
        assert_eq!(probe.descriptor_at_first_write, Some(0o600));
        assert_eq!(probe.requested_target_mode, Some(0o640));
        assert_eq!(probe.descriptor_at_file_fsync, Some(0o640));
        assert_eq!(
            fs::metadata(fixture.path().join("pages/alpha.md"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o640
        );
    }

    #[derive(Default)]
    struct PersistentCleanupNamedFailure {
        temporary: Option<OsString>,
    }

    impl WriteHooks for PersistentCleanupNamedFailure {
        fn after_page_temp_created(&mut self, _pages: &Path, name: &OsStr) {
            self.temporary = Some(name.to_owned());
        }

        fn write(&mut self, _fd: BorrowedFd<'_>, _bytes: &[u8]) -> Result<usize, Errno> {
            Err(Errno::IO)
        }

        fn cleanup_temporary_named_state(
            &mut self,
            _directory: BorrowedFd<'_>,
            _name: &OsStr,
        ) -> Result<NodeState, Errno> {
            Err(Errno::IO)
        }
    }

    #[test]
    fn prepublication_failure_reports_persistent_cleanup_stat_failure() {
        let fixture = fixture_with_canonical_index();
        let mut hooks = PersistentCleanupNamedFailure::default();

        let result =
            write_json_with_hooks(fixture.path(), &create_input(), "2026-08-07", &mut hooks);

        let canonical = fixture.path().canonicalize().unwrap();
        let temporary = canonical
            .join(PAGES_NAME)
            .join(hooks.temporary.as_ref().unwrap());
        let page = canonical.join("pages/fault-probe.md");
        let detail = Errno::IO.to_string();
        assert_eq!(
            result.body,
            json!({
                "ok": false,
                "exit": 4,
                "error": format!(
                    "could not write page temporary {}: {detail}; cleanup of {} also failed: {detail}",
                    page.display(),
                    temporary.display(),
                ),
                "hint": "make the memory directory writable and retry",
            })
        );
        assert_eq!(result.exit_code, ExitCode::Operational);
        assert!(temporary.exists());
        assert!(!page.exists());
    }

    #[derive(Default)]
    struct PersistentCleanupUnlinkFailure {
        temporary: Option<OsString>,
    }

    impl WriteHooks for PersistentCleanupUnlinkFailure {
        fn after_page_temp_created(&mut self, _pages: &Path, name: &OsStr) {
            self.temporary = Some(name.to_owned());
        }

        fn unlink(&mut self, _directory: BorrowedFd<'_>, _name: &OsStr) -> Result<(), Errno> {
            Err(Errno::IO)
        }
    }

    #[test]
    fn landed_page_reports_persistent_temporary_unlink_failure() {
        let fixture = fixture_with_canonical_index();
        let page = fixture.path().join("pages/fault-probe.md");
        let mut hooks = PersistentCleanupUnlinkFailure::default();

        let result =
            write_json_with_hooks(fixture.path(), &create_input(), "2026-08-07", &mut hooks);

        let temporary = fixture
            .path()
            .canonicalize()
            .unwrap()
            .join(PAGES_NAME)
            .join(hooks.temporary.as_ref().unwrap());
        let detail = Errno::IO.to_string();
        let primary = format!(
            "could not unlink page temporary {}: {detail}",
            temporary.display()
        );
        assert_eq!(
            result.body,
            json!({
                "ok": false,
                "exit": 4,
                "error": format!(
                    "{primary}; cleanup of {} also failed: {detail}",
                    temporary.display(),
                ),
                "hint": "confirm the page, then run yams-wiki catalog",
                "slug": "fault-probe",
                "paths": [page],
                "index_regenerated": false,
            })
        );
        assert_eq!(result.exit_code, ExitCode::Operational);
        assert!(page.exists());
        assert!(temporary.exists());
    }

    fn writer_temporaries(corpus: &Path) -> Vec<OsString> {
        fs::read_dir(corpus.join(PAGES_NAME))
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.file_name())
            .filter(|name| name.to_string_lossy().contains("memory-write"))
            .collect()
    }

    #[test]
    fn every_prepublication_page_fault_is_operational_without_paths_or_residue() {
        for phase in [
            FaultPhase::Temporary,
            FaultPhase::Write,
            FaultPhase::FileFsync,
            FaultPhase::Link,
        ] {
            let fixture = fixture_with_canonical_index();
            let result = write_json_with_hooks(
                fixture.path(),
                &create_input(),
                "2026-08-07",
                &mut FaultHooks::at(phase),
            );
            assert_eq!(
                result.exit_code,
                ExitCode::Operational,
                "{phase:?}: {}",
                result.body
            );
            assert!(
                result.body.get("paths").is_none(),
                "{phase:?}: {}",
                result.body
            );
            assert!(!fixture.path().join("pages/fault-probe.md").exists());
            assert!(writer_temporaries(fixture.path()).is_empty(), "{phase:?}");
        }
    }

    #[test]
    fn postpublication_unlink_and_directory_fsync_faults_report_the_visible_page() {
        for phase in [FaultPhase::Unlink, FaultPhase::DirectoryFsync] {
            let fixture = fixture_with_canonical_index();
            let page = fixture.path().join("pages/fault-probe.md");
            let result = write_json_with_hooks(
                fixture.path(),
                &create_input(),
                "2026-08-07",
                &mut FaultHooks::at(phase),
            );
            assert_eq!(
                result.exit_code,
                ExitCode::Operational,
                "{phase:?}: {}",
                result.body
            );
            assert_eq!(result.body["paths"], serde_json::json!([page]));
            assert_eq!(result.body["index_regenerated"], false);
            assert!(page.exists(), "{phase:?}");
            assert!(writer_temporaries(fixture.path()).is_empty(), "{phase:?}");
        }
    }

    #[test]
    fn update_rename_fault_preserves_the_original_and_leaves_no_residue() {
        let fixture = fixture_with_canonical_index();
        let page = fixture.path().join("pages/alpha.md");
        let before = fs::read(&page).unwrap();
        let input = serde_json::to_vec(&serde_json::json!({
            "title": "Alpha",
            "type": "gotcha",
            "fact": "changed",
            "why": "why",
            "how_to_apply": "how",
            "falsified_by": "counterexample",
            "summary": "summary",
            "related": [],
            "update": true,
            "target": "alpha",
        }))
        .unwrap();

        let result = write_json_with_hooks(
            fixture.path(),
            &input,
            "2026-08-07",
            &mut FaultHooks::at(FaultPhase::Rename),
        );

        assert_eq!(result.exit_code, ExitCode::Operational, "{}", result.body);
        assert!(result.body.get("paths").is_none());
        assert_eq!(fs::read(page).unwrap(), before);
        assert!(writer_temporaries(fixture.path()).is_empty());
    }

    enum UpdateRace {
        RestoredMtime,
        TransientHardLink,
    }

    struct UpdateRaceHooks(UpdateRace);

    impl WriteHooks for UpdateRaceHooks {
        fn before_update_commit(&mut self, pages: &Path, target: &str) {
            let page = pages.join(target);
            match self.0 {
                UpdateRace::RestoredMtime => {
                    let before = rfs::stat(&page).unwrap();
                    let bytes = fs::read(&page).unwrap();
                    let replacement = String::from_utf8(bytes).unwrap().replace("body", "B0dy");
                    fs::write(&page, replacement).unwrap();
                    rfs::utimensat(
                        rfs::CWD,
                        &page,
                        &rfs::Timestamps {
                            last_access: rfs::Timespec {
                                tv_sec: before.st_atime,
                                tv_nsec: before.st_atime_nsec as i64,
                            },
                            last_modification: rfs::Timespec {
                                tv_sec: before.st_mtime,
                                tv_nsec: before.st_mtime_nsec as i64,
                            },
                        },
                        AtFlags::empty(),
                    )
                    .unwrap();
                }
                UpdateRace::TransientHardLink => {
                    let alias = pages.join("transient-alias");
                    fs::hard_link(&page, &alias).unwrap();
                    fs::remove_file(alias).unwrap();
                }
            }
        }
    }

    #[test]
    fn restored_mtime_rewrite_and_transient_hardlink_abort_update_before_replacement() {
        for race in [UpdateRace::RestoredMtime, UpdateRace::TransientHardLink] {
            let fixture = fixture_with_canonical_index();
            let page = fixture.path().join("pages/alpha.md");
            let input = serde_json::to_vec(&serde_json::json!({
                "title": "Alpha",
                "type": "gotcha",
                "fact": "changed",
                "why": "why",
                "how_to_apply": "how",
                "falsified_by": "counterexample",
                "summary": "summary",
                "related": [],
                "update": true,
                "target": "alpha",
            }))
            .unwrap();

            let result = write_json_with_hooks(
                fixture.path(),
                &input,
                "2026-08-07",
                &mut UpdateRaceHooks(race),
            );

            assert_eq!(result.exit_code, ExitCode::Operational, "{}", result.body);
            assert!(result.body.get("paths").is_none());
            assert!(
                !fs::read_to_string(&page)
                    .unwrap()
                    .contains("\nchanged\n\n**Why:**")
            );
            assert!(writer_temporaries(fixture.path()).is_empty());
        }
    }

    #[derive(Clone, Copy, Debug)]
    enum IndexFault {
        Iteration,
        Temporary,
        TemporaryDescriptor,
        TemporaryNamed,
        Write,
        Chmod,
        FileFsync,
        FinalValidation,
        BeforeRenameValidation,
        Rename,
        Cleanup,
        DirectoryFsync,
    }

    struct IndexFaultHooks(IndexFault);

    impl crate::durable::DurableHooks for IndexFaultHooks {
        fn fail_directory_iteration(&mut self, _path: &Path, processed: usize) -> bool {
            matches!(self.0, IndexFault::Iteration) && processed == 1
        }

        fn temporary_name(&mut self, _pid: u32, _sequence: u64) -> OsString {
            if matches!(self.0, IndexFault::Temporary) {
                OsString::from(".")
            } else {
                OsString::from(".injected-index.tmp")
            }
        }

        fn write(&mut self, fd: BorrowedFd<'_>, bytes: &[u8]) -> Result<usize, Errno> {
            if matches!(self.0, IndexFault::Write) {
                Err(Errno::IO)
            } else {
                rustix::io::write(fd, bytes)
            }
        }

        fn chmod(&mut self, fd: BorrowedFd<'_>, mode: Mode) -> Result<(), Errno> {
            if matches!(self.0, IndexFault::Chmod) {
                Err(Errno::IO)
            } else {
                rfs::fchmod(fd, mode)
            }
        }

        fn file_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            if matches!(self.0, IndexFault::FileFsync) {
                Err(Errno::IO)
            } else {
                rfs::fsync(fd)
            }
        }

        fn before_final_validation(&mut self, corpus: &Path) {
            if matches!(self.0, IndexFault::FinalValidation) {
                fs::write(
                    corpus.join("pages/beta.md"),
                    PAGE.replace("slug: alpha", "slug: beta")
                        .replace("title: Alpha", "title: Beta"),
                )
                .unwrap();
            }
        }

        fn before_rename(&mut self, corpus: &Path, _name: &OsStr) {
            if matches!(self.0, IndexFault::BeforeRenameValidation) {
                fs::write(
                    corpus.join("pages/beta.md"),
                    PAGE.replace("slug: alpha", "slug: beta")
                        .replace("title: Alpha", "title: Beta"),
                )
                .unwrap();
            }
        }

        fn rename(&mut self, directory: BorrowedFd<'_>, temporary: &OsStr) -> Result<(), Errno> {
            if matches!(self.0, IndexFault::Rename | IndexFault::Cleanup) {
                Err(Errno::IO)
            } else {
                rfs::renameat(directory, temporary, directory, INDEX_NAME)
            }
        }

        fn directory_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
            if matches!(self.0, IndexFault::DirectoryFsync) {
                Err(Errno::IO)
            } else {
                rfs::fsync(fd)
            }
        }

        fn unlink(&mut self, directory: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
            if matches!(self.0, IndexFault::Cleanup) {
                Err(Errno::IO)
            } else {
                rfs::unlinkat(directory, name, AtFlags::empty())
            }
        }

        fn temporary_descriptor_state(
            &mut self,
            fd: BorrowedFd<'_>,
            path: &Path,
            operation: &'static str,
        ) -> Result<crate::durable::NodeState, DurableError> {
            if matches!(self.0, IndexFault::TemporaryDescriptor) {
                Err(DurableError::Io {
                    operation,
                    path: path.to_path_buf(),
                    detail: Errno::IO.to_string(),
                })
            } else {
                rfs::fstat(fd)
                    .map(|stat| crate::durable::NodeState::from_stat(&stat))
                    .map_err(|error| DurableError::Io {
                        operation,
                        path: path.to_path_buf(),
                        detail: error.to_string(),
                    })
            }
        }

        fn temporary_named_state(
            &mut self,
            directory: BorrowedFd<'_>,
            name: &OsStr,
            path: &Path,
            operation: &'static str,
        ) -> Result<crate::durable::NodeState, DurableError> {
            if matches!(self.0, IndexFault::TemporaryNamed) {
                Err(DurableError::Io {
                    operation,
                    path: path.to_path_buf(),
                    detail: Errno::IO.to_string(),
                })
            } else {
                rfs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map(|stat| crate::durable::NodeState::from_stat(&stat))
                    .map_err(|error| DurableError::Io {
                        operation,
                        path: path.to_path_buf(),
                        detail: error.to_string(),
                    })
            }
        }
    }

    impl WriteHooks for IndexFaultHooks {
        fn reindex(
            &mut self,
            guard: &LockGuard,
            options: &ReindexOptions,
        ) -> Result<crate::ReindexResult, DurableError> {
            crate::durable::reindex_locked_with_hooks(guard, options, self)
        }
    }

    #[test]
    fn durable_page_followed_by_each_index_failure_reports_only_visible_replacements() {
        for phase in [
            IndexFault::Iteration,
            IndexFault::Temporary,
            IndexFault::TemporaryDescriptor,
            IndexFault::TemporaryNamed,
            IndexFault::Write,
            IndexFault::Chmod,
            IndexFault::FileFsync,
            IndexFault::FinalValidation,
            IndexFault::BeforeRenameValidation,
            IndexFault::Rename,
            IndexFault::Cleanup,
            IndexFault::DirectoryFsync,
        ] {
            let fixture = fixture_with_canonical_index();
            let page = fixture.path().join("pages/fault-probe.md");
            let index = fixture.path().join(INDEX_NAME);
            let before_index = fs::read(&index).unwrap();
            let result = write_json_with_hooks(
                fixture.path(),
                &create_input(),
                "2026-08-07",
                &mut IndexFaultHooks(phase),
            );

            assert_eq!(
                result.exit_code,
                ExitCode::Operational,
                "{phase:?}: {}",
                result.body
            );
            assert!(page.exists(), "{phase:?}");
            if matches!(phase, IndexFault::DirectoryFsync) {
                assert_eq!(result.body["paths"], serde_json::json!([page, index]));
                assert_eq!(result.body["index_regenerated"], true);
                assert_ne!(fs::read(&index).unwrap(), before_index);
            } else {
                assert_eq!(result.body["paths"], serde_json::json!([page]));
                assert_eq!(result.body["index_regenerated"], false);
                assert_eq!(fs::read(&index).unwrap(), before_index, "{phase:?}");
            }
            let temporaries = fs::read_dir(fixture.path())
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().to_string_lossy().contains("index.tmp"))
                .count();
            assert_eq!(
                temporaries,
                usize::from(matches!(phase, IndexFault::Cleanup)),
                "{phase:?}"
            );
        }
    }

    struct RemovePagesAfterIndex;

    impl WriteHooks for RemovePagesAfterIndex {
        fn after_reindex(&mut self, corpus: &Path) {
            fs::rename(corpus.join(PAGES_NAME), corpus.join("pages-after-index")).unwrap();
        }
    }

    #[test]
    fn response_uses_the_under_lock_page_set_without_a_fresh_fallible_scan() {
        let fixture = fixture_with_canonical_index();
        let result = write_json_with_hooks(
            fixture.path(),
            &create_input(),
            "2026-08-07",
            &mut RemovePagesAfterIndex,
        );

        assert_eq!(result.exit_code, ExitCode::Ok, "{}", result.body);
        assert_eq!(result.body["forward_refs"], serde_json::json!([]));
        assert_eq!(result.body["index_regenerated"], true);
    }

    #[derive(Default)]
    struct RebindPageTemporary {
        temporary: Option<OsString>,
    }

    impl WriteHooks for RebindPageTemporary {
        fn after_page_temp_created(&mut self, pages: &Path, name: &OsStr) {
            self.temporary = Some(name.to_owned());
            fs::rename(pages.join(name), pages.join("created-temp-aside")).unwrap();
            fs::write(pages.join(name), b"attacker replacement\n").unwrap();
        }
    }

    #[test]
    fn rebound_page_temporary_is_not_unlinked_or_published() {
        let fixture = fixture_with_canonical_index();
        let mut hooks = RebindPageTemporary::default();
        let result =
            write_json_with_hooks(fixture.path(), &create_input(), "2026-08-07", &mut hooks);

        let temporary = fixture
            .path()
            .canonicalize()
            .unwrap()
            .join(PAGES_NAME)
            .join(hooks.temporary.as_ref().unwrap());
        assert_eq!(result.exit_code, ExitCode::Operational, "{}", result.body);
        assert!(result.body.get("paths").is_none());
        assert_eq!(
            result.body["error"],
            format!(
                "wiki changed while creating page temporary: {}: name was rebound; temporary path {} was rebound, so the foreign object was left untouched",
                temporary.display(),
                temporary.display(),
            )
        );
        assert!(!fixture.path().join("pages/fault-probe.md").exists());
        assert_eq!(fs::read(temporary).unwrap(), b"attacker replacement\n");
    }

    struct ChangeForwardPageSet;

    impl WriteHooks for ChangeForwardPageSet {
        fn after_forward_page_enumeration(&mut self, pages: &Path) {
            fs::write(
                pages.join("beta.md"),
                PAGE.replace("slug: alpha", "slug: beta")
                    .replace("title: Alpha", "title: Beta"),
            )
            .unwrap();
        }
    }

    #[test]
    fn forward_reference_page_set_is_bracketed_before_page_install() {
        let fixture = fixture_with_canonical_index();
        let result = write_json_with_hooks(
            fixture.path(),
            &create_input(),
            "2026-08-07",
            &mut ChangeForwardPageSet,
        );

        assert_eq!(result.exit_code, ExitCode::Operational, "{}", result.body);
        assert!(result.body.get("paths").is_none());
        assert!(!fixture.path().join("pages/fault-probe.md").exists());
    }
}
