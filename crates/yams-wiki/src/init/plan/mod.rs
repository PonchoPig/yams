use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering as AtomicOrdering};
use std::time::{SystemTime, UNIX_EPOCH};

use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use super::inspect::capture_repository;
use super::{
    AGENT_POLICY, INDEX_TEMPLATE, InitError, InitManifest, InitMode, InitOperation,
    InitPlanRequest, LAYOUT_VERSION, ManifestEnvelope, NodeKind, NodePrestate, OperationKind,
    PAGE_TEMPLATE, SCHEMA, inspect_policy, sha256,
};
use crate::check::compat_snapshot;
use crate::{
    CapturedPage, CapturedPageOutcome, CreateRequest, Owner, WikiSnapshot, parse_index_page,
    parse_wiki_page, rebuild_index, render_create, validate_wiki,
};

const DIRECTORY_MODE: u32 = 0o755;
const FILE_MODE: u32 = 0o644;
const CLEANUP_DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const CANDIDATE_NAME_ATTEMPTS: u32 = 64;
static CANDIDATE_NAME_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum DesiredNode {
    Directory { mode: u32 },
    File { mode: u32, bytes: Vec<u8> },
}

impl DesiredNode {
    const fn mode(&self) -> u32 {
        match self {
            Self::Directory { mode } | Self::File { mode, .. } => *mode,
        }
    }
}

pub fn canonical_manifest_bytes(manifest: &InitManifest) -> Result<Vec<u8>, InitError> {
    serde_json::to_vec(manifest).map_err(InitError::from)
}

fn resolve_agents_md(
    request: &InitPlanRequest,
    existing: Option<&[u8]>,
) -> Result<String, InitError> {
    if !request.agents_md.is_empty() {
        return Ok(request.agents_md.clone());
    }
    match existing {
        None => Ok(AGENT_POLICY.to_owned()),
        Some(bytes) => {
            let text = std::str::from_utf8(bytes).map_err(|_| {
                InitError::InvalidRequest(
                    "omit agents_md only when AGENTS.md is missing or already contains the canonical Project memory section"
                        .to_owned(),
                )
            })?;
            let policy = inspect_policy(text);
            if policy.heading_count == 1 && policy.exact {
                Ok(text.to_owned())
            } else {
                Err(InitError::InvalidRequest(
                    "omit agents_md only when AGENTS.md is missing or already contains the canonical Project memory section; otherwise supply the exact desired AGENTS.md"
                        .to_owned(),
                ))
            }
        }
    }
}

/// Bind `root` and `inspection_sha256` from an inspect result.
///
/// When `mode` is omitted, use `inspection.recommended_mode`. That fails when
/// inspection reports no recommended mode.
pub fn plan_request_from_inspection(
    inspection: &super::InitInspection,
    mode: Option<InitMode>,
    date: String,
    project_page: super::ProjectPageRequest,
    agents_md: String,
) -> Result<InitPlanRequest, InitError> {
    let mode = match mode {
        Some(mode) => mode,
        None => inspection.recommended_mode.ok_or_else(|| {
            InitError::InvalidRequest("mode is required when recommended_mode is null".to_owned())
        })?,
    };
    Ok(InitPlanRequest {
        root: inspection.root.clone(),
        inspection_sha256: inspection.inspection_sha256.clone(),
        mode,
        date,
        agents_md,
        project_page,
    })
}

pub fn plan_repository(request: &InitPlanRequest) -> Result<ManifestEnvelope, InitError> {
    let snapshot = capture_repository(Path::new(&request.root))?;
    let inspection = &snapshot.inspection;
    if request.root != inspection.root {
        return Err(InitError::InvalidRequest(
            "root must be the canonical absolute repository path returned by inspection".to_owned(),
        ));
    }
    if request.inspection_sha256 != inspection.inspection_sha256 {
        return Err(InitError::Drift(format!(
            "inspection digest changed from {} to {}",
            request.inspection_sha256, inspection.inspection_sha256
        )));
    }
    if !inspection.dirty_paths.is_empty() {
        return Err(InitError::Conflict(format!(
            "owned paths have uncommitted changes: {}",
            inspection.dirty_paths.join(", ")
        )));
    }
    if !inspection.conflicts.is_empty() {
        let conflicts = inspection
            .conflicts
            .iter()
            .map(|conflict| format!("{} ({})", conflict.path, conflict.code))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(InitError::Conflict(format!(
            "inspection reported conflicts: {conflicts}"
        )));
    }
    if !inspection.attainable.contains(&request.mode) {
        return Err(InitError::Conflict(format!(
            "{} initialization is not attainable from the inspected layout",
            mode_name(request.mode)
        )));
    }
    let mut request = request.clone();
    request.agents_md = resolve_agents_md(
        &request,
        snapshot.contents.get("AGENTS.md").map(Vec::as_slice),
    )?;
    let policy = inspect_policy(&request.agents_md);
    if policy.heading_count != 1 || !policy.exact {
        return Err(InitError::InvalidRequest(
            "agents_md must contain exactly one canonical Project memory section".to_owned(),
        ));
    }

    let project_page = render_project_page(&request)?;
    let mut desired = desired_candidate(&request, &snapshot.contents, inspection, project_page);
    if request.mode == InitMode::Full {
        canonicalize_full_index(&mut desired)?;
    }
    with_candidate_dir(Path::new(&inspection.root), |candidate| {
        stage_candidate(candidate, &desired)?;
        validate_candidate(candidate.path(), request.mode)
    })?;
    validate_owned_candidate(&desired, request.mode)?;

    let mut operations = build_operations(&desired, &inspection.prestates)?;
    sort_operations(&mut operations);
    with_candidate_dir(Path::new(&inspection.root), |candidate| {
        stage_retained(candidate, &inspection.prestates, &snapshot.contents)?;
        apply_candidate_operations(candidate, &operations)?;
        finalize_candidate_modes(candidate.path(), &desired)?;
        validate_candidate(candidate.path(), request.mode)?;
        verify_candidate(candidate.path(), &desired)
    })?;
    let proposal = operations
        .iter()
        .map(proposal_line)
        .collect::<Vec<_>>()
        .join("\n");
    let manifest = InitManifest {
        manifest_contract: 1,
        layout_version: LAYOUT_VERSION,
        yams_version: env!("CARGO_PKG_VERSION").to_owned(),
        root: inspection.root.clone(),
        mode: request.mode,
        inspection_sha256: inspection.inspection_sha256.clone(),
        asset_sha256: asset_digests(),
        operations,
        candidate_sha256: owned_candidate_sha256(&desired),
        proposal,
    };
    let manifest_sha256 = sha256(&canonical_manifest_bytes(&manifest)?);
    Ok(ManifestEnvelope {
        ok: true,
        manifest_sha256,
        manifest,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CandidateIdentity {
    device: u64,
    inode: u64,
    kind: FileType,
}

impl CandidateIdentity {
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(state: &Stat) -> Self {
        Self {
            device: state.st_dev as u64,
            inode: state.st_ino as u64,
            kind: FileType::from_raw_mode(state.st_mode),
        }
    }
}

#[derive(Debug)]
struct CandidateOwnership {
    parent: OwnedFd,
    parent_path: PathBuf,
    parent_identity: CandidateIdentity,
    name: OsString,
    root: OwnedFd,
    root_identity: CandidateIdentity,
    journal: BTreeMap<PathBuf, CandidateIdentity>,
}

#[derive(Debug)]
struct CandidateTemp {
    path: PathBuf,
    ownership: Option<CandidateOwnership>,
}

impl CandidateTemp {
    fn path(&self) -> &Path {
        &self.path
    }

    fn ownership(&self) -> Result<&CandidateOwnership, InitError> {
        self.ownership.as_ref().ok_or_else(|| {
            InitError::Candidate("candidate ownership is no longer available".to_owned())
        })
    }

    fn ownership_mut(&mut self) -> Result<&mut CandidateOwnership, InitError> {
        self.ownership.as_mut().ok_or_else(|| {
            InitError::Candidate("candidate ownership is no longer available".to_owned())
        })
    }

    fn record_created(&mut self, relative: &str) -> Result<(), InitError> {
        let ownership = self.ownership_mut()?;
        let identity = record_candidate_identity(ownership, Path::new(relative))
            .map_err(|source| io_error("record candidate node identity", relative, source))?;
        ownership.journal.insert(PathBuf::from(relative), identity);
        Ok(())
    }

    fn forget_owned(&mut self, relative: &str) -> Result<(), InitError> {
        self.ownership_mut()?.journal.remove(Path::new(relative));
        Ok(())
    }

    fn verify_access_binding(&self) -> Result<(), InitError> {
        let ownership = self.ownership()?;
        verify_candidate_access_binding(ownership).map_err(|source| InitError::Io {
            operation: "verify candidate staging path binding",
            path: self.path.clone(),
            source,
        })
    }

    #[cfg(test)]
    fn capture_owned_tree(&mut self) -> Result<(), InitError> {
        let path = self.path.clone();
        let ownership = self.ownership_mut()?;
        let mut journal = BTreeMap::new();
        capture_candidate_tree(ownership.root.as_fd(), Path::new(""), &mut journal).map_err(
            |source| InitError::Io {
                operation: "capture candidate ownership journal",
                path,
                source,
            },
        )?;
        ownership.journal = journal;
        Ok(())
    }

    fn close(mut self) -> Result<(), InitError> {
        let mut hooks = SystemCandidateCleanupHooks;
        self.close_inner(&mut hooks)
    }

    #[cfg(test)]
    fn close_with_hooks(mut self, hooks: &mut impl CandidateCleanupHooks) -> Result<(), InitError> {
        self.close_inner(hooks)
    }

    fn close_inner(&mut self, hooks: &mut impl CandidateCleanupHooks) -> Result<(), InitError> {
        let path = self.path().to_path_buf();
        let ownership = self.ownership.take().ok_or_else(|| {
            InitError::Candidate("candidate ownership is no longer available".to_owned())
        })?;
        cleanup_candidate(ownership, hooks).map_err(|source| InitError::Io {
            operation: "remove owned candidate staging tree",
            path,
            source,
        })
    }
}

impl Drop for CandidateTemp {
    fn drop(&mut self) {
        if let Some(ownership) = self.ownership.take() {
            let mut hooks = SystemCandidateCleanupHooks;
            let _ = cleanup_candidate(ownership, &mut hooks);
        }
    }
}

trait CandidateCleanupHooks {
    fn before_root_check(&mut self) {}
    fn before_child_open(&mut self, _relative: &Path) {}
    fn after_child_opened(&mut self, _relative: &Path) {}
    fn before_leaf_remove(&mut self, _relative: &Path) {}
}

struct SystemCandidateCleanupHooks;

impl CandidateCleanupHooks for SystemCandidateCleanupHooks {}

trait CandidateCreationHooks {
    fn after_base_opened(&mut self, _base: &Path) {}

    fn candidate_name(&mut self, prefix: &str, attempt: u32) -> OsString {
        generated_candidate_name(prefix, attempt)
    }

    fn mkdir_candidate(&mut self, base: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
        rfs::mkdirat(base, name, Mode::RWXU)
    }

    fn after_candidate_pinned(&mut self) -> Result<(), Errno> {
        Ok(())
    }

    fn remove_created(&mut self, base: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
        rfs::unlinkat(base, name, AtFlags::REMOVEDIR)
    }
}

struct SystemCandidateCreationHooks;

impl CandidateCreationHooks for SystemCandidateCreationHooks {}

fn with_candidate_dir<T>(
    root: &Path,
    operation: impl FnOnce(&mut CandidateTemp) -> Result<T, InitError>,
) -> Result<T, InitError> {
    let mut candidate = create_candidate_dir(root)?;
    let result = candidate
        .verify_access_binding()
        .and_then(|()| operation(&mut candidate))
        .and_then(|value| candidate.verify_access_binding().map(|()| value));
    let cleanup = candidate.close();
    match (result, cleanup) {
        (Ok(value), Ok(())) => Ok(value),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(_), Err(cleanup)) => Err(cleanup),
        (Err(primary), Err(cleanup)) => Err(InitError::Candidate(format!(
            "{primary}; candidate cleanup also failed: {cleanup}"
        ))),
    }
}

fn verify_candidate_access_binding(ownership: &CandidateOwnership) -> std::io::Result<()> {
    let parent_named = rfs::stat(&ownership.parent_path).map_err(errno_to_io_error)?;
    let parent_opened = rfs::fstat(&ownership.parent).map_err(errno_to_io_error)?;
    if CandidateIdentity::from_stat(&parent_named) != ownership.parent_identity
        || CandidateIdentity::from_stat(&parent_opened) != ownership.parent_identity
    {
        return Err(candidate_staging_base_binding_changed());
    }
    verify_candidate_root_binding(ownership)
}

fn verify_candidate_root_binding(ownership: &CandidateOwnership) -> std::io::Result<()> {
    let root_named = rfs::statat(
        ownership.parent.as_fd(),
        &ownership.name,
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|_| candidate_cleanup_root_binding_changed())?;
    let root_opened = rfs::fstat(&ownership.root).map_err(errno_to_io_error)?;
    if CandidateIdentity::from_stat(&root_named) != ownership.root_identity
        || CandidateIdentity::from_stat(&root_opened) != ownership.root_identity
    {
        return Err(candidate_cleanup_root_binding_changed());
    }
    Ok(())
}

fn cleanup_candidate(
    mut ownership: CandidateOwnership,
    hooks: &mut impl CandidateCleanupHooks,
) -> std::io::Result<()> {
    hooks.before_root_check();
    let named = rfs::statat(
        ownership.parent.as_fd(),
        &ownership.name,
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|_| candidate_cleanup_root_binding_changed())?;
    let opened = rfs::fstat(&ownership.root).map_err(errno_to_io_error)?;
    if CandidateIdentity::from_stat(&named) != ownership.root_identity
        || CandidateIdentity::from_stat(&opened) != ownership.root_identity
    {
        return Err(candidate_cleanup_root_binding_changed());
    }
    rfs::fchmod(ownership.root.as_fd(), writable_directory_mode(&opened))
        .map_err(errno_to_io_error)?;
    cleanup_candidate_directory(
        ownership.root.as_fd(),
        Path::new(""),
        &mut ownership.journal,
        hooks,
    )?;
    let named = rfs::statat(
        ownership.parent.as_fd(),
        &ownership.name,
        AtFlags::SYMLINK_NOFOLLOW,
    )
    .map_err(|_| candidate_cleanup_root_binding_changed())?;
    let opened = rfs::fstat(&ownership.root).map_err(errno_to_io_error)?;
    if CandidateIdentity::from_stat(&named) != ownership.root_identity
        || CandidateIdentity::from_stat(&opened) != ownership.root_identity
    {
        return Err(candidate_cleanup_root_binding_changed());
    }
    // POSIX offers no conditional rmdir-by-inode. Only the owning UID can race
    // this final check in the pinned staging parent; that same-UID actor is the
    // explicit identity-check-to-unlink trust boundary.
    rfs::unlinkat(
        ownership.parent.as_fd(),
        &ownership.name,
        AtFlags::REMOVEDIR,
    )
    .map_err(errno_to_io_error)
}

fn cleanup_candidate_directory(
    directory: BorrowedFd<'_>,
    relative: &Path,
    journal: &mut BTreeMap<PathBuf, CandidateIdentity>,
    hooks: &mut impl CandidateCleanupHooks,
) -> std::io::Result<()> {
    let names = cleanup_directory_names(directory)?;
    let expected_names = journal
        .keys()
        .filter(|path| path.parent() == Some(relative))
        .filter_map(|path| path.file_name().map(OsStr::to_os_string))
        .collect::<Vec<_>>();
    if names != expected_names {
        return Err(candidate_cleanup_binding_changed());
    }

    for name in names {
        let child_relative = relative.join(&name);
        let expected = *journal
            .get(&child_relative)
            .ok_or_else(candidate_cleanup_binding_changed)?;
        if expected.kind.is_dir() {
            hooks.before_child_open(&child_relative);
            let named = rfs::statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| candidate_cleanup_binding_changed())?;
            if CandidateIdentity::from_stat(&named) != expected {
                return Err(candidate_cleanup_binding_changed());
            }
            let child = rfs::openat(directory, &name, CLEANUP_DIRECTORY_FLAGS, Mode::empty())
                .map_err(|_| candidate_cleanup_binding_changed())?;
            let opened = rfs::fstat(&child).map_err(errno_to_io_error)?;
            if CandidateIdentity::from_stat(&opened) != expected {
                return Err(candidate_cleanup_binding_changed());
            }
            hooks.after_child_opened(&child_relative);
            verify_candidate_binding(directory, &name, child.as_fd(), expected)?;
            rfs::fchmod(child.as_fd(), writable_directory_mode(&opened))
                .map_err(errno_to_io_error)?;
            cleanup_candidate_directory(child.as_fd(), &child_relative, journal, hooks)?;
            verify_candidate_binding(directory, &name, child.as_fd(), expected)?;
            // POSIX has no conditional unlink-by-inode. The private 0700 candidate tree
            // excludes other UIDs; a same-UID process remains the boundary between this
            // final identity check and unlinkat, as it does for the repository writers.
            rfs::unlinkat(directory, &name, AtFlags::REMOVEDIR).map_err(errno_to_io_error)?;
        } else {
            hooks.before_leaf_remove(&child_relative);
            let named = rfs::statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(|_| candidate_cleanup_binding_changed())?;
            if CandidateIdentity::from_stat(&named) != expected {
                return Err(candidate_cleanup_binding_changed());
            }
            // See the same-UID identity-check-to-unlink boundary documented above.
            rfs::unlinkat(directory, &name, AtFlags::empty()).map_err(errno_to_io_error)?;
        }
        journal.remove(&child_relative);
    }
    Ok(())
}

fn verify_candidate_binding(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    opened: BorrowedFd<'_>,
    expected: CandidateIdentity,
) -> std::io::Result<()> {
    let named = rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| candidate_cleanup_binding_changed())?;
    let opened = rfs::fstat(opened).map_err(errno_to_io_error)?;
    if CandidateIdentity::from_stat(&named) != expected
        || CandidateIdentity::from_stat(&opened) != expected
    {
        return Err(candidate_cleanup_binding_changed());
    }
    Ok(())
}

fn record_candidate_identity(
    ownership: &CandidateOwnership,
    relative: &Path,
) -> std::io::Result<CandidateIdentity> {
    let parent_relative = relative.parent().unwrap_or_else(|| Path::new(""));
    let parent = open_candidate_relative_directory(ownership.root.as_fd(), parent_relative)?;
    let name = relative
        .file_name()
        .ok_or_else(|| std::io::Error::other("candidate node has no file name"))?;
    let state =
        rfs::statat(parent.as_fd(), name, AtFlags::SYMLINK_NOFOLLOW).map_err(errno_to_io_error)?;
    Ok(CandidateIdentity::from_stat(&state))
}

fn open_candidate_relative_directory(
    root: BorrowedFd<'_>,
    relative: &Path,
) -> std::io::Result<OwnedFd> {
    let mut directory = rustix::io::dup(root).map_err(errno_to_io_error)?;
    for component in relative.components() {
        let std::path::Component::Normal(name) = component else {
            return Err(std::io::Error::other(
                "candidate ownership path is not relative and normalized",
            ));
        };
        directory = rfs::openat(
            directory.as_fd(),
            name,
            CLEANUP_DIRECTORY_FLAGS,
            Mode::empty(),
        )
        .map_err(errno_to_io_error)?;
    }
    Ok(directory)
}

#[cfg(test)]
fn capture_candidate_tree(
    directory: BorrowedFd<'_>,
    relative: &Path,
    journal: &mut BTreeMap<PathBuf, CandidateIdentity>,
) -> std::io::Result<()> {
    for name in cleanup_directory_names(directory)? {
        let child_relative = relative.join(&name);
        let state =
            rfs::statat(directory, &name, AtFlags::SYMLINK_NOFOLLOW).map_err(errno_to_io_error)?;
        let identity = CandidateIdentity::from_stat(&state);
        journal.insert(child_relative.clone(), identity);
        if identity.kind.is_dir() {
            let child = rfs::openat(directory, &name, CLEANUP_DIRECTORY_FLAGS, Mode::empty())
                .map_err(errno_to_io_error)?;
            let opened = rfs::fstat(&child).map_err(errno_to_io_error)?;
            if CandidateIdentity::from_stat(&opened) != identity {
                return Err(candidate_cleanup_binding_changed());
            }
            capture_candidate_tree(child.as_fd(), &child_relative, journal)?;
            verify_candidate_binding(directory, &name, child.as_fd(), identity)?;
        }
    }
    Ok(())
}

fn cleanup_directory_names(directory: BorrowedFd<'_>) -> std::io::Result<Vec<OsString>> {
    let mut stream = Dir::read_from(directory).map_err(errno_to_io_error)?;
    let mut names = Vec::new();
    for entry in &mut stream {
        let entry = entry.map_err(errno_to_io_error)?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(OsString::from_vec(bytes.to_vec()));
        }
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    Ok(names)
}

#[allow(clippy::unnecessary_cast)]
fn writable_directory_mode(state: &Stat) -> Mode {
    Mode::from_bits_retain((((state.st_mode as u32) & 0o7777) | 0o700) as _)
}

fn candidate_cleanup_binding_changed() -> std::io::Error {
    std::io::Error::other("candidate cleanup binding changed during traversal")
}

fn candidate_cleanup_root_binding_changed() -> std::io::Error {
    std::io::Error::other("candidate cleanup root binding changed during traversal")
}

fn candidate_staging_base_binding_changed() -> std::io::Error {
    std::io::Error::other("candidate staging base binding changed during creation")
}

fn errno_to_io_error(error: Errno) -> std::io::Error {
    std::io::Error::from_raw_os_error(error.raw_os_error())
}

fn create_candidate_dir(root: &Path) -> Result<CandidateTemp, InitError> {
    let mut hooks = SystemCandidateCreationHooks;
    create_candidate_dir_with_hooks(root, &mut hooks)
}

fn create_candidate_dir_with_hooks(
    root: &Path,
    hooks: &mut impl CandidateCreationHooks,
) -> Result<CandidateTemp, InitError> {
    let prefix = candidate_prefix(root);
    let mut bases = Vec::new();
    if let Some(parent) = root.parent()
        && let Ok(canonical) = parent.canonicalize()
    {
        bases.push(canonical);
    }
    for fallback in [Path::new("/tmp"), Path::new("/var/tmp")] {
        if let Ok(canonical) = fallback.canonicalize()
            && !bases.contains(&canonical)
        {
            bases.push(canonical);
        }
    }

    for base in bases {
        if base.starts_with(root) {
            continue;
        }
        let parent = match rfs::open(&base, CLEANUP_DIRECTORY_FLAGS, Mode::empty()) {
            Ok(parent) => parent,
            Err(_) => continue,
        };
        let parent_state = match rfs::fstat(&parent) {
            Ok(parent_state) => parent_state,
            Err(_) => continue,
        };
        let parent_identity = CandidateIdentity::from_stat(&parent_state);
        if !parent_identity.kind.is_dir() {
            continue;
        }
        hooks.after_base_opened(&base);
        let mut name = None;
        for attempt in 0..CANDIDATE_NAME_ATTEMPTS {
            let candidate = hooks.candidate_name(&prefix, attempt);
            if !valid_candidate_name(&candidate, &prefix) {
                return Err(InitError::Candidate(
                    "candidate name generator returned an invalid basename".to_owned(),
                ));
            }
            match hooks.mkdir_candidate(parent.as_fd(), &candidate) {
                Ok(()) => {
                    name = Some(candidate);
                    break;
                }
                Err(Errno::EXIST) => continue,
                Err(_) => break,
            }
        }
        let Some(name) = name else {
            continue;
        };
        let candidate_path = base.join(&name);
        let candidate_root = rfs::openat(
            parent.as_fd(),
            &name,
            CLEANUP_DIRECTORY_FLAGS,
            Mode::empty(),
        )
        .map_err(|_| {
            InitError::Candidate(format!(
                "candidate staging root could not be pinned safely; unresolved staging residue may remain at {}",
                candidate_path.display()
            ))
        })?;
        let named = rfs::statat(parent.as_fd(), &name, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| {
            InitError::Candidate(format!(
                "candidate staging root binding changed; unresolved staging residue may remain at {}",
                candidate_path.display()
            ))
        })?;
        let opened = rfs::fstat(&candidate_root).map_err(|_| {
            InitError::Candidate(format!(
                "candidate staging root descriptor could not be verified; unresolved staging residue may remain at {}",
                candidate_path.display()
            ))
        })?;
        let root_identity = CandidateIdentity::from_stat(&opened);
        if CandidateIdentity::from_stat(&named) != root_identity || !root_identity.kind.is_dir() {
            return Err(InitError::Candidate(format!(
                "candidate staging root binding changed; unresolved staging residue may remain at {}",
                candidate_path.display()
            )));
        }
        let ownership = CandidateOwnership {
            parent,
            parent_path: base.clone(),
            parent_identity,
            name,
            root: candidate_root,
            root_identity,
            journal: BTreeMap::new(),
        };
        if let Err(primary) = hooks.after_candidate_pinned() {
            return rollback_candidate_creation(
                ownership,
                hooks,
                format!("candidate creation hook failed: {primary}"),
            );
        }
        if let Err(error) = verify_candidate_access_binding(&ownership) {
            return rollback_candidate_creation(ownership, hooks, error.to_string());
        }
        let candidate = CandidateTemp {
            path: candidate_path,
            ownership: Some(ownership),
        };
        return Ok(candidate);
    }
    Err(InitError::Candidate(
        "no safe writable candidate staging base exists outside the repository root".to_owned(),
    ))
}

fn rollback_candidate_creation<T>(
    ownership: CandidateOwnership,
    hooks: &mut impl CandidateCreationHooks,
    primary: String,
) -> Result<T, InitError> {
    let binding = verify_candidate_root_binding(&ownership);
    let cleanup = binding.and_then(|()| {
        hooks
            .remove_created(ownership.parent.as_fd(), &ownership.name)
            .map_err(errno_to_io_error)
    });
    match cleanup {
        Ok(()) => Err(InitError::Candidate(primary)),
        Err(cleanup) => Err(InitError::Candidate(format!(
            "{primary}; candidate creation rollback also failed: {cleanup}; unresolved staging residue may remain"
        ))),
    }
}

fn valid_candidate_name(name: &OsStr, prefix: &str) -> bool {
    let path = Path::new(name);
    path.components().count() == 1
        && matches!(
            path.components().next(),
            Some(std::path::Component::Normal(_))
        )
        && name.as_bytes().starts_with(prefix.as_bytes())
}

fn generated_candidate_name(prefix: &str, attempt: u32) -> OsString {
    let sequence = CANDIDATE_NAME_SEQUENCE.fetch_add(1, AtomicOrdering::Relaxed);
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| duration.as_nanos());
    let material = format!(
        "{}:{}:{now}:{sequence}:{attempt}:{:?}",
        std::process::id(),
        prefix,
        std::thread::current().id()
    );
    OsString::from(format!("{prefix}{}", &sha256(material.as_bytes())[..32]))
}

fn candidate_prefix(root: &Path) -> String {
    let digest = sha256(root.as_os_str().as_bytes());
    format!(".yams-init-candidate-{}-", &digest[..12])
}

fn render_project_page(request: &InitPlanRequest) -> Result<String, InitError> {
    let page = &request.project_page;
    let create = CreateRequest {
        title: page.title.clone(),
        page_type: page.page_type,
        owner: Owner::Shared,
        fact: page.fact.clone(),
        why: page.why.clone(),
        how_to_apply: page.how_to_apply.clone(),
        falsified_by: page.falsified_by.clone(),
        summary: page.summary.clone(),
        related: Vec::new(),
    };
    let rendered = render_create(&create, &request.date)
        .map_err(|error| InitError::InvalidRequest(error.to_string()))?;
    let parsed =
        parse_wiki_page(&rendered).map_err(|error| InitError::InvalidRequest(error.to_string()))?;
    if parsed.slug != "project-context" {
        return Err(InitError::InvalidRequest(format!(
            "project page title must produce slug project-context, not {}",
            parsed.slug
        )));
    }
    Ok(rendered)
}

fn desired_candidate(
    request: &InitPlanRequest,
    retained: &BTreeMap<String, Vec<u8>>,
    inspection: &super::InitInspection,
    project_page: String,
) -> BTreeMap<String, DesiredNode> {
    let mut desired = BTreeMap::new();
    desired.insert(
        "AGENTS.md".to_owned(),
        DesiredNode::File {
            mode: retained_mode(inspection, "AGENTS.md", FILE_MODE),
            bytes: request.agents_md.as_bytes().to_vec(),
        },
    );
    for path in [".agents", ".agents/memory"] {
        desired.insert(
            path.to_owned(),
            DesiredNode::Directory {
                mode: retained_mode(inspection, path, DIRECTORY_MODE),
            },
        );
    }
    desired.insert(
        ".agents/memory/.gitignore".to_owned(),
        DesiredNode::File {
            mode: retained_mode(inspection, ".agents/memory/.gitignore", FILE_MODE),
            bytes: crate::MEMORY_GITIGNORE.as_bytes().to_vec(),
        },
    );
    match request.mode {
        InitMode::Minimal => {
            let path = ".agents/memory/project-context.md";
            desired.insert(
                path.to_owned(),
                DesiredNode::File {
                    mode: retained_mode(inspection, path, FILE_MODE),
                    bytes: project_page.into_bytes(),
                },
            );
        }
        InitMode::Full => {
            desired.insert(
                ".agents/memory/pages".to_owned(),
                DesiredNode::Directory {
                    mode: retained_mode(inspection, ".agents/memory/pages", DIRECTORY_MODE),
                },
            );
            for (path, bytes) in retained {
                if path.starts_with(".agents/memory/pages/") {
                    desired.insert(
                        path.clone(),
                        DesiredNode::File {
                            mode: retained_mode(inspection, path, FILE_MODE),
                            bytes: bytes.clone(),
                        },
                    );
                }
            }
            for (path, bytes) in [
                (".agents/memory/SCHEMA.md", SCHEMA.as_bytes().to_vec()),
                (
                    ".agents/memory/INDEX.md",
                    retained
                        .get(".agents/memory/INDEX.md")
                        .cloned()
                        .unwrap_or_else(|| INDEX_TEMPLATE.as_bytes().to_vec()),
                ),
                (
                    ".agents/memory/pages/project-context.md",
                    project_page.into_bytes(),
                ),
            ] {
                desired.insert(
                    path.to_owned(),
                    DesiredNode::File {
                        mode: retained_mode(inspection, path, FILE_MODE),
                        bytes,
                    },
                );
            }
        }
    }
    desired
}

fn retained_mode(inspection: &super::InitInspection, path: &str, fallback: u32) -> u32 {
    inspection
        .prestates
        .iter()
        .find(|prestate| prestate.path == path)
        .and_then(|prestate| prestate.mode)
        .unwrap_or(fallback)
}

fn canonicalize_full_index(desired: &mut BTreeMap<String, DesiredNode>) -> Result<(), InitError> {
    let page_prefix = ".agents/memory/pages/";
    let mut pages = Vec::new();
    for (path, node) in desired.iter() {
        if !path.starts_with(page_prefix) {
            continue;
        }
        let DesiredNode::File { bytes, .. } = node else {
            return Err(InitError::Candidate(format!(
                "{path} is not a candidate page file"
            )));
        };
        let name = &path[page_prefix.len()..];
        let source = std::str::from_utf8(bytes)
            .map_err(|_| InitError::Candidate(format!("{path} is not valid UTF-8")))?;
        pages.push(
            parse_index_page(name, source)
                .map_err(|error| InitError::Candidate(error.to_string()))?,
        );
    }
    let index_path = ".agents/memory/INDEX.md";
    let index = desired
        .get(index_path)
        .ok_or_else(|| InitError::Candidate("full candidate is missing INDEX.md".to_owned()))?;
    let DesiredNode::File { mode, bytes } = index else {
        return Err(InitError::Candidate(
            "full candidate INDEX.md is not a file".to_owned(),
        ));
    };
    let current = std::str::from_utf8(bytes)
        .map_err(|_| InitError::Candidate(format!("{index_path} is not valid UTF-8")))?;
    let canonical =
        rebuild_index(current, &pages).map_err(|error| InitError::Candidate(error.to_string()))?;
    desired.insert(
        index_path.to_owned(),
        DesiredNode::File {
            mode: *mode,
            bytes: canonical.into_bytes(),
        },
    );
    Ok(())
}

fn stage_candidate(
    candidate: &mut CandidateTemp,
    desired: &BTreeMap<String, DesiredNode>,
) -> Result<(), InitError> {
    stage_candidate_nodes(candidate, desired)?;
    finalize_candidate_modes(candidate.path(), desired)
}

fn stage_candidate_nodes(
    candidate: &mut CandidateTemp,
    desired: &BTreeMap<String, DesiredNode>,
) -> Result<(), InitError> {
    let mut directories = desired
        .iter()
        .filter(|(_, node)| matches!(node, DesiredNode::Directory { .. }))
        .collect::<Vec<_>>();
    directories.sort_by(|(left, _), (right, _)| {
        path_depth(left)
            .cmp(&path_depth(right))
            .then_with(|| left.cmp(right))
    });
    for (path, node) in directories {
        let target = candidate.path().join(path);
        fs::create_dir(&target)
            .map_err(|source| io_error("create candidate directory", path, source))?;
        candidate.record_created(path)?;
        set_mode(&target, node.mode() | 0o700, path)?;
    }
    for (path, node) in desired {
        let DesiredNode::File { mode, bytes } = node else {
            continue;
        };
        let target = candidate.path().join(path);
        fs::write(&target, bytes)
            .map_err(|source| io_error("write candidate file", path, source))?;
        candidate.record_created(path)?;
        set_mode(&target, *mode | 0o600, path)?;
    }
    Ok(())
}

fn stage_retained(
    candidate: &mut CandidateTemp,
    prestates: &[NodePrestate],
    contents: &BTreeMap<String, Vec<u8>>,
) -> Result<(), InitError> {
    let mut retained = BTreeMap::new();
    for prestate in prestates {
        match prestate.kind {
            NodeKind::Directory => {
                let mode = prestate.mode.ok_or_else(|| {
                    InitError::Candidate(format!(
                        "captured directory has no mode: {}",
                        prestate.path
                    ))
                })?;
                retained.insert(prestate.path.clone(), DesiredNode::Directory { mode });
            }
            NodeKind::File => {
                let mode = prestate.mode.ok_or_else(|| {
                    InitError::Candidate(format!("captured file has no mode: {}", prestate.path))
                })?;
                if let Some(bytes) = contents.get(&prestate.path) {
                    retained.insert(
                        prestate.path.clone(),
                        DesiredNode::File {
                            mode,
                            bytes: bytes.clone(),
                        },
                    );
                }
            }
            NodeKind::Missing | NodeKind::Symlink | NodeKind::Other => {}
        }
    }
    stage_candidate_nodes(candidate, &retained)
}

fn apply_candidate_operations(
    candidate: &mut CandidateTemp,
    operations: &[InitOperation],
) -> Result<(), InitError> {
    for operation in operations {
        let target = candidate.path().join(&operation.path);
        match operation.kind {
            OperationKind::CreateDirectory => {
                fs::create_dir(&target).map_err(|source| {
                    io_error(
                        "apply candidate directory creation",
                        &operation.path,
                        source,
                    )
                })?;
                candidate.record_created(&operation.path)?;
                set_mode(&target, 0o700, &operation.path)?;
            }
            OperationKind::CreateFile => {
                fs::write(
                    &target,
                    operation.content.as_ref().ok_or_else(|| {
                        InitError::Candidate(format!(
                            "candidate file operation has no content: {}",
                            operation.path
                        ))
                    })?,
                )
                .map_err(|source| {
                    io_error("apply candidate file write", &operation.path, source)
                })?;
                candidate.record_created(&operation.path)?;
                set_mode(&target, 0o600, &operation.path)?;
            }
            OperationKind::ReplaceFile => {
                fs::remove_file(&target).map_err(|source| {
                    io_error("remove replaced candidate file", &operation.path, source)
                })?;
                candidate.forget_owned(&operation.path)?;
                fs::write(
                    &target,
                    operation.content.as_ref().ok_or_else(|| {
                        InitError::Candidate(format!(
                            "candidate file operation has no content: {}",
                            operation.path
                        ))
                    })?,
                )
                .map_err(|source| {
                    io_error("install replaced candidate file", &operation.path, source)
                })?;
                candidate.record_created(&operation.path)?;
                set_mode(&target, 0o600, &operation.path)?;
            }
            OperationKind::RemoveFile => {
                fs::remove_file(&target).map_err(|source| {
                    io_error("apply candidate file removal", &operation.path, source)
                })?;
                candidate.forget_owned(&operation.path)?;
            }
        }
    }
    Ok(())
}

fn finalize_candidate_modes(
    root: &Path,
    desired: &BTreeMap<String, DesiredNode>,
) -> Result<(), InitError> {
    let mut nodes = desired.iter().collect::<Vec<_>>();
    nodes.sort_by(|(left, _), (right, _)| {
        path_depth(right)
            .cmp(&path_depth(left))
            .then_with(|| left.cmp(right))
    });
    for (path, node) in nodes {
        set_mode(&root.join(path), node.mode(), path)?;
    }
    Ok(())
}

fn verify_candidate(root: &Path, desired: &BTreeMap<String, DesiredNode>) -> Result<(), InitError> {
    let mut actual_paths = BTreeSet::new();
    collect_candidate_paths(root, root, &mut actual_paths)?;
    let desired_paths = desired.keys().cloned().collect::<BTreeSet<_>>();
    if actual_paths != desired_paths {
        return Err(InitError::Candidate(format!(
            "candidate node set differs from the manifest: expected {desired_paths:?}, found {actual_paths:?}"
        )));
    }
    for (path, node) in desired {
        let target = root.join(path);
        let metadata = fs::symlink_metadata(&target)
            .map_err(|source| io_error("verify candidate node", path, source))?;
        let actual_mode = metadata.permissions().mode() & 0o7777;
        match node {
            DesiredNode::Directory { mode } => {
                if !metadata.is_dir() || actual_mode != *mode {
                    return Err(InitError::Candidate(format!(
                        "{path} does not match the planned directory kind and mode"
                    )));
                }
            }
            DesiredNode::File { mode, bytes } => {
                let actual = fs::read(&target)
                    .map_err(|source| io_error("verify candidate file", path, source))?;
                if !metadata.is_file() || actual_mode != *mode || actual != *bytes {
                    return Err(InitError::Candidate(format!(
                        "{path} does not match the planned file bytes and mode"
                    )));
                }
            }
        }
    }
    Ok(())
}

fn collect_candidate_paths(
    root: &Path,
    directory: &Path,
    paths: &mut BTreeSet<String>,
) -> Result<(), InitError> {
    let mut entries = fs::read_dir(directory)
        .map_err(|source| io_error("enumerate candidate directory", "<candidate>", source))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|source| io_error("read candidate directory entry", "<candidate>", source))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| {
                InitError::Candidate(format!(
                    "candidate entry escaped the staging root: {}",
                    path.display()
                ))
            })?
            .to_str()
            .ok_or_else(|| InitError::Candidate("candidate path is not UTF-8".to_owned()))?
            .to_owned();
        let metadata = fs::symlink_metadata(&path)
            .map_err(|source| io_error("inspect candidate node", &relative, source))?;
        paths.insert(relative);
        if metadata.is_dir() {
            collect_candidate_paths(root, &path, paths)?;
        }
    }
    Ok(())
}

fn set_mode(path: &Path, mode: u32, relative: &str) -> Result<(), InitError> {
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
        .map_err(|source| io_error("set candidate mode", relative, source))
}

fn validate_candidate(root: &Path, mode: InitMode) -> Result<(), InitError> {
    let agents = fs::read_to_string(root.join("AGENTS.md"))
        .map_err(|source| io_error("read candidate policy", "AGENTS.md", source))?;
    let policy = inspect_policy(&agents);
    if policy.heading_count != 1 || !policy.exact {
        return Err(InitError::Candidate(
            "candidate AGENTS.md does not contain the canonical policy".to_owned(),
        ));
    }
    let memory = root.join(".agents/memory");
    match mode {
        InitMode::Minimal => {
            let gitignore = fs::read(memory.join(".gitignore")).map_err(|source| {
                io_error(
                    "read candidate memory gitignore",
                    ".agents/memory/.gitignore",
                    source,
                )
            })?;
            if gitignore != crate::MEMORY_GITIGNORE.as_bytes() {
                return Err(InitError::Candidate(
                    "candidate memory gitignore differs from the embedded lock-ignore file"
                        .to_owned(),
                ));
            }
            let source =
                fs::read_to_string(memory.join("project-context.md")).map_err(|source| {
                    io_error(
                        "read candidate project page",
                        ".agents/memory/project-context.md",
                        source,
                    )
                })?;
            let page = parse_wiki_page(&source)
                .map_err(|error| InitError::Candidate(error.to_string()))?;
            if page.slug != "project-context" {
                return Err(InitError::Candidate(
                    "candidate flat page slug is not project-context".to_owned(),
                ));
            }
        }
        InitMode::Full => {
            let gitignore = fs::read(memory.join(".gitignore")).map_err(|source| {
                io_error(
                    "read candidate memory gitignore",
                    ".agents/memory/.gitignore",
                    source,
                )
            })?;
            if gitignore != crate::MEMORY_GITIGNORE.as_bytes() {
                return Err(InitError::Candidate(
                    "candidate memory gitignore differs from the embedded lock-ignore file"
                        .to_owned(),
                ));
            }
            let schema = fs::read(memory.join("SCHEMA.md")).map_err(|source| {
                io_error("read candidate schema", ".agents/memory/SCHEMA.md", source)
            })?;
            if schema != SCHEMA.as_bytes() {
                return Err(InitError::Candidate(
                    "candidate SCHEMA.md differs from the embedded schema".to_owned(),
                ));
            }
            let index = fs::read(memory.join("INDEX.md")).map_err(|source| {
                io_error("read candidate index", ".agents/memory/INDEX.md", source)
            })?;
            let pages_path = memory.join("pages");
            let mut entries = fs::read_dir(&pages_path)
                .map_err(|source| {
                    io_error("enumerate candidate pages", ".agents/memory/pages", source)
                })?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|source| {
                    io_error("read candidate page entry", ".agents/memory/pages", source)
                })?;
            entries.sort_by_key(|entry| entry.file_name());
            let mut pages = Vec::new();
            for entry in entries {
                let metadata = fs::symlink_metadata(entry.path()).map_err(|source| {
                    io_error("inspect candidate page", ".agents/memory/pages", source)
                })?;
                let outcome = if metadata.is_file() {
                    CapturedPageOutcome::Present(fs::read(entry.path()).map_err(|source| {
                        io_error("read candidate page", ".agents/memory/pages", source)
                    })?)
                } else {
                    CapturedPageOutcome::NotRegular
                };
                pages.push(CapturedPage {
                    name: entry.file_name(),
                    outcome,
                });
            }
            let snapshot = WikiSnapshot {
                corpus: memory,
                index,
                pages,
                isolated: true,
                isolation_note: None,
            };
            let check = validate_wiki(&snapshot);
            if !check.failures.is_empty() {
                return Err(InitError::Candidate(check.failures.join("; ")));
            }
            let compat = compat_snapshot(&snapshot);
            if !compat.violations.is_empty() {
                return Err(InitError::Candidate(compat.violations.join("; ")));
            }
        }
    }
    Ok(())
}

/// Validates a complete initialization-owned candidate without filesystem
/// access. Apply uses this before mutation and again over its pinned final
/// snapshot so planning and application share one layout contract.
pub(crate) fn validate_owned_candidate(
    desired: &BTreeMap<String, DesiredNode>,
    mode: InitMode,
) -> Result<(), InitError> {
    let expected_base = match mode {
        InitMode::Minimal => BTreeSet::from([
            "AGENTS.md".to_owned(),
            ".agents".to_owned(),
            ".agents/memory".to_owned(),
            ".agents/memory/.gitignore".to_owned(),
            ".agents/memory/project-context.md".to_owned(),
        ]),
        InitMode::Full => BTreeSet::from([
            "AGENTS.md".to_owned(),
            ".agents".to_owned(),
            ".agents/memory".to_owned(),
            ".agents/memory/.gitignore".to_owned(),
            ".agents/memory/SCHEMA.md".to_owned(),
            ".agents/memory/INDEX.md".to_owned(),
            ".agents/memory/pages".to_owned(),
            ".agents/memory/pages/project-context.md".to_owned(),
        ]),
    };
    let actual = desired.keys().cloned().collect::<BTreeSet<_>>();
    let valid_set = match mode {
        InitMode::Minimal => actual == expected_base,
        InitMode::Full => {
            expected_base.is_subset(&actual)
                && actual.iter().all(|path| {
                    expected_base.contains(path)
                        || path
                            .strip_prefix(".agents/memory/pages/")
                            .is_some_and(|name| {
                                !name.is_empty() && !name.contains('/') && name.ends_with(".md")
                            })
                })
        }
    };
    if !valid_set {
        return Err(InitError::Candidate(format!(
            "candidate owned node set is invalid for {mode:?}: {actual:?}"
        )));
    }
    for path in [".agents", ".agents/memory"] {
        if !matches!(desired.get(path), Some(DesiredNode::Directory { .. })) {
            return Err(InitError::Candidate(format!(
                "{path} is not a candidate directory"
            )));
        }
    }
    if mode == InitMode::Full
        && !matches!(
            desired.get(".agents/memory/pages"),
            Some(DesiredNode::Directory { .. })
        )
    {
        return Err(InitError::Candidate(
            ".agents/memory/pages is not a candidate directory".to_owned(),
        ));
    }
    let agents = desired_file(desired, "AGENTS.md")?;
    let agents = std::str::from_utf8(agents)
        .map_err(|_| InitError::Candidate("candidate AGENTS.md is not UTF-8".to_owned()))?;
    let policy = inspect_policy(agents);
    if policy.heading_count != 1 || !policy.exact {
        return Err(InitError::Candidate(
            "candidate AGENTS.md does not contain the canonical policy".to_owned(),
        ));
    }
    match mode {
        InitMode::Minimal => {
            let source =
                std::str::from_utf8(desired_file(desired, ".agents/memory/project-context.md")?)
                    .map_err(|_| {
                        InitError::Candidate("candidate project page is not UTF-8".to_owned())
                    })?;
            let page =
                parse_wiki_page(source).map_err(|error| InitError::Candidate(error.to_string()))?;
            if page.slug != "project-context" {
                return Err(InitError::Candidate(
                    "candidate flat page slug is not project-context".to_owned(),
                ));
            }
        }
        InitMode::Full => {
            if desired_file(desired, ".agents/memory/.gitignore")?
                != crate::MEMORY_GITIGNORE.as_bytes()
            {
                return Err(InitError::Candidate(
                    "candidate memory gitignore differs from the embedded lock-ignore file"
                        .to_owned(),
                ));
            }
            if desired_file(desired, ".agents/memory/SCHEMA.md")? != SCHEMA.as_bytes() {
                return Err(InitError::Candidate(
                    "candidate SCHEMA.md differs from the embedded schema".to_owned(),
                ));
            }
            let index = desired_file(desired, ".agents/memory/INDEX.md")?.to_vec();
            let mut pages = desired
                .iter()
                .filter_map(|(path, node)| {
                    path.strip_prefix(".agents/memory/pages/")
                        .map(|name| (name, node))
                })
                .map(|(name, node)| CapturedPage {
                    name: OsString::from(name),
                    outcome: match node {
                        DesiredNode::File { bytes, .. } => {
                            CapturedPageOutcome::Present(bytes.clone())
                        }
                        DesiredNode::Directory { .. } => CapturedPageOutcome::NotRegular,
                    },
                })
                .collect::<Vec<_>>();
            pages.sort_by(|left, right| left.name.cmp(&right.name));
            let snapshot = WikiSnapshot {
                corpus: PathBuf::from("<owned-candidate>"),
                index,
                pages,
                isolated: true,
                isolation_note: None,
            };
            let check = validate_wiki(&snapshot);
            if !check.failures.is_empty() {
                return Err(InitError::Candidate(check.failures.join("; ")));
            }
            let compat = compat_snapshot(&snapshot);
            if !compat.violations.is_empty() {
                return Err(InitError::Candidate(compat.violations.join("; ")));
            }
        }
    }
    Ok(())
}

fn desired_file<'a>(
    desired: &'a BTreeMap<String, DesiredNode>,
    path: &str,
) -> Result<&'a [u8], InitError> {
    match desired.get(path) {
        Some(DesiredNode::File { bytes, .. }) => Ok(bytes),
        _ => Err(InitError::Candidate(format!(
            "{path} is not a candidate file"
        ))),
    }
}

fn build_operations(
    desired: &BTreeMap<String, DesiredNode>,
    prestates: &[NodePrestate],
) -> Result<Vec<InitOperation>, InitError> {
    let mut operations = Vec::new();
    for (path, node) in desired {
        let prestate = prestate_for(prestates, path);
        match node {
            DesiredNode::Directory { mode } if prestate.kind == NodeKind::Missing => {
                operations.push(InitOperation {
                    kind: OperationKind::CreateDirectory,
                    path: path.clone(),
                    prestate,
                    mode: Some(*mode),
                    content: None,
                    post_sha256: None,
                });
            }
            DesiredNode::File { mode, bytes } if prestate.kind == NodeKind::Missing => {
                operations.push(file_operation(
                    OperationKind::CreateFile,
                    path,
                    prestate,
                    *mode,
                    bytes,
                )?);
            }
            DesiredNode::File { mode, bytes }
                if prestate.kind == NodeKind::File
                    && (prestate.sha256.as_deref() != Some(sha256(bytes).as_str())
                        || prestate.mode != Some(*mode)) =>
            {
                operations.push(file_operation(
                    OperationKind::ReplaceFile,
                    path,
                    prestate,
                    *mode,
                    bytes,
                )?);
            }
            DesiredNode::Directory { .. } | DesiredNode::File { .. } => {}
        }
    }
    for prestate in prestates {
        if prestate.kind != NodeKind::Missing
            && !desired.contains_key(&prestate.path)
            && prestate.path == ".agents/memory/project-context.md"
        {
            operations.push(InitOperation {
                kind: OperationKind::RemoveFile,
                path: prestate.path.clone(),
                prestate: prestate.clone(),
                mode: None,
                content: None,
                post_sha256: None,
            });
        }
    }
    Ok(operations)
}

fn file_operation(
    kind: OperationKind,
    path: &str,
    prestate: NodePrestate,
    mode: u32,
    bytes: &[u8],
) -> Result<InitOperation, InitError> {
    let content = String::from_utf8(bytes.to_vec())
        .map_err(|_| InitError::Candidate(format!("{path} is not valid UTF-8")))?;
    Ok(InitOperation {
        kind,
        path: path.to_owned(),
        prestate,
        mode: Some(mode),
        content: Some(content),
        post_sha256: Some(sha256(bytes)),
    })
}

fn prestate_for(prestates: &[NodePrestate], path: &str) -> NodePrestate {
    prestates
        .iter()
        .find(|prestate| prestate.path == path)
        .cloned()
        .unwrap_or_else(|| NodePrestate {
            path: path.to_owned(),
            kind: NodeKind::Missing,
            mode: None,
            sha256: None,
            entries_sha256: None,
        })
}

fn operation_rank(kind: OperationKind) -> u8 {
    match kind {
        OperationKind::CreateDirectory => 0,
        OperationKind::CreateFile | OperationKind::ReplaceFile => 1,
        OperationKind::RemoveFile => 2,
    }
}

fn sort_operations(operations: &mut [InitOperation]) {
    operations.sort_by(|left, right| {
        let rank = operation_rank(left.kind).cmp(&operation_rank(right.kind));
        if rank != Ordering::Equal {
            return rank;
        }
        if left.kind == OperationKind::RemoveFile {
            path_depth(&right.path)
                .cmp(&path_depth(&left.path))
                .then_with(|| left.path.cmp(&right.path))
        } else {
            path_depth(&left.path)
                .cmp(&path_depth(&right.path))
                .then_with(|| left.path.cmp(&right.path))
        }
    });
}

fn path_depth(path: &str) -> usize {
    path.split('/').count()
}

fn proposal_line(operation: &InitOperation) -> String {
    let action = match operation.kind {
        OperationKind::CreateDirectory => "CREATE DIR",
        OperationKind::CreateFile => "CREATE FILE",
        OperationKind::ReplaceFile => "REPLACE FILE",
        OperationKind::RemoveFile => "REMOVE FILE",
    };
    format!("{action} {}", operation.path)
}

fn asset_digests() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("SCHEMA.md".to_owned(), sha256(SCHEMA.as_bytes())),
        (
            "agent-policy.md".to_owned(),
            sha256(AGENT_POLICY.as_bytes()),
        ),
        (
            "index-template.md".to_owned(),
            sha256(INDEX_TEMPLATE.as_bytes()),
        ),
        (
            "page-template.md".to_owned(),
            sha256(PAGE_TEMPLATE.as_bytes()),
        ),
        (
            "memory-gitignore".to_owned(),
            sha256(crate::MEMORY_GITIGNORE.as_bytes()),
        ),
    ])
}

/// Digests initialization-owned desired nodes only. Runtime lock files and
/// unrelated repository nodes are deliberately outside this contract.
pub(crate) fn owned_candidate_sha256(desired: &BTreeMap<String, DesiredNode>) -> String {
    let mut encoded = Vec::new();
    for (path, node) in desired {
        if !is_initialization_owned_path(path) {
            continue;
        }
        encoded.extend_from_slice(&(path.len() as u64).to_be_bytes());
        encoded.extend_from_slice(path.as_bytes());
        match node {
            DesiredNode::Directory { mode } => {
                encoded.push(b'd');
                encoded.extend_from_slice(&mode.to_be_bytes());
            }
            DesiredNode::File { mode, bytes } => {
                encoded.push(b'f');
                encoded.extend_from_slice(&mode.to_be_bytes());
                encoded.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
                encoded.extend_from_slice(bytes);
            }
        }
    }
    sha256(&encoded)
}

fn is_initialization_owned_path(path: &str) -> bool {
    matches!(
        path,
        "AGENTS.md"
            | ".agents"
            | ".agents/memory"
            | ".agents/memory/project-context.md"
            | ".agents/memory/.gitignore"
            | ".agents/memory/SCHEMA.md"
            | ".agents/memory/INDEX.md"
            | ".agents/memory/pages"
    ) || path
        .strip_prefix(".agents/memory/pages/")
        .is_some_and(|name| !name.is_empty() && !name.contains('/') && name.ends_with(".md"))
}

const fn mode_name(mode: InitMode) -> &'static str {
    match mode {
        InitMode::Minimal => "minimal",
        InitMode::Full => "full",
    }
}

fn io_error(operation: &'static str, path: &str, source: std::io::Error) -> InitError {
    InitError::Io {
        operation,
        path: PathBuf::from(path),
        source,
    }
}

#[cfg(test)]
include!("tests.rs");
