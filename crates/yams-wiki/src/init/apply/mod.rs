#[cfg(test)]
include!("tests.rs");

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::path::{Component, Path};
use std::sync::atomic::{AtomicU64, Ordering};

use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags, RenameFlags, Stat};
use rustix::io::Errno;

use super::inspect::capture_repository;
use super::plan::{
    DesiredNode, canonical_manifest_bytes, owned_candidate_sha256, validate_owned_candidate,
};
use super::{
    AGENT_POLICY, ApplyResult, INDEX_TEMPLATE, InitError, InitManifest, InitMode, InitOperation,
    LAYOUT_VERSION, LayoutClass, ManifestEnvelope, NodeKind, NodePrestate, OperationKind,
    PAGE_TEMPLATE, SCHEMA, inspect_policy, sha256,
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
const CREATE_FLAGS: OFlags = OFlags::WRONLY
    .union(OFlags::CREATE)
    .union(OFlags::EXCL)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// Process-level classification kept separate from the serialized apply result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ApplyExitClass {
    /// The manifest was applied and fully validated.
    Success,
    /// Applying was refused because the manifest or repository state no
    /// longer matched its approved domain preconditions.
    Usage,
    /// Applying or recovering encountered an operating-system, Git, I/O, or
    /// resource failure. Operational recovery failures dominate a usage
    /// refusal for process exit purposes.
    Operational,
}

/// An apply result paired with its typed CLI exit classification.
///
/// `result` retains the frozen serialized contract and the primary apply
/// diagnostic. `class` is process-only metadata: it is `Success` exactly when
/// `result.ok` is true, and an operational recovery failure may upgrade it
/// without replacing the primary diagnostic in `result.error`.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ApplyOutcome {
    /// The stable machine-readable apply result.
    pub result: ApplyResult,
    /// The process exit classification, deliberately excluded from `result`.
    pub class: ApplyExitClass,
}

#[derive(Debug)]
struct ApplyFailure {
    class: ApplyExitClass,
    message: String,
}

#[derive(Debug)]
struct ResidueFailure {
    failure: ApplyFailure,
    path: String,
}

impl ApplyFailure {
    fn usage(message: impl Into<String>) -> Self {
        Self {
            class: ApplyExitClass::Usage,
            message: message.into(),
        }
    }

    fn operational(message: impl Into<String>) -> Self {
        Self {
            class: ApplyExitClass::Operational,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity {
    device: u64,
    inode: u64,
    kind: FileType,
    mode: u32,
    nlink: u64,
    size: u64,
    modified_ns: i128,
    changed_ns: i128,
}

impl Identity {
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            kind: FileType::from_raw_mode(stat.st_mode),
            mode: stat.st_mode as u32 & 0o7777,
            nlink: stat.st_nlink as u64,
            size: u64::try_from(stat.st_size).unwrap_or(0),
            modified_ns: timestamp_ns(stat.st_mtime as i64, stat.st_mtime_nsec as i64),
            changed_ns: timestamp_ns(stat.st_ctime as i64, stat.st_ctime_nsec as i64),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Captured {
    prestate: NodePrestate,
    bytes: Option<Vec<u8>>,
    identity: Option<Identity>,
}

#[derive(Debug)]
enum JournalEntry {
    CreatedFile {
        path: String,
        fd: Option<OwnedFd>,
        initial: Option<Identity>,
        expected: NodePrestate,
        post: Option<Captured>,
    },
    CreatedDirectory {
        path: String,
        temporary: OsString,
        identity: Option<Identity>,
        fd: Option<OwnedFd>,
        location: DirectoryLocation,
    },
    Replaced {
        path: String,
        original: Box<Captured>,
        intended: NodePrestate,
        post: Option<Box<Captured>>,
        temporary: OsString,
        temporary_fd: Option<OwnedFd>,
        temporary_initial: Option<Identity>,
        installed: bool,
    },
    ParentMode {
        path: String,
        fd: OwnedFd,
        identity: Identity,
        original_mode: u32,
        active: bool,
    },
    Removed {
        path: String,
        original: Captured,
        removed: bool,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryLocation {
    UnknownTemporary,
    Temporary,
    Installed,
}

trait ApplyHooks {
    fn usage_failure_before_operation(&mut self, _index: usize, _path: &str) -> Option<String> {
        None
    }
    fn before_operation(&mut self, _index: usize, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn after_operation(&mut self, _index: usize, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn before_parent_verification(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn before_create_file_open(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn after_mkdir_journaled(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn after_mkdir_identified(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn before_directory_install(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn after_file_open_journaled(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn after_rename_journaled(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn before_replace_install(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn after_parent_widened(&mut self, _path: &str) -> Result<(), String> {
        Ok(())
    }
    fn before_final_validation(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn during_final_validation(&mut self) -> Result<(), String> {
        Ok(())
    }
    fn after_failure(&mut self) {}
    fn file_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        rfs::fsync(fd)
    }
    fn directory_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        rfs::fsync(fd)
    }
    fn restore_parent_mode(&mut self, fd: BorrowedFd<'_>, mode: Mode) -> Result<(), Errno> {
        rfs::fchmod(fd, mode)
    }
    fn parent_mode_fsync(&mut self, fd: BorrowedFd<'_>, _recovery: bool) -> Result<(), Errno> {
        rfs::fsync(fd)
    }
    fn replaced_temp_cleanup_fsync(&mut self, fd: BorrowedFd<'_>) -> Result<(), Errno> {
        rfs::fsync(fd)
    }
    fn remove_file(&mut self, parent: BorrowedFd<'_>, name: &OsStr) -> Result<(), Errno> {
        rfs::unlinkat(parent, name, AtFlags::empty())
    }
    fn install_directory(
        &mut self,
        old_parent: BorrowedFd<'_>,
        old_name: &OsStr,
        new_parent: BorrowedFd<'_>,
        new_name: &OsStr,
    ) -> Result<(), Errno> {
        rfs::renameat_with(
            old_parent,
            old_name,
            new_parent,
            new_name,
            RenameFlags::NOREPLACE,
        )
    }
    fn remove_restore_temporary(
        &mut self,
        parent: BorrowedFd<'_>,
        name: &OsStr,
    ) -> Result<(), Errno> {
        rfs::unlinkat(parent, name, AtFlags::empty())
    }
    fn before_final_root_verification(&mut self) -> Result<(), String> {
        Ok(())
    }

    fn acquire_wiki_lock(&mut self, corpus: &Path) -> Result<crate::LockLease, crate::LockError> {
        crate::acquire_lock(corpus, crate::LockMode::Exclusive)
    }
}

struct SystemHooks;
impl ApplyHooks for SystemHooks {}

struct CreateFileFailure {
    failure: ApplyFailure,
    owned: Option<Identity>,
}

struct PinnedRoot {
    fd: OwnedFd,
    parent: OwnedFd,
    name: OsString,
    identity: Identity,
}

impl PinnedRoot {
    fn open(root: &Path) -> Result<Self, ApplyFailure> {
        let canonical = root.canonicalize().map_err(|error| {
            ApplyFailure::operational(format!("repository root cannot be canonicalized: {error}"))
        })?;
        if canonical != root {
            return Err(ApplyFailure::usage(
                "manifest root no longer names its exact canonical path",
            ));
        }
        let parent_path = root
            .parent()
            .ok_or_else(|| ApplyFailure::usage("repository root has no parent"))?;
        let name = root
            .file_name()
            .ok_or_else(|| ApplyFailure::usage("repository root has no final component"))?
            .to_os_string();
        let parent = rfs::open(parent_path, DIRECTORY_FLAGS, Mode::empty())
            .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
        let named = named_identity(parent.as_fd(), &name)
            .map_err(ApplyFailure::operational)?
            .ok_or_else(|| ApplyFailure::operational("repository root disappeared"))?;
        let fd = rfs::openat(parent.as_fd(), &name, DIRECTORY_FLAGS, Mode::empty())
            .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
        let opened = descriptor_identity(fd.as_fd()).map_err(ApplyFailure::operational)?;
        if named != opened || !opened.kind.is_dir() {
            return Err(ApplyFailure::usage(
                "repository root changed while it was pinned",
            ));
        }
        Ok(Self {
            fd,
            parent,
            name,
            identity: opened,
        })
    }

    fn verify_classified(&self) -> Result<(), ApplyFailure> {
        let descriptor = descriptor_identity(self.fd.as_fd()).map_err(ApplyFailure::operational)?;
        let named =
            named_identity(self.parent.as_fd(), &self.name).map_err(ApplyFailure::operational)?;
        if !same_binding(descriptor, self.identity)
            || !named.is_some_and(|named| same_binding(named, self.identity))
        {
            return Err(ApplyFailure::usage("repository root binding drifted"));
        }
        Ok(())
    }
}

pub fn apply_manifest(envelope: &ManifestEnvelope) -> ApplyResult {
    apply_manifest_classified(envelope).result
}

/// Applies a manifest while preserving whether a failure was a refusal or an
/// operational error. Only `result` belongs to the public JSON contract.
pub fn apply_manifest_classified(envelope: &ManifestEnvelope) -> ApplyOutcome {
    let manifest_sha256 = envelope.manifest_sha256.clone();
    if let Err(error) = validate_manifest(envelope) {
        return ApplyOutcome {
            result: failed_result(manifest_sha256, LayoutClass::Partial, error),
            class: ApplyExitClass::Usage,
        };
    }
    let mut hooks = SystemHooks;
    apply_manifest_classified_with_hooks(envelope, &mut hooks)
}

#[cfg(test)]
fn apply_manifest_with_hooks(
    envelope: &ManifestEnvelope,
    hooks: &mut impl ApplyHooks,
) -> ApplyResult {
    apply_manifest_classified_with_hooks(envelope, hooks).result
}

fn apply_manifest_classified_with_hooks(
    envelope: &ManifestEnvelope,
    hooks: &mut impl ApplyHooks,
) -> ApplyOutcome {
    let manifest = &envelope.manifest;
    let root_path = Path::new(&manifest.root);
    let mut result = failed_result(
        envelope.manifest_sha256.clone(),
        LayoutClass::Partial,
        String::new(),
    );
    let root = match PinnedRoot::open(root_path) {
        Ok(root) => root,
        Err(failure) => return failure_outcome(result, failure),
    };
    let initial = match capture_repository(root_path) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            let class = match error {
                InitError::InvalidRequest(_)
                | InitError::Conflict(_)
                | InitError::Drift(_)
                | InitError::Candidate(_)
                | InitError::Apply(_)
                | InitError::Json(_) => ApplyExitClass::Usage,
                InitError::Io { .. } | InitError::Git(_) | InitError::InvalidRoot(_) => {
                    ApplyExitClass::Operational
                }
            };
            return failure_outcome(
                result,
                ApplyFailure {
                    class,
                    message: error.to_string(),
                },
            );
        }
    };
    if let Err(failure) = root.verify_classified() {
        return failure_outcome(result, failure);
    }
    if initial.inspection.root != manifest.root
        || initial.inspection.inspection_sha256 != manifest.inspection_sha256
    {
        result.error = Some("approved repository inspection drifted before apply".to_owned());
        result.final_layout = initial.inspection.layout;
        return ApplyOutcome {
            result,
            class: ApplyExitClass::Usage,
        };
    }
    let mut wiki_lock = match acquire_existing_memory_lock(root_path, manifest, hooks) {
        Ok(guard) => guard,
        Err(failure) => return failure_outcome(result, failure),
    };
    let mut created_runtime_lock = false;
    let mut parent_pins = match capture_parent_pins(&root, manifest) {
        Ok(pins) => pins,
        Err(failure) => return failure_outcome(result, failure),
    };
    for operation in &manifest.operations {
        let actual = match capture_relative(&root, &parent_pins, &operation.path) {
            Ok(actual) => actual,
            Err(failure) => return failure_outcome(result, failure),
        };
        if actual.prestate != operation.prestate {
            result.error = Some(format!(
                "approved repository state drifted at {}",
                operation.path
            ));
            return ApplyOutcome {
                result,
                class: ApplyExitClass::Usage,
            };
        }
    }
    let approved_candidate = match reconstruct_candidate(&initial, manifest) {
        Ok(candidate) => candidate,
        Err(error) => {
            result.error = Some(error);
            result.final_layout = initial.inspection.layout;
            return ApplyOutcome {
                result,
                class: ApplyExitClass::Usage,
            };
        }
    };
    if let Err(failure) = root.verify_classified() {
        result.final_layout = initial.inspection.layout;
        return failure_outcome(result, failure);
    }
    let mut runtime_lock = match capture_relative(&root, &parent_pins, ".agents/memory/.write.lock")
    {
        Ok(lock) => lock,
        Err(failure) => {
            result.final_layout = initial.inspection.layout;
            return failure_outcome(result, failure);
        }
    };
    let initial_runtime_lock = runtime_lock.clone();

    let mut journal = Vec::new();
    let mut failure: Option<ApplyFailure> = None;
    for (index, operation) in manifest.operations.iter().enumerate() {
        if let Some(error) = hooks.usage_failure_before_operation(index, &operation.path) {
            failure = Some(ApplyFailure::usage(error));
            break;
        }
        if let Err(error) = hooks.before_operation(index, &operation.path) {
            failure = Some(ApplyFailure::operational(error));
            break;
        }
        if let Err(error) = root.verify_classified() {
            failure = Some(error);
            break;
        }
        if let Err(error) = hooks.before_parent_verification(&operation.path) {
            failure = Some(ApplyFailure::operational(error));
            break;
        }
        if let Err(error) = verify_parent_pins(&root, &parent_pins, &operation.path) {
            failure = Some(error);
            break;
        }
        if let Err(error) = verify_expected(&root, &parent_pins, &operation.prestate) {
            failure = Some(error);
            break;
        }
        if let Err(error) = apply_operation(&root, &mut parent_pins, operation, hooks, &mut journal)
        {
            failure = Some(error);
            break;
        }
        if wiki_lock.is_none() && operation.path == ".agents/memory" {
            match take_memory_lock(root_path, hooks) {
                Ok(guard) => {
                    wiki_lock = Some(guard);
                    created_runtime_lock = true;
                    match capture_relative(&root, &parent_pins, ".agents/memory/.write.lock") {
                        Ok(updated) => runtime_lock = updated,
                        Err(error) => {
                            failure = Some(error);
                            break;
                        }
                    }
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }
        match operation.kind {
            OperationKind::CreateDirectory | OperationKind::CreateFile => {
                result.created.push(operation.path.clone())
            }
            OperationKind::ReplaceFile => result.changed.push(operation.path.clone()),
            OperationKind::RemoveFile => result.removed.push(operation.path.clone()),
        }
        if let Err(error) = hooks.after_operation(index, &operation.path) {
            failure = Some(ApplyFailure::operational(error));
            break;
        }
    }

    if failure.is_none() {
        failure = hooks
            .before_final_validation()
            .err()
            .map(ApplyFailure::operational);
    }
    if failure.is_none()
        && let Err(error) = finalize_and_validate(
            &root,
            &parent_pins,
            manifest,
            &approved_candidate,
            &runtime_lock,
            hooks,
        )
    {
        failure = Some(error);
    }
    if let Some(failure) = failure {
        if created_runtime_lock {
            if let Some(guard) = wiki_lock.take() {
                let _ = rfs::unlinkat(guard.corpus_fd(), crate::LOCK_NAME, AtFlags::empty());
                drop(guard);
            }
            runtime_lock = initial_runtime_lock;
        }
        hooks.after_failure();
        account_journal(&journal, &mut result);
        let mut recovery_failure =
            recover(&root, &mut parent_pins, &mut journal, hooks, &mut result);
        result.error = Some(failure.message.clone());
        let (layout, drift, classification_failure) =
            classify_recovered_layout(&root, &parent_pins, &runtime_lock, hooks);
        if let Some(classification_failure) = classification_failure {
            merge_recovery_failure(&mut recovery_failure, classification_failure);
        }
        result.unresolved.extend(drift);
        result.final_layout = if result.unresolved.is_empty() {
            layout
        } else {
            LayoutClass::Partial
        };
        if let Some(recovery) = recovery_failure.as_ref() {
            result.error = Some(format!(
                "{}; recovery: {}",
                failure.message, recovery.message
            ));
        }
        normalize_result(&mut result);
        return ApplyOutcome {
            result,
            class: recovery_failure.map_or(failure.class, |recovery| {
                if recovery.class == ApplyExitClass::Operational {
                    ApplyExitClass::Operational
                } else {
                    failure.class
                }
            }),
        };
    }

    result.ok = true;
    result.validated = true;
    result.error = None;
    result.next = vec!["yams --index".to_owned()];
    result.final_layout = match manifest.mode {
        InitMode::Minimal => LayoutClass::Minimal,
        InitMode::Full => LayoutClass::Full,
    };
    normalize_result(&mut result);
    drop(wiki_lock);
    ApplyOutcome {
        result,
        class: ApplyExitClass::Success,
    }
}

fn memory_corpus(root: &Path) -> std::path::PathBuf {
    root.join(".agents/memory")
}

fn acquire_existing_memory_lock(
    root: &Path,
    _manifest: &super::InitManifest,
    hooks: &mut impl ApplyHooks,
) -> Result<Option<crate::LockGuard>, ApplyFailure> {
    let corpus = memory_corpus(root);
    match std::fs::symlink_metadata(&corpus) {
        Ok(metadata) if metadata.is_dir() => take_memory_lock(root, hooks).map(Some),
        Ok(_) => Err(ApplyFailure::operational(format!(
            "wiki lock corpus {} is not a directory",
            corpus.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(ApplyFailure::operational(format!(
            "cannot inspect wiki lock corpus {}: {error}",
            corpus.display()
        ))),
    }
}

fn take_memory_lock(
    root: &Path,
    hooks: &mut impl ApplyHooks,
) -> Result<crate::LockGuard, ApplyFailure> {
    let corpus = memory_corpus(root);
    match hooks.acquire_wiki_lock(&corpus) {
        Ok(crate::LockLease::Isolated(guard)) => Ok(guard),
        Ok(crate::LockLease::Unisolated(unisolated)) => Err(ApplyFailure::operational(format!(
            "the corpus is not writable: {:?}",
            unisolated.reason
        ))),
        Err(error) => Err(ApplyFailure::operational(error.to_string())),
    }
}

fn validate_manifest(envelope: &ManifestEnvelope) -> Result<(), String> {
    let manifest = &envelope.manifest;
    if !envelope.ok {
        return Err("manifest envelope is not applicable".to_owned());
    }
    let digest = sha256(&canonical_manifest_bytes(manifest).map_err(|error| error.to_string())?);
    if !is_digest(&envelope.manifest_sha256) || digest != envelope.manifest_sha256 {
        return Err("manifest digest does not match its canonical contents".to_owned());
    }
    if manifest.manifest_contract != 1
        || manifest.layout_version != LAYOUT_VERSION
        || manifest.yams_version != env!("CARGO_PKG_VERSION")
    {
        return Err("manifest contract, layout, or Yams version is unsupported".to_owned());
    }
    if manifest.asset_sha256 != expected_asset_digests() {
        return Err("manifest asset digests do not match this Yams binary".to_owned());
    }
    validate_absolute_root(&manifest.root)?;
    if !is_digest(&manifest.inspection_sha256) || !is_digest(&manifest.candidate_sha256) {
        return Err("manifest contains an invalid digest".to_owned());
    }
    let max_operations = match manifest.mode {
        InitMode::Minimal => 5,
        InitMode::Full => 9,
    };
    if manifest.operations.len() > max_operations {
        return Err("manifest contains too many operations".to_owned());
    }
    let mut seen = BTreeSet::new();
    for operation in &manifest.operations {
        validate_operation(operation, manifest.mode)?;
        if !seen.insert(operation.path.clone()) {
            return Err(format!(
                "manifest repeats operation path {}",
                operation.path
            ));
        }
    }
    let mut sorted = manifest.operations.clone();
    sort_operations(&mut sorted);
    if sorted != manifest.operations {
        return Err("manifest operations are not in canonical order".to_owned());
    }
    let proposal = manifest
        .operations
        .iter()
        .map(proposal_line)
        .collect::<Vec<_>>()
        .join("\n");
    if proposal != manifest.proposal {
        return Err("manifest proposal does not match its operations".to_owned());
    }
    Ok(())
}

fn validate_absolute_root(root: &str) -> Result<(), String> {
    let path = Path::new(root);
    if !path.is_absolute() || root.as_bytes().contains(&0) {
        return Err("manifest root must be an absolute UTF-8 path".to_owned());
    }
    for component in path.components() {
        if matches!(component, Component::CurDir | Component::ParentDir) {
            return Err("manifest root must be lexically canonical".to_owned());
        }
    }
    if root != "/" && root.ends_with('/') {
        return Err("manifest root must be lexically canonical".to_owned());
    }
    if root != "/"
        && root[1..]
            .split('/')
            .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("manifest root must be lexically canonical".to_owned());
    }
    Ok(())
}

fn validate_operation(operation: &InitOperation, mode: InitMode) -> Result<(), String> {
    validate_relative_path(&operation.path)?;
    if operation.prestate.path != operation.path {
        return Err("operation and prestate paths differ".to_owned());
    }
    if !mutable_operation_path(&operation.path, mode) {
        return Err(format!(
            "operation path is outside the owned layout: {}",
            operation.path
        ));
    }
    validate_prestate(&operation.prestate)?;
    match operation.kind {
        OperationKind::CreateDirectory => {
            if operation.prestate.kind != NodeKind::Missing
                || operation.mode.is_none()
                || operation.content.is_some()
                || operation.post_sha256.is_some()
                || !directory_path(&operation.path)
                || operation.mode != Some(0o755)
            {
                return Err("create-directory operation is internally inconsistent".to_owned());
            }
        }
        OperationKind::CreateFile => {
            if operation.prestate.kind != NodeKind::Missing
                || operation.mode.is_none()
                || operation.content.is_none()
                || !post_digest_matches(operation)
                || directory_path(&operation.path)
                || operation.mode != Some(0o644)
            {
                return Err("create-file operation is internally inconsistent".to_owned());
            }
        }
        OperationKind::ReplaceFile => {
            if operation.prestate.kind != NodeKind::File
                || operation.mode.is_none()
                || operation.content.is_none()
                || !post_digest_matches(operation)
                || directory_path(&operation.path)
                || operation.mode != operation.prestate.mode
            {
                return Err("replace-file operation is internally inconsistent".to_owned());
            }
        }
        OperationKind::RemoveFile => {
            if mode != InitMode::Full
                || operation.path != ".agents/memory/project-context.md"
                || operation.prestate.kind != NodeKind::File
                || operation.mode.is_some()
                || operation.content.is_some()
                || operation.post_sha256.is_some()
            {
                return Err("remove-file operation is internally inconsistent".to_owned());
            }
        }
    }
    if operation.mode.is_some_and(|mode| mode > 0o7777) {
        return Err("operation mode is invalid".to_owned());
    }
    if operation
        .content
        .as_ref()
        .is_some_and(|content| content.len() as u64 > yams_core::MAX_FILE_BYTES)
    {
        return Err("operation content exceeds the supported file size".to_owned());
    }
    validate_operation_content(operation)?;
    Ok(())
}

fn validate_operation_content(operation: &InitOperation) -> Result<(), String> {
    let Some(content) = operation.content.as_deref() else {
        return Ok(());
    };
    match operation.path.as_str() {
        "AGENTS.md" => {
            let policy = inspect_policy(content);
            if policy.heading_count != 1 || !policy.exact {
                return Err("planned AGENTS.md does not contain the canonical policy".to_owned());
            }
        }
        ".agents/memory/SCHEMA.md" if content.as_bytes() != SCHEMA.as_bytes() => {
            return Err("planned SCHEMA.md differs from the embedded asset".to_owned());
        }
        ".agents/memory/.gitignore" if content.as_bytes() != crate::MEMORY_GITIGNORE.as_bytes() => {
            return Err("planned memory gitignore differs from the embedded asset".to_owned());
        }
        ".agents/memory/project-context.md" | ".agents/memory/pages/project-context.md" => {
            let page = crate::parse_wiki_page(content)
                .map_err(|error| format!("planned project page is invalid: {error}"))?;
            if page.slug != "project-context" {
                return Err("planned project page has the wrong slug".to_owned());
            }
        }
        _ => {}
    }
    Ok(())
}

fn validate_prestate(prestate: &NodePrestate) -> Result<(), String> {
    match prestate.kind {
        NodeKind::Missing
            if prestate.mode.is_none()
                && prestate.sha256.is_none()
                && prestate.entries_sha256.is_none() =>
        {
            Ok(())
        }
        NodeKind::File
            if prestate.mode.is_some()
                && prestate.sha256.as_deref().is_some_and(is_digest)
                && prestate.entries_sha256.is_none() =>
        {
            Ok(())
        }
        NodeKind::Directory
            if prestate.mode.is_some()
                && prestate.sha256.is_none()
                && prestate.entries_sha256.as_deref().is_some_and(is_digest) =>
        {
            Ok(())
        }
        _ => Err("operation prestate is internally inconsistent".to_owned()),
    }
}

fn validate_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with('/') || path.ends_with('/') || path.contains("//") {
        return Err("operation path is not a confined relative path".to_owned());
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err("operation path is not a confined relative path".to_owned());
    }
    Ok(())
}

fn post_digest_matches(operation: &InitOperation) -> bool {
    operation
        .content
        .as_ref()
        .zip(operation.post_sha256.as_ref())
        .is_some_and(|(content, digest)| is_digest(digest) && sha256(content.as_bytes()) == *digest)
}

fn expected_asset_digests() -> BTreeMap<String, String> {
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

fn is_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn directory_path(path: &str) -> bool {
    matches!(path, ".agents" | ".agents/memory" | ".agents/memory/pages")
}
fn mutable_operation_path(path: &str, mode: InitMode) -> bool {
    matches!(path, "AGENTS.md" | ".agents" | ".agents/memory")
        || path == ".agents/memory/.gitignore"
        || (mode == InitMode::Minimal && path == ".agents/memory/project-context.md")
        || (mode == InitMode::Full
            && (matches!(
                path,
                ".agents/memory/project-context.md"
                    | ".agents/memory/SCHEMA.md"
                    | ".agents/memory/INDEX.md"
                    | ".agents/memory/pages"
                    | ".agents/memory/pages/project-context.md"
            )))
}

fn candidate_owned_path(path: &str, mode: InitMode) -> bool {
    matches!(
        path,
        "AGENTS.md" | ".agents" | ".agents/memory" | ".agents/memory/.gitignore"
    ) || (mode == InitMode::Minimal && path == ".agents/memory/project-context.md")
        || (mode == InitMode::Full
            && (matches!(
                path,
                ".agents/memory/SCHEMA.md" | ".agents/memory/INDEX.md" | ".agents/memory/pages"
            ) || path
                .strip_prefix(".agents/memory/pages/")
                .is_some_and(|name| {
                    !name.is_empty() && !name.contains('/') && name.ends_with(".md")
                })))
}

fn reconstruct_candidate(
    initial: &super::inspect::RepositorySnapshot,
    manifest: &InitManifest,
) -> Result<BTreeMap<String, DesiredNode>, String> {
    let desired = build_candidate(initial, manifest)?;
    validate_owned_candidate(&desired, manifest.mode).map_err(|error| error.to_string())?;
    if owned_candidate_sha256(&desired) != manifest.candidate_sha256 {
        return Err(
            "approved candidate digest does not match the reconstructed candidate".to_owned(),
        );
    }
    Ok(desired)
}

fn build_candidate(
    initial: &super::inspect::RepositorySnapshot,
    manifest: &InitManifest,
) -> Result<BTreeMap<String, DesiredNode>, String> {
    let mut desired = BTreeMap::new();
    for prestate in &initial.inspection.prestates {
        if prestate.kind == NodeKind::Missing
            || prestate.path == ".agents/memory/.write.lock"
            || !portable_owned_path(&prestate.path)
        {
            continue;
        }
        match prestate.kind {
            NodeKind::Directory => {
                desired.insert(
                    prestate.path.clone(),
                    DesiredNode::Directory {
                        mode: prestate.mode.ok_or_else(|| {
                            format!("captured directory has no mode: {}", prestate.path)
                        })?,
                    },
                );
            }
            NodeKind::File => {
                desired.insert(
                    prestate.path.clone(),
                    DesiredNode::File {
                        mode: prestate.mode.ok_or_else(|| {
                            format!("captured file has no mode: {}", prestate.path)
                        })?,
                        bytes: initial
                            .contents
                            .get(&prestate.path)
                            .cloned()
                            .ok_or_else(|| {
                                format!("captured file has no bytes: {}", prestate.path)
                            })?,
                    },
                );
            }
            NodeKind::Missing | NodeKind::Symlink | NodeKind::Other => {
                return Err(format!("unsafe retained owned node: {}", prestate.path));
            }
        }
    }
    for operation in &manifest.operations {
        match operation.kind {
            OperationKind::CreateDirectory => {
                desired.insert(
                    operation.path.clone(),
                    DesiredNode::Directory {
                        mode: operation.mode.ok_or_else(|| {
                            format!(
                                "validated directory operation has no mode: {}",
                                operation.path
                            )
                        })?,
                    },
                );
            }
            OperationKind::CreateFile | OperationKind::ReplaceFile => {
                desired.insert(
                    operation.path.clone(),
                    DesiredNode::File {
                        mode: operation.mode.ok_or_else(|| {
                            format!("validated file operation has no mode: {}", operation.path)
                        })?,
                        bytes: operation
                            .content
                            .as_ref()
                            .ok_or_else(|| {
                                format!(
                                    "validated file operation has no content: {}",
                                    operation.path
                                )
                            })?
                            .as_bytes()
                            .to_vec(),
                    },
                );
            }
            OperationKind::RemoveFile => {
                desired.remove(&operation.path);
            }
        }
    }
    Ok(desired)
}

fn portable_owned_path(path: &str) -> bool {
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

fn capture_parent_pins(
    root: &PinnedRoot,
    manifest: &InitManifest,
) -> Result<BTreeMap<String, Identity>, ApplyFailure> {
    let mut pins = BTreeMap::new();
    pins.insert(String::new(), root.identity);
    let mut paths = manifest
        .operations
        .iter()
        .map(|operation| operation.path.as_str())
        .collect::<Vec<_>>();
    paths.extend([".agents/memory/pages/.pin", ".agents/memory/.write.lock"]);
    for path in paths {
        let parts = path.split('/').collect::<Vec<_>>();
        let mut current = String::new();
        let mut fd = rustix::io::dup(root.fd.as_fd())
            .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
        for part in &parts[..parts.len() - 1] {
            let next = if current.is_empty() {
                (*part).to_owned()
            } else {
                format!("{current}/{part}")
            };
            match rfs::openat(fd.as_fd(), *part, DIRECTORY_FLAGS, Mode::empty()) {
                Ok(child) => {
                    let identity =
                        descriptor_identity(child.as_fd()).map_err(ApplyFailure::operational)?;
                    if !identity.kind.is_dir() {
                        return Err(ApplyFailure::usage(format!(
                            "parent {next} is not a directory"
                        )));
                    }
                    pins.entry(next.clone()).or_insert(identity);
                    fd = child;
                    current = next;
                }
                Err(Errno::NOENT) => break,
                Err(error) => {
                    return Err(parent_access_failure(
                        error,
                        format!("could not pin parent {next}"),
                    ));
                }
            }
        }
    }
    Ok(pins)
}

fn open_parent(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    path: &str,
) -> Result<(OwnedFd, OsString), ApplyFailure> {
    root.verify_classified()?;
    let mut parts = path.split('/').collect::<Vec<_>>();
    let name = OsString::from(
        parts
            .pop()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| ApplyFailure::operational(format!("path has no component: {path}")))?,
    );
    let mut fd = rustix::io::dup(root.fd.as_fd())
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    let mut current = String::new();
    for part in parts {
        current = if current.is_empty() {
            part.to_owned()
        } else {
            format!("{current}/{part}")
        };
        let child =
            rfs::openat(fd.as_fd(), part, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
                parent_access_failure(error, format!("could not open parent {current}"))
            })?;
        let identity = descriptor_identity(child.as_fd()).map_err(ApplyFailure::operational)?;
        if !pins
            .get(&current)
            .is_some_and(|expected| same_binding(*expected, identity))
        {
            return Err(ApplyFailure::usage(format!(
                "parent binding drifted at {current}"
            )));
        }
        fd = child;
    }
    Ok((fd, name))
}

fn open_parent_pinned(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    path: &str,
) -> Result<(OwnedFd, OsString), ApplyFailure> {
    if !same_binding(
        descriptor_identity(root.fd.as_fd()).map_err(ApplyFailure::operational)?,
        root.identity,
    ) {
        return Err(ApplyFailure::usage("pinned repository descriptor drifted"));
    }
    let mut parts = path.split('/').collect::<Vec<_>>();
    let name = OsString::from(
        parts
            .pop()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| ApplyFailure::operational(format!("path has no component: {path}")))?,
    );
    let mut fd = rustix::io::dup(root.fd.as_fd())
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    let mut current = String::new();
    for part in parts {
        current = if current.is_empty() {
            part.to_owned()
        } else {
            format!("{current}/{part}")
        };
        let child =
            rfs::openat(fd.as_fd(), part, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
                parent_access_failure(error, format!("could not open pinned parent {current}"))
            })?;
        let identity = descriptor_identity(child.as_fd()).map_err(ApplyFailure::operational)?;
        if !pins
            .get(&current)
            .is_some_and(|expected| same_binding(*expected, identity))
        {
            return Err(ApplyFailure::usage(format!(
                "pinned parent binding drifted at {current}"
            )));
        }
        fd = child;
    }
    Ok((fd, name))
}

fn verify_parent_pins(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    path: &str,
) -> Result<(), ApplyFailure> {
    open_parent(root, pins, path).map(|_| ())
}

fn capture_relative(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    path: &str,
) -> Result<Captured, ApplyFailure> {
    root.verify_classified()?;
    let mut parts = path.split('/').collect::<Vec<_>>();
    let name = OsString::from(
        parts
            .pop()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| ApplyFailure::operational(format!("path has no component: {path}")))?,
    );
    let mut fd = rustix::io::dup(root.fd.as_fd())
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    let mut current = String::new();
    for part in parts {
        current = if current.is_empty() {
            part.to_owned()
        } else {
            format!("{current}/{part}")
        };
        match rfs::openat(fd.as_fd(), part, DIRECTORY_FLAGS, Mode::empty()) {
            Ok(child) => {
                let identity =
                    descriptor_identity(child.as_fd()).map_err(ApplyFailure::operational)?;
                if !pins
                    .get(&current)
                    .is_some_and(|expected| same_binding(*expected, identity))
                {
                    return Err(ApplyFailure::usage(format!(
                        "parent binding drifted at {current}"
                    )));
                }
                fd = child;
            }
            Err(Errno::NOENT) => {
                return Ok(Captured {
                    prestate: NodePrestate {
                        path: path.to_owned(),
                        kind: NodeKind::Missing,
                        mode: None,
                        sha256: None,
                        entries_sha256: None,
                    },
                    bytes: None,
                    identity: None,
                });
            }
            Err(error) => {
                return Err(parent_access_failure(
                    error,
                    format!("could not open parent {current}"),
                ));
            }
        }
    }
    capture_named(fd.as_fd(), &name, path)
}

fn capture_relative_pinned(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    path: &str,
) -> Result<Captured, ApplyFailure> {
    if !same_binding(
        descriptor_identity(root.fd.as_fd()).map_err(ApplyFailure::operational)?,
        root.identity,
    ) {
        return Err(ApplyFailure::usage("pinned repository descriptor drifted"));
    }
    let mut parts = path.split('/').collect::<Vec<_>>();
    let name = OsString::from(
        parts
            .pop()
            .filter(|part| !part.is_empty())
            .ok_or_else(|| ApplyFailure::operational(format!("path has no component: {path}")))?,
    );
    let mut fd = rustix::io::dup(root.fd.as_fd())
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    let mut current = String::new();
    for part in parts {
        current = if current.is_empty() {
            part.to_owned()
        } else {
            format!("{current}/{part}")
        };
        match rfs::openat(fd.as_fd(), part, DIRECTORY_FLAGS, Mode::empty()) {
            Ok(child) => {
                let identity =
                    descriptor_identity(child.as_fd()).map_err(ApplyFailure::operational)?;
                if !pins
                    .get(&current)
                    .is_some_and(|expected| same_binding(*expected, identity))
                {
                    return Err(ApplyFailure::usage(format!(
                        "pinned parent binding drifted at {current}"
                    )));
                }
                fd = child;
            }
            Err(Errno::NOENT) => {
                return Ok(missing_capture(path));
            }
            Err(error) => {
                return Err(parent_access_failure(
                    error,
                    format!("could not open pinned parent {current}"),
                ));
            }
        }
    }
    capture_named(fd.as_fd(), &name, path)
}

fn missing_capture(path: &str) -> Captured {
    Captured {
        prestate: NodePrestate {
            path: path.to_owned(),
            kind: NodeKind::Missing,
            mode: None,
            sha256: None,
            entries_sha256: None,
        },
        bytes: None,
        identity: None,
    }
}

fn capture_named(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    path: &str,
) -> Result<Captured, ApplyFailure> {
    let Some(named) = named_identity(parent, name).map_err(ApplyFailure::operational)? else {
        return Ok(Captured {
            prestate: NodePrestate {
                path: path.to_owned(),
                kind: NodeKind::Missing,
                mode: None,
                sha256: None,
                entries_sha256: None,
            },
            bytes: None,
            identity: None,
        });
    };
    if named.kind.is_file() {
        if named.nlink != 1 || named.size > yams_core::MAX_FILE_BYTES {
            return Err(ApplyFailure::usage(format!(
                "unsafe regular file at {path}"
            )));
        }
        let fd = rfs::openat(parent, name, FILE_FLAGS, Mode::empty())
            .map_err(|error| named_access_failure(error, format!("could not open {path}")))?;
        if descriptor_identity(fd.as_fd()).map_err(ApplyFailure::operational)? != named {
            return Err(ApplyFailure::usage(format!(
                "file binding drifted at {path}"
            )));
        }
        let mut file = File::from(fd);
        let mut bytes = Vec::new();
        Read::by_ref(&mut file)
            .take(yams_core::MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| ApplyFailure::operational(error.to_string()))?;
        if descriptor_identity(file.as_fd()).map_err(ApplyFailure::operational)? != named
            || named_identity(parent, name).map_err(ApplyFailure::operational)? != Some(named)
            || bytes.len() as u64 != named.size
        {
            return Err(ApplyFailure::usage(format!(
                "file binding drifted at {path}"
            )));
        }
        Ok(Captured {
            prestate: NodePrestate {
                path: path.to_owned(),
                kind: NodeKind::File,
                mode: Some(named.mode),
                sha256: Some(sha256(&bytes)),
                entries_sha256: None,
            },
            bytes: Some(bytes),
            identity: Some(named),
        })
    } else if named.kind.is_dir() {
        let fd = rfs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty())
            .map_err(|error| named_access_failure(error, format!("could not open {path}")))?;
        if descriptor_identity(fd.as_fd()).map_err(ApplyFailure::operational)? != named {
            return Err(ApplyFailure::usage(format!(
                "directory binding drifted at {path}"
            )));
        }
        let first = directory_signatures(fd.as_fd())?;
        let second = directory_signatures(fd.as_fd())?;
        if first != second
            || descriptor_identity(fd.as_fd()).map_err(ApplyFailure::operational)? != named
            || named_identity(parent, name).map_err(ApplyFailure::operational)? != Some(named)
        {
            return Err(ApplyFailure::usage(format!(
                "directory binding drifted at {path}"
            )));
        }
        let mut encoded = Vec::new();
        for (entry, state) in &first {
            let bytes = entry.as_bytes();
            encoded.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            encoded.extend_from_slice(bytes);
            encoded.push(kind_tag(state.kind));
            encoded.extend_from_slice(&state.mode.to_be_bytes());
        }
        Ok(Captured {
            prestate: NodePrestate {
                path: path.to_owned(),
                kind: NodeKind::Directory,
                mode: Some(named.mode),
                sha256: None,
                entries_sha256: Some(sha256(&encoded)),
            },
            bytes: None,
            identity: Some(named),
        })
    } else {
        let kind = if named.kind.is_symlink() {
            NodeKind::Symlink
        } else {
            NodeKind::Other
        };
        Ok(Captured {
            prestate: NodePrestate {
                path: path.to_owned(),
                kind,
                mode: Some(named.mode),
                sha256: None,
                entries_sha256: None,
            },
            bytes: None,
            identity: Some(named),
        })
    }
}

fn verify_expected(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    expected: &NodePrestate,
) -> Result<(), ApplyFailure> {
    let actual = capture_relative(root, pins, &expected.path)?;
    if actual.prestate == *expected {
        Ok(())
    } else {
        Err(ApplyFailure::usage(format!(
            "approved repository state drifted at {}",
            expected.path
        )))
    }
}

fn required_mode(operation: &InitOperation) -> Result<u32, ApplyFailure> {
    operation.mode.ok_or_else(|| {
        ApplyFailure::operational(format!(
            "validated operation has no mode: {}",
            operation.path
        ))
    })
}

fn required_content(operation: &InitOperation) -> Result<&str, ApplyFailure> {
    operation.content.as_deref().ok_or_else(|| {
        ApplyFailure::operational(format!(
            "validated operation has no content: {}",
            operation.path
        ))
    })
}

fn apply_operation(
    root: &PinnedRoot,
    pins: &mut BTreeMap<String, Identity>,
    operation: &InitOperation,
    hooks: &mut impl ApplyHooks,
    journal: &mut Vec<JournalEntry>,
) -> Result<(), ApplyFailure> {
    let (parent, name) = open_parent(root, pins, &operation.path)?;
    let parent_path = operation.path.rsplit_once('/').map_or("", |(path, _)| path);
    let parent_mode = make_parent_writable(parent.as_fd(), parent_path, journal)?;
    if parent_mode.is_some() {
        hooks
            .after_parent_widened(parent_path)
            .map_err(ApplyFailure::operational)?;
    }
    let outcome: Result<(), ApplyFailure> = (|| {
        match operation.kind {
            OperationKind::CreateDirectory => {
                let (temporary, record) = (0..128)
                    .find_map(|_| {
                        let temporary = match unique_directory_temp(parent.as_fd(), &name) {
                            Ok(temporary) => temporary,
                            Err(failure) => return Some(Err(failure)),
                        };
                        journal.push(JournalEntry::CreatedDirectory {
                            path: operation.path.clone(),
                            temporary: temporary.clone(),
                            identity: None,
                            fd: None,
                            location: DirectoryLocation::UnknownTemporary,
                        });
                        match rfs::mkdirat(parent.as_fd(), &temporary, Mode::from_raw_mode(0o700)) {
                            Ok(()) => Some(Ok((temporary, journal.len() - 1))),
                            Err(Errno::EXIST) => {
                                journal.pop();
                                None
                            }
                            Err(error) => {
                                journal.pop();
                                Some(Err(ApplyFailure::operational(format!(
                                    "could not create directory temporary for {}: {error}",
                                    operation.path
                                ))))
                            }
                        }
                    })
                    .unwrap_or_else(|| {
                        Err(ApplyFailure::operational(format!(
                            "could not allocate a private directory temporary for {}",
                            operation.path
                        )))
                    })?;
                hooks
                    .after_mkdir_journaled(&operation.path)
                    .map_err(ApplyFailure::operational)?;
                let tentative = named_identity(parent.as_fd(), &temporary)
                    .map_err(ApplyFailure::operational)?
                    .ok_or_else(|| {
                        ApplyFailure::usage(format!(
                            "created directory temporary disappeared: {}",
                            operation.path
                        ))
                    })?;
                let fd = rfs::openat(parent.as_fd(), &temporary, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|error| {
                        named_access_failure(
                            error,
                            format!("could not open directory temporary for {}", operation.path),
                        )
                    })?;
                let identity =
                    descriptor_identity(fd.as_fd()).map_err(ApplyFailure::operational)?;
                if !same_binding(identity, tentative)
                    || !named_identity(parent.as_fd(), &temporary)
                        .map_err(ApplyFailure::operational)?
                        .is_some_and(|named| same_binding(named, identity))
                {
                    return Err(ApplyFailure::usage(format!(
                        "created directory temporary was rebound: {}",
                        operation.path
                    )));
                }
                let JournalEntry::CreatedDirectory {
                    identity: recorded_identity,
                    fd: recorded_fd,
                    location,
                    ..
                } = &mut journal[record]
                else {
                    return Err(ApplyFailure::operational(
                        "apply journal invariant failed: directory journal record index is stable",
                    ));
                };
                *recorded_identity = Some(identity);
                *recorded_fd = Some(fd);
                *location = DirectoryLocation::Temporary;
                let JournalEntry::CreatedDirectory {
                    fd: Some(created), ..
                } = &journal[record]
                else {
                    return Err(ApplyFailure::operational(
                        "apply journal invariant failed: created directory descriptor was recorded",
                    ));
                };
                rfs::fchmod(
                    created.as_fd(),
                    Mode::from_raw_mode(required_mode(operation)? as _),
                )
                .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
                hooks
                    .before_directory_install(&operation.path)
                    .map_err(ApplyFailure::operational)?;
                if !named_identity(parent.as_fd(), &temporary)
                    .map_err(ApplyFailure::operational)?
                    .is_some_and(|named| same_binding(named, identity))
                    || !same_binding(
                        descriptor_identity(created.as_fd()).map_err(ApplyFailure::operational)?,
                        identity,
                    )
                {
                    return Err(ApplyFailure::usage(format!(
                        "created directory temporary drifted before install: {}",
                        operation.path
                    )));
                }
                hooks
                    .install_directory(parent.as_fd(), &temporary, parent.as_fd(), &name)
                    .map_err(|error| {
                        classify_failed_captured_mutation(
                            error,
                            parent.as_fd(),
                            &name,
                            &operation.path,
                            &missing_capture(&operation.path),
                            "install directory",
                        )
                    })?;
                let JournalEntry::CreatedDirectory { location, .. } = &mut journal[record] else {
                    return Err(ApplyFailure::operational(
                        "apply journal invariant failed: directory journal record index is stable",
                    ));
                };
                *location = DirectoryLocation::Installed;
                hooks
                    .after_mkdir_identified(&operation.path)
                    .map_err(ApplyFailure::operational)?;
                if !named_identity(parent.as_fd(), &name)
                    .map_err(ApplyFailure::operational)?
                    .is_some_and(|named| same_binding(named, identity))
                    || named_identity(parent.as_fd(), &temporary)
                        .map_err(ApplyFailure::operational)?
                        .is_some()
                {
                    return Err(ApplyFailure::usage(format!(
                        "created directory was rebound: {}",
                        operation.path
                    )));
                }
                pins.insert(operation.path.clone(), identity);
                hooks
                    .directory_fsync(parent.as_fd())
                    .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
            }
            OperationKind::CreateFile => {
                journal.push(JournalEntry::CreatedFile {
                    path: operation.path.clone(),
                    fd: None,
                    initial: None,
                    expected: expected_file_prestate(operation),
                    post: None,
                });
                let record = journal.len() - 1;
                hooks
                    .before_create_file_open(&operation.path)
                    .map_err(ApplyFailure::operational)?;
                let fd = match rfs::openat(
                    parent.as_fd(),
                    &name,
                    CREATE_FLAGS,
                    Mode::from_raw_mode(0o600),
                ) {
                    Ok(fd) => fd,
                    Err(error) => {
                        journal.pop();
                        return Err(exclusive_target_failure(
                            error,
                            format!("could not create {}", operation.path),
                        ));
                    }
                };
                let JournalEntry::CreatedFile { fd: recorded, .. } = &mut journal[record] else {
                    return Err(ApplyFailure::operational(
                        "apply journal invariant failed: file journal record index is stable",
                    ));
                };
                *recorded = Some(fd);
                let JournalEntry::CreatedFile {
                    fd: Some(created), ..
                } = &journal[record]
                else {
                    return Err(ApplyFailure::operational(
                        "apply journal invariant failed: created file descriptor was recorded",
                    ));
                };
                let owned =
                    descriptor_identity(created.as_fd()).map_err(ApplyFailure::operational)?;
                let JournalEntry::CreatedFile { initial, .. } = &mut journal[record] else {
                    return Err(ApplyFailure::operational(
                        "apply journal invariant failed: file journal record index is stable",
                    ));
                };
                *initial = Some(owned);
                hooks
                    .after_file_open_journaled(&operation.path)
                    .map_err(ApplyFailure::operational)?;
                let JournalEntry::CreatedFile {
                    fd: Some(created), ..
                } = &journal[record]
                else {
                    return Err(ApplyFailure::operational(
                        "apply journal invariant failed: created file descriptor was recorded",
                    ));
                };
                if !named_identity(parent.as_fd(), &name)
                    .map_err(ApplyFailure::operational)?
                    .is_some_and(|named| same_binding(named, owned))
                {
                    return Err(ApplyFailure::usage(format!(
                        "created path was rebound at {}",
                        operation.path
                    )));
                }
                rfs::fchmod(
                    created.as_fd(),
                    Mode::from_raw_mode(required_mode(operation)? as _),
                )
                .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
                let duplicate = rustix::io::dup(created.as_fd())
                    .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
                File::from(duplicate)
                    .write_all(required_content(operation)?.as_bytes())
                    .map_err(|error| ApplyFailure::operational(error.to_string()))?;
                hooks
                    .file_fsync(created.as_fd())
                    .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
                let actual = capture_named(parent.as_fd(), &name, &operation.path)?;
                let owned_match = actual
                    .identity
                    .is_some_and(|identity| same_binding(identity, owned));
                if !owned_match {
                    return Err(ApplyFailure::usage(format!(
                        "created path was rebound at {}",
                        operation.path
                    )));
                }
                ensure_post(operation, &actual)?;
                let JournalEntry::CreatedFile { post, .. } = &mut journal[record] else {
                    return Err(ApplyFailure::operational(
                        "apply journal invariant failed: file journal record index is stable",
                    ));
                };
                *post = Some(actual);
                hooks
                    .directory_fsync(parent.as_fd())
                    .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
            }
            OperationKind::ReplaceFile => {
                let original = capture_named(parent.as_fd(), &name, &operation.path)?;
                let installed = replace_file(
                    parent.as_fd(),
                    &name,
                    &operation.path,
                    required_content(operation)?.as_bytes(),
                    required_mode(operation)?,
                    &operation.prestate,
                    hooks,
                    journal,
                    original,
                    operation,
                )?;
                let actual = capture_named(parent.as_fd(), &name, &operation.path)?;
                let installed_match = actual
                    .identity
                    .is_some_and(|identity| same_binding(identity, installed));
                let post = if installed_match {
                    actual
                } else {
                    synthetic_file_post(operation, installed)
                };
                ensure_post(operation, &post)?;
                if !installed_match {
                    return Err(ApplyFailure::usage(format!(
                        "replaced path was rebound at {}",
                        operation.path
                    )));
                }
                hooks
                    .directory_fsync(parent.as_fd())
                    .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
            }
            OperationKind::RemoveFile => {
                let original = capture_named(parent.as_fd(), &name, &operation.path)?;
                let expected_original = original.clone();
                if original.prestate != operation.prestate {
                    return Err(ApplyFailure::usage(format!(
                        "approved repository state drifted at {}",
                        operation.path
                    )));
                }
                if !captured_matches(
                    capture_named(parent.as_fd(), &name, &operation.path)?,
                    &original,
                ) {
                    return Err(ApplyFailure::usage(format!(
                        "approved repository binding drifted at {}",
                        operation.path
                    )));
                }
                journal.push(JournalEntry::Removed {
                    path: operation.path.clone(),
                    original,
                    removed: false,
                });
                let record = journal.len() - 1;
                if let Err(error) = hooks.remove_file(parent.as_fd(), &name) {
                    journal.pop();
                    return Err(classify_failed_captured_mutation(
                        error,
                        parent.as_fd(),
                        &name,
                        &operation.path,
                        &expected_original,
                        "remove",
                    ));
                }
                let JournalEntry::Removed { removed, .. } = &mut journal[record] else {
                    return Err(ApplyFailure::operational(
                        "apply journal invariant failed: remove journal record index is stable",
                    ));
                };
                *removed = true;
                if capture_named(parent.as_fd(), &name, &operation.path)?
                    .prestate
                    .kind
                    != NodeKind::Missing
                {
                    return Err(ApplyFailure::usage(format!(
                        "removed path reappeared at {}",
                        operation.path
                    )));
                }
                hooks
                    .directory_fsync(parent.as_fd())
                    .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
            }
        }
        Ok(())
    })();
    let restored = restore_parent_mode(
        root,
        pins,
        parent.as_fd(),
        parent_path,
        parent_mode,
        hooks,
        journal,
    );
    combine_apply_results(outcome, restored, "restoring parent mode")
}

fn make_parent_writable(
    parent: BorrowedFd<'_>,
    path: &str,
    journal: &mut Vec<JournalEntry>,
) -> Result<Option<usize>, ApplyFailure> {
    let state = descriptor_identity(parent).map_err(ApplyFailure::operational)?;
    if state.mode & 0o300 == 0o300 {
        return Ok(None);
    }
    let fd =
        rustix::io::dup(parent).map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    journal.push(JournalEntry::ParentMode {
        path: if path.is_empty() {
            ".".to_owned()
        } else {
            path.to_owned()
        },
        fd,
        identity: state,
        original_mode: state.mode,
        active: true,
    });
    rfs::fchmod(parent, Mode::from_raw_mode((state.mode | 0o700) as _))
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    Ok(Some(journal.len() - 1))
}

#[allow(clippy::too_many_arguments)]
fn restore_parent_mode(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    parent: BorrowedFd<'_>,
    parent_path: &str,
    record: Option<usize>,
    hooks: &mut impl ApplyHooks,
    journal: &mut [JournalEntry],
) -> Result<(), ApplyFailure> {
    let Some(record) = record else {
        return Ok(());
    };
    let JournalEntry::ParentMode {
        identity,
        original_mode,
        active,
        ..
    } = &mut journal[record]
    else {
        return Err(ApplyFailure::operational(
            "apply journal invariant failed: parent mode record index is stable",
        ));
    };
    if !parent_binding_current(root, pins, parent, parent_path, *identity)? {
        return Err(ApplyFailure::usage(format!(
            "parent binding drifted while restoring mode: {}",
            if parent_path.is_empty() {
                "."
            } else {
                parent_path
            }
        )));
    }
    hooks
        .restore_parent_mode(parent, Mode::from_raw_mode(*original_mode as _))
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    hooks
        .parent_mode_fsync(parent, false)
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    *active = false;
    Ok(())
}

fn parent_binding_current(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    parent: BorrowedFd<'_>,
    path: &str,
    expected: Identity,
) -> Result<bool, ApplyFailure> {
    if !same_binding(
        descriptor_identity(parent).map_err(ApplyFailure::operational)?,
        expected,
    ) {
        return Ok(false);
    }
    if path.is_empty() {
        return root.verify_classified().map(|()| true);
    }
    let probe = format!("{path}/.binding-probe");
    match open_parent(root, pins, &probe) {
        Ok((named, _)) => descriptor_identity(named.as_fd())
            .map_err(ApplyFailure::operational)
            .map(|identity| same_binding(identity, expected)),
        Err(failure) => Err(failure),
    }
}

fn create_file(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    bytes: &[u8],
    mode: u32,
    hooks: &mut impl ApplyHooks,
) -> Result<Identity, CreateFileFailure> {
    let fd =
        rfs::openat(parent, name, CREATE_FLAGS, Mode::from_raw_mode(0o600)).map_err(|error| {
            CreateFileFailure {
                failure: exclusive_target_failure(
                    error,
                    "could not create recovery file".to_owned(),
                ),
                owned: None,
            }
        })?;
    let owned = descriptor_identity(fd.as_fd()).map_err(|message| CreateFileFailure {
        failure: ApplyFailure::operational(message),
        owned: None,
    })?;
    rfs::fchmod(fd.as_fd(), Mode::from_raw_mode(mode as _)).map_err(|error| CreateFileFailure {
        failure: ApplyFailure::operational(errno_text(error)),
        owned: Some(owned),
    })?;
    let mut file = File::from(fd);
    file.write_all(bytes).map_err(|error| CreateFileFailure {
        failure: ApplyFailure::operational(error.to_string()),
        owned: Some(owned),
    })?;
    hooks
        .file_fsync(file.as_fd())
        .map_err(|error| CreateFileFailure {
            failure: ApplyFailure::operational(errno_text(error)),
            owned: Some(owned),
        })?;
    Ok(owned)
}

#[allow(clippy::too_many_arguments)]
fn replace_file(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    path: &str,
    bytes: &[u8],
    mode: u32,
    expected: &NodePrestate,
    hooks: &mut impl ApplyHooks,
    journal: &mut Vec<JournalEntry>,
    original: Captured,
    operation: &InitOperation,
) -> Result<Identity, ApplyFailure> {
    let expected_original = original.clone();
    let (temporary, record, fd) = (0..128)
        .find_map(|_| {
            let temporary = match unique_temp(parent, name) {
                Ok(temporary) => temporary,
                Err(failure) => return Some(Err(failure)),
            };
            journal.push(JournalEntry::Replaced {
                path: path.to_owned(),
                original: Box::new(original.clone()),
                intended: expected_file_prestate(operation),
                post: None,
                temporary: temporary.clone(),
                temporary_fd: None,
                temporary_initial: None,
                installed: false,
            });
            match rfs::openat(parent, &temporary, CREATE_FLAGS, Mode::from_raw_mode(0o600)) {
                Ok(fd) => Some(Ok((temporary, journal.len() - 1, fd))),
                Err(Errno::EXIST) => {
                    journal.pop();
                    None
                }
                Err(error) => {
                    journal.pop();
                    Some(Err(ApplyFailure::operational(format!(
                        "could not create apply temporary for {path}: {error}"
                    ))))
                }
            }
        })
        .unwrap_or_else(|| {
            Err(ApplyFailure::operational(format!(
                "could not allocate an exclusive apply temporary for {path}"
            )))
        })?;
    let JournalEntry::Replaced { temporary_fd, .. } = &mut journal[record] else {
        return Err(ApplyFailure::operational(
            "apply journal invariant failed: replace journal record index is stable",
        ));
    };
    *temporary_fd = Some(fd);
    let JournalEntry::Replaced {
        temporary_fd: Some(fd),
        ..
    } = &journal[record]
    else {
        return Err(ApplyFailure::operational(
            "apply journal invariant failed: replace temporary descriptor was recorded",
        ));
    };
    let owned = descriptor_identity(fd.as_fd()).map_err(ApplyFailure::operational)?;
    let JournalEntry::Replaced {
        temporary_initial, ..
    } = &mut journal[record]
    else {
        return Err(ApplyFailure::operational(
            "apply journal invariant failed: replace journal record index is stable",
        ));
    };
    *temporary_initial = Some(owned);
    hooks
        .after_file_open_journaled(path)
        .map_err(ApplyFailure::operational)?;
    let JournalEntry::Replaced {
        temporary_fd: Some(fd),
        ..
    } = &journal[record]
    else {
        return Err(ApplyFailure::operational(
            "apply journal invariant failed: replace temporary descriptor was recorded",
        ));
    };
    rfs::fchmod(fd.as_fd(), Mode::from_raw_mode(mode as _))
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    let duplicate = rustix::io::dup(fd.as_fd())
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    File::from(duplicate)
        .write_all(bytes)
        .map_err(|error| ApplyFailure::operational(error.to_string()))?;
    hooks
        .file_fsync(fd.as_fd())
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    if !named_identity(parent, &temporary)
        .map_err(ApplyFailure::operational)?
        .is_some_and(|named| same_binding(named, owned))
    {
        return Err(ApplyFailure::usage(
            "apply temporary file was rebound".to_owned(),
        ));
    }
    let JournalEntry::Replaced { post, .. } = &mut journal[record] else {
        return Err(ApplyFailure::operational(
            "apply journal invariant failed: replace journal record index is stable",
        ));
    };
    *post = Some(Box::new(synthetic_file_post(operation, owned)));
    if !captured_matches(capture_named(parent, name, path)?, &expected_original)
        || expected_original.prestate != *expected
    {
        return Err(ApplyFailure::usage(format!(
            "approved repository state drifted at {path}"
        )));
    }
    hooks
        .before_replace_install(path)
        .map_err(ApplyFailure::operational)?;
    rfs::renameat(parent, &temporary, parent, name).map_err(|error| {
        classify_failed_captured_mutation(
            error,
            parent,
            name,
            path,
            &expected_original,
            "install replacement at",
        )
    })?;
    let JournalEntry::Replaced { installed, .. } = &mut journal[record] else {
        return Err(ApplyFailure::operational(
            "apply journal invariant failed: replace journal record index is stable",
        ));
    };
    *installed = true;
    hooks
        .after_rename_journaled(path)
        .map_err(ApplyFailure::operational)?;
    Ok(owned)
}

fn unique_temp(parent: BorrowedFd<'_>, target: &OsStr) -> Result<OsString, ApplyFailure> {
    for _ in 0..128 {
        let seq = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let name = OsString::from(format!(
            ".{}.yams-apply-{}-{seq}.tmp",
            target.to_string_lossy(),
            std::process::id()
        ));
        if named_identity(parent, &name)
            .map_err(ApplyFailure::operational)?
            .is_none()
        {
            return Ok(name);
        }
    }
    Err(ApplyFailure::operational(
        "could not allocate an exclusive apply temporary name".to_owned(),
    ))
}

fn unique_directory_temp(parent: BorrowedFd<'_>, target: &OsStr) -> Result<OsString, ApplyFailure> {
    for _ in 0..128 {
        let mut entropy = [0_u8; 16];
        getrandom::fill(&mut entropy).map_err(|error| {
            ApplyFailure::operational(format!("directory temporary entropy unavailable: {error}"))
        })?;
        let random = entropy
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        let name = OsString::from(format!(
            ".{}.yams-dir-{random}.tmp",
            target.to_string_lossy()
        ));
        if named_identity(parent, &name)
            .map_err(ApplyFailure::operational)?
            .is_none()
        {
            return Ok(name);
        }
    }
    Err(ApplyFailure::operational(
        "could not allocate a private directory temporary name".to_owned(),
    ))
}

fn ensure_post(operation: &InitOperation, post: &Captured) -> Result<(), ApplyFailure> {
    if post.prestate.kind != NodeKind::File
        || post.prestate.mode != operation.mode
        || post.prestate.sha256 != operation.post_sha256
    {
        return Err(ApplyFailure::usage(format!(
            "poststate verification failed at {}",
            operation.path
        )));
    }
    Ok(())
}

fn synthetic_file_post(operation: &InitOperation, identity: Identity) -> Captured {
    Captured {
        prestate: expected_file_prestate(operation),
        bytes: operation
            .content
            .as_ref()
            .map(|content| content.as_bytes().to_vec()),
        identity: Some(identity),
    }
}

fn expected_file_prestate(operation: &InitOperation) -> NodePrestate {
    NodePrestate {
        path: operation.path.clone(),
        kind: NodeKind::File,
        mode: operation.mode,
        sha256: operation.post_sha256.clone(),
        entries_sha256: None,
    }
}

fn classify_recovered_layout(
    pinned: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    expected_runtime: &Captured,
    hooks: &mut impl ApplyHooks,
) -> (LayoutClass, Vec<String>, Option<ApplyFailure>) {
    if let Err(failure) = pinned.verify_classified() {
        return (LayoutClass::Partial, vec![".".to_owned()], Some(failure));
    }
    let residues = match directory_temporary_residues(pinned, pins) {
        Ok(residues) => residues,
        Err(failure) => {
            return (
                LayoutClass::Partial,
                vec![failure.path],
                Some(failure.failure),
            );
        }
    };
    if !residues.is_empty() {
        return (LayoutClass::Partial, residues, None);
    }
    let first = classify_recovered_layout_once(pinned, pins, expected_runtime);
    if matches!(
        first.0,
        LayoutClass::Absent | LayoutClass::Minimal | LayoutClass::Full
    ) && first.1.is_empty()
    {
        let second = classify_recovered_layout_once(pinned, pins, expected_runtime);
        if second.0 == first.0 && second.1.is_empty() {
            return match directory_temporary_residues(pinned, pins) {
                Ok(residues) if residues.is_empty() => match hooks
                    .before_final_root_verification()
                    .map_err(ApplyFailure::operational)
                    .and_then(|()| pinned.verify_classified())
                {
                    Ok(()) => second,
                    Err(failure) => (LayoutClass::Partial, vec![".".to_owned()], Some(failure)),
                },
                Ok(residues) => (LayoutClass::Partial, residues, second.2),
                Err(failure) => (
                    LayoutClass::Partial,
                    vec![failure.path],
                    Some(failure.failure),
                ),
            };
        }
        return (LayoutClass::Partial, second.1, second.2);
    }
    first
}

fn classify_recovered_layout_once(
    pinned: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    expected_runtime: &Captured,
) -> (LayoutClass, Vec<String>, Option<ApplyFailure>) {
    let runtime_path = ".agents/memory/.write.lock";
    let mut drift = Vec::new();
    let mut failure = None;
    match capture_relative_pinned(pinned, pins, runtime_path) {
        Ok(actual) if actual == *expected_runtime => {}
        Ok(_) => drift.push(runtime_path.to_owned()),
        Err(capture_failure) => {
            merge_recovery_failure(&mut failure, capture_failure);
            drift.push(runtime_path.to_owned());
        }
    }

    let memory = recovery_capture(pinned, pins, ".agents/memory", &mut failure);
    let agents = recovery_capture(pinned, pins, "AGENTS.md", &mut failure);
    let policy_absent = agents.as_ref().is_some_and(|agents| {
        agents.prestate.kind == NodeKind::Missing
            || (agents.prestate.kind == NodeKind::File
                && agents
                    .bytes
                    .as_ref()
                    .and_then(|bytes| std::str::from_utf8(bytes).ok())
                    .is_some_and(|source| inspect_policy(source).heading_count == 0))
    });
    if drift.is_empty()
        && expected_runtime.prestate.kind == NodeKind::Missing
        && memory
            .as_ref()
            .is_some_and(|memory| memory.prestate.kind == NodeKind::Missing)
        && policy_absent
    {
        return (LayoutClass::Absent, drift, failure);
    }

    let flat_path = ".agents/memory/project-context.md";
    let flat = recovery_capture(pinned, pins, flat_path, &mut failure);
    let forbidden = [
        ".agents/memory/SCHEMA.md",
        ".agents/memory/INDEX.md",
        ".agents/memory/pages",
    ];
    let forbidden_states = forbidden.map(|path| recovery_capture(pinned, pins, path, &mut failure));
    if drift.is_empty()
        && forbidden_states.iter().all(|state| {
            state
                .as_ref()
                .is_some_and(|node| node.prestate.kind == NodeKind::Missing)
        })
        && let Some(ref flat) = flat
    {
        let paths = [
            "AGENTS.md",
            ".agents",
            ".agents/memory",
            ".agents/memory/.gitignore",
        ];
        let mut candidate = BTreeMap::new();
        let mut complete = insert_recovered_node(&mut candidate, flat_path, flat).is_ok();
        for path in paths {
            complete &= recovery_capture(pinned, pins, path, &mut failure)
                .is_some_and(|node| insert_recovered_node(&mut candidate, path, &node).is_ok());
        }
        if complete && validate_owned_candidate(&candidate, InitMode::Minimal).is_ok() {
            return (LayoutClass::Minimal, drift, failure);
        }
    }

    if drift.is_empty()
        && flat
            .as_ref()
            .is_some_and(|node| node.prestate.kind == NodeKind::Missing)
    {
        let paths = [
            "AGENTS.md",
            ".agents",
            ".agents/memory",
            ".agents/memory/.gitignore",
            ".agents/memory/SCHEMA.md",
            ".agents/memory/INDEX.md",
            ".agents/memory/pages",
        ];
        let mut candidate = BTreeMap::new();
        let mut complete = true;
        for path in paths {
            complete &= recovery_capture(pinned, pins, path, &mut failure)
                .is_some_and(|node| insert_recovered_node(&mut candidate, path, &node).is_ok());
        }
        let pages = match open_parent_pinned(pinned, pins, ".agents/memory/pages/.entry") {
            Ok((pages, _)) => Some(pages),
            Err(open_failure) => {
                merge_recovery_failure(&mut failure, open_failure);
                None
            }
        };
        let entries = pages.as_ref().and_then(|pages| {
            directory_signatures(pages.as_fd()).map_or_else(
                |read_failure| {
                    merge_recovery_failure(&mut failure, read_failure);
                    None
                },
                Some,
            )
        });
        if complete && let Some(entries) = entries {
            for (name, _) in entries {
                let Some(name) = name.to_str() else {
                    complete = false;
                    break;
                };
                if !name.ends_with(".md") || name.contains('/') {
                    complete = false;
                    break;
                }
                let path = format!(".agents/memory/pages/{name}");
                complete &=
                    recovery_capture(pinned, pins, &path, &mut failure).is_some_and(|node| {
                        insert_recovered_node(&mut candidate, &path, &node).is_ok()
                    });
            }
        } else {
            complete = false;
        }
        if complete && validate_owned_candidate(&candidate, InitMode::Full).is_ok() {
            return (LayoutClass::Full, drift, failure);
        }
    }

    for (path, state) in forbidden.into_iter().zip(&forbidden_states) {
        if state
            .as_ref()
            .is_some_and(|node| node.prestate.kind != NodeKind::Missing)
        {
            drift.push(path.to_owned());
        }
    }

    (LayoutClass::Partial, drift, failure)
}

fn recovery_capture(
    pinned: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    path: &str,
    failure: &mut Option<ApplyFailure>,
) -> Option<Captured> {
    match capture_relative_pinned(pinned, pins, path) {
        Ok(captured) => Some(captured),
        Err(capture_failure) => {
            merge_recovery_failure(failure, capture_failure);
            None
        }
    }
}

fn insert_recovered_node(
    candidate: &mut BTreeMap<String, DesiredNode>,
    path: &str,
    captured: &Captured,
) -> Result<(), String> {
    let node = match captured.prestate.kind {
        NodeKind::Directory => DesiredNode::Directory {
            mode: captured
                .prestate
                .mode
                .ok_or_else(|| format!("directory has no mode: {path}"))?,
        },
        NodeKind::File => DesiredNode::File {
            mode: captured
                .prestate
                .mode
                .ok_or_else(|| format!("file has no mode: {path}"))?,
            bytes: captured
                .bytes
                .clone()
                .ok_or_else(|| format!("file has no bytes: {path}"))?,
        },
        _ => return Err(format!("owned node is incomplete or unsafe: {path}")),
    };
    candidate.insert(path.to_owned(), node);
    Ok(())
}

fn finalize_and_validate(
    pinned: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    manifest: &InitManifest,
    approved: &BTreeMap<String, DesiredNode>,
    runtime_lock: &Captured,
    hooks: &mut impl ApplyHooks,
) -> Result<(), ApplyFailure> {
    pinned.verify_classified()?;
    if !directory_temporary_residues(pinned, pins)
        .map_err(|failure| failure.failure)?
        .is_empty()
    {
        return Err(ApplyFailure::usage(
            "a Yams directory temporary exists during final validation",
        ));
    }
    let mut actual = BTreeMap::new();
    for (path, expected) in approved {
        if !candidate_owned_path(path, manifest.mode) {
            return Err(ApplyFailure::usage(format!(
                "approved candidate contains an out-of-layout path: {path}"
            )));
        }
        let captured = capture_relative_pinned(pinned, pins, path)?;
        let node = match captured.prestate.kind {
            NodeKind::Directory => DesiredNode::Directory {
                mode: captured.prestate.mode.ok_or_else(|| {
                    ApplyFailure::usage(format!("final directory has no mode: {path}"))
                })?,
            },
            NodeKind::File => DesiredNode::File {
                mode: captured.prestate.mode.ok_or_else(|| {
                    ApplyFailure::usage(format!("final file has no mode: {path}"))
                })?,
                bytes: captured.bytes.ok_or_else(|| {
                    ApplyFailure::usage(format!("final file has no bytes: {path}"))
                })?,
            },
            _ => {
                return Err(ApplyFailure::usage(format!(
                    "unsafe final owned node at {path}"
                )));
            }
        };
        if &node != expected {
            return Err(ApplyFailure::usage(format!(
                "final owned node differs from the approved candidate at {path}"
            )));
        }
        actual.insert(path.clone(), node);
    }
    if manifest.mode == InitMode::Minimal {
        for path in [
            ".agents/memory/SCHEMA.md",
            ".agents/memory/INDEX.md",
            ".agents/memory/pages",
        ] {
            if capture_relative_pinned(pinned, pins, path)?.prestate.kind != NodeKind::Missing {
                return Err(ApplyFailure::usage(format!(
                    "structured layout fragment exists after minimal initialization: {path}"
                )));
            }
        }
    } else {
        let flat = capture_relative_pinned(pinned, pins, ".agents/memory/project-context.md")?;
        if flat.prestate.kind != NodeKind::Missing {
            return Err(ApplyFailure::usage(
                "the flat project page remains after full initialization",
            ));
        }
        let (pages, _) = open_parent_pinned(pinned, pins, ".agents/memory/pages/.entry")?;
        let names = directory_signatures(pages.as_fd())?
            .into_iter()
            .map(|(name, _)| name)
            .collect::<BTreeSet<_>>();
        let expected_names = approved
            .keys()
            .filter_map(|path| {
                path.strip_prefix(".agents/memory/pages/")
                    .map(OsString::from)
            })
            .collect::<BTreeSet<_>>();
        if names != expected_names {
            return Err(ApplyFailure::usage(
                "final pages directory membership differs from approval",
            ));
        }
    }
    let final_runtime = capture_relative_pinned(pinned, pins, ".agents/memory/.write.lock")?;
    if &final_runtime != runtime_lock {
        return Err(ApplyFailure::usage(
            "runtime lock changed during manifest apply",
        ));
    }
    validate_owned_candidate(&actual, manifest.mode)
        .map_err(|error| ApplyFailure::usage(error.to_string()))?;
    if owned_candidate_sha256(&actual) != manifest.candidate_sha256 {
        return Err(ApplyFailure::usage(
            "final owned candidate digest differs from the approved manifest",
        ));
    }
    hooks
        .during_final_validation()
        .map_err(ApplyFailure::operational)?;
    let mut revalidated = BTreeMap::new();
    for path in approved.keys() {
        let captured = capture_relative_pinned(pinned, pins, path)?;
        insert_recovered_node(&mut revalidated, path, &captured).map_err(ApplyFailure::usage)?;
    }
    if revalidated != actual {
        return Err(ApplyFailure::usage(
            "final owned bindings changed during validation",
        ));
    }
    match manifest.mode {
        InitMode::Minimal => {
            for path in [
                ".agents/memory/SCHEMA.md",
                ".agents/memory/INDEX.md",
                ".agents/memory/pages",
            ] {
                if capture_relative_pinned(pinned, pins, path)?.prestate.kind != NodeKind::Missing {
                    return Err(ApplyFailure::usage(format!(
                        "structured layout fragment appeared during minimal validation: {path}"
                    )));
                }
            }
        }
        InitMode::Full => {
            if capture_relative_pinned(pinned, pins, ".agents/memory/project-context.md")?
                .prestate
                .kind
                != NodeKind::Missing
            {
                return Err(ApplyFailure::usage(
                    "the flat project page appeared during full validation",
                ));
            }
            let (pages, _) = open_parent_pinned(pinned, pins, ".agents/memory/pages/.revalidate")?;
            let names = directory_signatures(pages.as_fd())?
                .into_iter()
                .map(|(name, _)| name)
                .collect::<BTreeSet<_>>();
            let expected_names = approved
                .keys()
                .filter_map(|path| {
                    path.strip_prefix(".agents/memory/pages/")
                        .map(OsString::from)
                })
                .collect::<BTreeSet<_>>();
            if names != expected_names {
                return Err(ApplyFailure::usage(
                    "final pages directory membership changed during validation",
                ));
            }
        }
    }
    if capture_relative_pinned(pinned, pins, ".agents/memory/.write.lock")? != *runtime_lock {
        return Err(ApplyFailure::usage(
            "runtime lock changed during final revalidation",
        ));
    }
    if !directory_temporary_residues(pinned, pins)
        .map_err(|failure| failure.failure)?
        .is_empty()
    {
        return Err(ApplyFailure::usage(
            "a Yams directory temporary appeared during validation",
        ));
    }
    pinned.verify_classified()?;
    Ok(())
}

fn recover(
    root: &PinnedRoot,
    pins: &mut BTreeMap<String, Identity>,
    journal: &mut [JournalEntry],
    hooks: &mut impl ApplyHooks,
    result: &mut ApplyResult,
) -> Option<ApplyFailure> {
    let mut failure = None;
    for entry in journal.iter_mut().rev() {
        let outcome = match entry {
            JournalEntry::CreatedFile {
                path,
                fd,
                initial,
                expected,
                post,
            } => recover_created_file(
                root,
                pins,
                path,
                fd.as_ref(),
                *initial,
                expected,
                post.as_ref(),
                hooks,
            ),
            JournalEntry::CreatedDirectory {
                path,
                temporary,
                identity,
                fd,
                location,
            } => match location {
                DirectoryLocation::UnknownTemporary => Err(ApplyFailure::usage(
                    "directory temporary ownership was not established",
                )),
                DirectoryLocation::Temporary | DirectoryLocation::Installed => {
                    recover_created_directory(
                        root,
                        pins,
                        path,
                        temporary,
                        *identity,
                        fd.as_ref(),
                        *location,
                        hooks,
                    )
                }
            },
            JournalEntry::Replaced {
                path,
                original,
                intended,
                post,
                temporary,
                temporary_fd,
                temporary_initial,
                installed,
            } => recover_replaced(
                root,
                pins,
                path,
                original,
                intended,
                post.as_deref(),
                temporary,
                temporary_fd.as_ref(),
                *temporary_initial,
                *installed,
                hooks,
            ),
            JournalEntry::Removed {
                path,
                original,
                removed,
            } => {
                if !*removed {
                    continue;
                }
                recover_removed(root, pins, path, original, hooks)
            }
            JournalEntry::ParentMode {
                path,
                fd,
                identity,
                original_mode,
                active,
            } => {
                if !*active {
                    continue;
                }
                let named_current = if path == "." {
                    root.verify_classified()
                } else {
                    let probe = format!("{path}/.recovery-probe");
                    open_parent(root, pins, &probe).map(|_| ())
                };
                named_current.and_then(|()| {
                    let descriptor =
                        descriptor_identity(fd.as_fd()).map_err(ApplyFailure::operational)?;
                    if !same_binding(descriptor, *identity) {
                        return Err(ApplyFailure::usage(
                            "parent binding drifted during recovery",
                        ));
                    }
                    hooks
                        .restore_parent_mode(fd.as_fd(), Mode::from_raw_mode(*original_mode as _))
                        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
                    hooks
                        .parent_mode_fsync(fd.as_fd(), true)
                        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
                    *active = false;
                    Ok(())
                })
            }
        };
        match outcome {
            Ok(()) => result.restored.push(entry_report_path(entry)),
            Err(entry_failure) => {
                result.unresolved.push(entry_report_path(entry));
                merge_recovery_failure(&mut failure, entry_failure);
            }
        }
    }
    failure
}

fn merge_recovery_failure(current: &mut Option<ApplyFailure>, candidate: ApplyFailure) {
    *current = Some(match current.take() {
        None => candidate,
        Some(primary) => {
            combine_apply_results(Err(primary), Err(candidate), "additional recovery failure")
                .expect_err("combining two failures cannot succeed")
        }
    });
}

fn combine_apply_results(
    primary: Result<(), ApplyFailure>,
    cleanup: Result<(), ApplyFailure>,
    cleanup_context: &str,
) -> Result<(), ApplyFailure> {
    match (primary, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(failure), Ok(())) => Err(failure),
        (Ok(()), Err(mut failure)) => {
            failure.message = format!("{cleanup_context}: {}", failure.message);
            Err(failure)
        }
        (Err(primary), Err(cleanup)) => Err(ApplyFailure {
            class: dominant_exit_class(primary.class, cleanup.class),
            message: format!(
                "{}; {cleanup_context}: {}",
                primary.message, cleanup.message
            ),
        }),
    }
}

fn dominant_exit_class(left: ApplyExitClass, right: ApplyExitClass) -> ApplyExitClass {
    if left == ApplyExitClass::Operational || right == ApplyExitClass::Operational {
        ApplyExitClass::Operational
    } else if left == ApplyExitClass::Usage || right == ApplyExitClass::Usage {
        ApplyExitClass::Usage
    } else {
        ApplyExitClass::Success
    }
}

fn entry_report_path(entry: &JournalEntry) -> String {
    match entry {
        JournalEntry::CreatedDirectory {
            path,
            temporary,
            location: DirectoryLocation::UnknownTemporary | DirectoryLocation::Temporary,
            ..
        } => directory_temporary_relative_path(path, temporary),
        JournalEntry::CreatedFile { path, .. }
        | JournalEntry::CreatedDirectory { path, .. }
        | JournalEntry::Replaced { path, .. }
        | JournalEntry::Removed { path, .. }
        | JournalEntry::ParentMode { path, .. } => path.clone(),
    }
}

fn directory_temporary_relative_path(path: &str, temporary: &OsStr) -> String {
    let temporary = temporary.to_string_lossy();
    match path.rsplit_once('/') {
        None => temporary.into_owned(),
        Some((parent, _)) => format!("{parent}/{temporary}"),
    }
}

#[allow(clippy::too_many_arguments)]
fn recover_created_file(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    path: &str,
    fd: Option<&OwnedFd>,
    initial: Option<Identity>,
    expected: &NodePrestate,
    post: Option<&Captured>,
    hooks: &mut impl ApplyHooks,
) -> Result<(), ApplyFailure> {
    let (parent, name) = open_parent_pinned(root, pins, path)?;
    let mode = recovery_parent_writable(parent.as_fd())?;
    let outcome = (|| {
        let owned = descriptor_identity(
            fd.ok_or_else(|| ApplyFailure::usage("created file ownership was not recorded"))?
                .as_fd(),
        )
        .map_err(ApplyFailure::operational)?;
        if !named_identity(parent.as_fd(), &name)
            .map_err(ApplyFailure::operational)?
            .is_some_and(|named| same_binding(named, owned))
        {
            return Err(ApplyFailure::usage(
                "created file binding drifted during recovery",
            ));
        }
        let actual = capture_named(parent.as_fd(), &name, path)?;
        if !actual
            .identity
            .is_some_and(|identity| same_binding(identity, owned))
        {
            return Err(ApplyFailure::usage(
                "created file was replaced during recovery",
            ));
        }
        let is_initial = initial.is_some_and(|initial| {
            actual.identity == Some(initial)
                && actual.bytes.as_deref() == Some(&[])
                && actual.prestate.mode == Some(initial.mode)
        });
        let is_expected = actual.prestate == *expected;
        let is_recorded_post = post.is_some_and(|post| captured_matches(actual.clone(), post));
        if !is_initial && !is_expected && !is_recorded_post {
            return Err(ApplyFailure::usage(
                "created file contents drifted during recovery",
            ));
        }
        if !named_identity(parent.as_fd(), &name)
            .map_err(ApplyFailure::operational)?
            .is_some_and(|named| same_binding(named, owned))
        {
            return Err(ApplyFailure::usage(
                "created file binding drifted before recovery removal",
            ));
        }
        rfs::unlinkat(parent.as_fd(), &name, AtFlags::empty())
            .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
        hooks
            .directory_fsync(parent.as_fd())
            .map_err(|error| ApplyFailure::operational(errno_text(error)))
    })();
    let restored = recovery_restore_parent(parent.as_fd(), mode, hooks);
    combine_apply_results(outcome, restored, "restoring parent mode")
}

#[allow(clippy::too_many_arguments)]
fn recover_created_directory(
    root: &PinnedRoot,
    pins: &mut BTreeMap<String, Identity>,
    path: &str,
    temporary: &OsStr,
    identity: Option<Identity>,
    fd: Option<&OwnedFd>,
    location: DirectoryLocation,
    hooks: &mut impl ApplyHooks,
) -> Result<(), ApplyFailure> {
    let (parent, final_name) = open_parent_pinned(root, pins, path)?;
    let name = match location {
        DirectoryLocation::Temporary => temporary,
        DirectoryLocation::Installed => final_name.as_os_str(),
        DirectoryLocation::UnknownTemporary => {
            return Err(ApplyFailure::usage(
                "directory temporary location was not recorded",
            ));
        }
    };
    let mode = recovery_parent_writable(parent.as_fd())?;
    let outcome = (|| {
        let identity =
            identity.ok_or_else(|| ApplyFailure::usage("directory ownership was not recorded"))?;
        if !same_binding(
            descriptor_identity(
                fd.ok_or_else(|| ApplyFailure::usage("directory descriptor was not recorded"))?
                    .as_fd(),
            )
            .map_err(ApplyFailure::operational)?,
            identity,
        ) {
            return Err(ApplyFailure::usage(
                "created directory descriptor drifted during recovery",
            ));
        }
        let actual = capture_named(parent.as_fd(), name, path)?;
        if actual.prestate.kind != NodeKind::Directory
            || !actual
                .identity
                .is_some_and(|actual| same_binding(actual, identity))
            || !directory_signatures(
                rfs::openat(parent.as_fd(), name, DIRECTORY_FLAGS, Mode::empty())
                    .map_err(|error| {
                        named_access_failure(
                            error,
                            "could not open created directory during recovery".to_owned(),
                        )
                    })?
                    .as_fd(),
            )?
            .is_empty()
        {
            return Err(ApplyFailure::usage(
                "created directory drifted during recovery",
            ));
        }
        if !named_identity(parent.as_fd(), name)
            .map_err(ApplyFailure::operational)?
            .is_some_and(|named| same_binding(named, identity))
        {
            return Err(ApplyFailure::usage(
                "created directory binding drifted before recovery removal",
            ));
        }
        rfs::unlinkat(parent.as_fd(), name, AtFlags::REMOVEDIR)
            .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
        hooks
            .directory_fsync(parent.as_fd())
            .map_err(|error| ApplyFailure::operational(errno_text(error)))
    })();
    let restored = recovery_restore_parent(parent.as_fd(), mode, hooks);
    if outcome.is_ok() && location == DirectoryLocation::Installed {
        pins.remove(path);
    }
    combine_apply_results(outcome, restored, "restoring parent mode")
}

#[allow(clippy::too_many_arguments)]
fn recover_replaced(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    path: &str,
    original: &Captured,
    intended: &NodePrestate,
    post: Option<&Captured>,
    temporary: &OsStr,
    temporary_fd: Option<&OwnedFd>,
    temporary_initial: Option<Identity>,
    installed: bool,
    hooks: &mut impl ApplyHooks,
) -> Result<(), ApplyFailure> {
    let (parent, name) = open_parent_pinned(root, pins, path)?;
    let mode = recovery_parent_writable(parent.as_fd())?;
    let outcome =
        (|| {
            let mut removed_temporary = false;
            if let Some(temp_state) =
                named_identity(parent.as_fd(), temporary).map_err(ApplyFailure::operational)?
            {
                let owned = descriptor_identity(
                    temporary_fd
                        .ok_or_else(|| ApplyFailure::usage("replace descriptor was not recorded"))?
                        .as_fd(),
                )
                .map_err(ApplyFailure::operational)?;
                if !same_binding(temp_state, owned) {
                    return Err(ApplyFailure::usage(
                        "replace temporary binding drifted during recovery",
                    ));
                }
                let actual = capture_named(parent.as_fd(), temporary, path)?;
                if !actual
                    .identity
                    .is_some_and(|identity| same_binding(identity, owned))
                {
                    return Err(ApplyFailure::usage(
                        "replace temporary was rebound during recovery",
                    ));
                }
                let initial = temporary_initial.is_some_and(|initial| {
                    actual.identity == Some(initial)
                        && actual.bytes.as_deref() == Some(&[])
                        && actual.prestate.mode == Some(initial.mode)
                });
                if !initial && actual.prestate != *intended {
                    return Err(ApplyFailure::usage(
                        "replace temporary contents drifted during recovery",
                    ));
                }
                if !named_identity(parent.as_fd(), temporary)
                    .map_err(ApplyFailure::operational)?
                    .is_some_and(|named| same_binding(named, owned))
                {
                    return Err(ApplyFailure::usage(
                        "replace temporary binding drifted before cleanup",
                    ));
                }
                rfs::unlinkat(parent.as_fd(), temporary, AtFlags::empty())
                    .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
                removed_temporary = true;
            }
            let current = capture_named(parent.as_fd(), &name, path)?;
            if captured_matches(current.clone(), original) {
                if removed_temporary {
                    hooks
                        .replaced_temp_cleanup_fsync(parent.as_fd())
                        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
                }
                return Ok(());
            }
            let installed_post = post
                .filter(|post| installed && captured_matches(current, post))
                .ok_or_else(|| ApplyFailure::usage("replacement target drifted during recovery"))?;
            let bytes = original.bytes.as_ref().ok_or_else(|| {
                ApplyFailure::usage("original replacement bytes were not recorded")
            })?;
            restore_file_atomic(
                parent.as_fd(),
                &name,
                path,
                bytes,
                original.prestate.mode.ok_or_else(|| {
                    ApplyFailure::usage("original replacement mode was not recorded")
                })?,
                installed_post,
                hooks,
            )?;
            hooks
                .directory_fsync(parent.as_fd())
                .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
            let restored = capture_named(parent.as_fd(), &name, path)?;
            if restored.prestate != original.prestate || restored.bytes != original.bytes {
                return Err(ApplyFailure::usage(
                    "restored replacement did not match its original state",
                ));
            }
            Ok(())
        })();
    let restored_mode = recovery_restore_parent(parent.as_fd(), mode, hooks);
    combine_apply_results(outcome, restored_mode, "restoring parent mode")
}

fn captured_matches(actual: Captured, expected: &Captured) -> bool {
    actual.prestate == expected.prestate
        && actual.bytes == expected.bytes
        && match (actual.identity, expected.identity) {
            (None, None) => true,
            (Some(actual), Some(expected)) => same_binding(actual, expected),
            _ => false,
        }
}

fn recover_removed(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
    path: &str,
    original: &Captured,
    hooks: &mut impl ApplyHooks,
) -> Result<(), ApplyFailure> {
    let (parent, name) = open_parent_pinned(root, pins, path)?;
    let mode = recovery_parent_writable(parent.as_fd())?;
    let outcome = (|| {
        if capture_named(parent.as_fd(), &name, path)?.prestate.kind != NodeKind::Missing {
            return Err(ApplyFailure::usage(
                "removed target reappeared during recovery",
            ));
        }
        create_file(
            parent.as_fd(),
            &name,
            original
                .bytes
                .as_ref()
                .ok_or_else(|| ApplyFailure::usage("removed file bytes were not recorded"))?,
            original
                .prestate
                .mode
                .ok_or_else(|| ApplyFailure::usage("removed file mode was not recorded"))?,
            hooks,
        )
        .map_err(|failure| failure.failure)?;
        hooks
            .directory_fsync(parent.as_fd())
            .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
        let restored = capture_named(parent.as_fd(), &name, path)?;
        if restored.prestate != original.prestate || restored.bytes != original.bytes {
            return Err(ApplyFailure::usage(
                "restored removed file did not match its original state",
            ));
        }
        Ok(())
    })();
    let restored_mode = recovery_restore_parent(parent.as_fd(), mode, hooks);
    combine_apply_results(outcome, restored_mode, "restoring parent mode")
}

fn restore_file_atomic(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    path: &str,
    bytes: &[u8],
    mode: u32,
    expected: &Captured,
    hooks: &mut impl ApplyHooks,
) -> Result<(), ApplyFailure> {
    let temporary = unique_temp(parent, name)?;
    let mut owned = None;
    let result = (|| {
        let created = match create_file(parent, &temporary, bytes, mode, hooks) {
            Ok(identity) => identity,
            Err(failure) => {
                owned = failure.owned;
                return Err(failure.failure);
            }
        };
        owned = Some(created);
        if !captured_matches(capture_named(parent, name, path)?, expected) {
            return Err(ApplyFailure::usage(format!(
                "recovery target drifted at {path}"
            )));
        }
        rfs::renameat(parent, &temporary, parent, name)
            .map_err(|error| ApplyFailure::operational(errno_text(error)))
    })();
    let cleanup = if result.is_err() {
        cleanup_restore_temporary(parent, &temporary, owned, hooks)
    } else {
        Ok(())
    };
    combine_apply_results(result, cleanup, "temporary cleanup")
}

fn cleanup_restore_temporary(
    parent: BorrowedFd<'_>,
    temporary: &OsStr,
    owned: Option<Identity>,
    hooks: &mut impl ApplyHooks,
) -> Result<(), ApplyFailure> {
    let Some(owned) = owned else {
        return Ok(());
    };
    let named = named_identity(parent, temporary).map_err(ApplyFailure::operational)?;
    let Some(named) = named else {
        return Err(ApplyFailure::usage(
            "restore temporary disappeared before cleanup",
        ));
    };
    if !same_binding(named, owned) {
        return Err(ApplyFailure::usage(
            "restore temporary binding drifted before cleanup",
        ));
    }
    hooks
        .remove_restore_temporary(parent, temporary)
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    hooks
        .directory_fsync(parent)
        .map_err(|error| ApplyFailure::operational(errno_text(error)))
}

fn recovery_parent_writable(parent: BorrowedFd<'_>) -> Result<Option<u32>, ApplyFailure> {
    let state = descriptor_identity(parent).map_err(ApplyFailure::operational)?;
    if state.mode & 0o300 == 0o300 {
        return Ok(None);
    }
    rfs::fchmod(parent, Mode::from_raw_mode((state.mode | 0o700) as _))
        .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    Ok(Some(state.mode))
}

fn recovery_restore_parent(
    parent: BorrowedFd<'_>,
    original: Option<u32>,
    hooks: &mut impl ApplyHooks,
) -> Result<(), ApplyFailure> {
    if let Some(mode) = original {
        hooks
            .restore_parent_mode(parent, Mode::from_raw_mode(mode as _))
            .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
        hooks
            .directory_fsync(parent)
            .map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    }
    Ok(())
}

fn directory_signatures(fd: BorrowedFd<'_>) -> Result<Vec<(OsString, Identity)>, ApplyFailure> {
    let mut dir =
        Dir::read_from(fd).map_err(|error| ApplyFailure::operational(errno_text(error)))?;
    let mut names = Vec::new();
    for entry in &mut dir {
        let entry = entry.map_err(|error| ApplyFailure::operational(errno_text(error)))?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(OsString::from_vec(bytes.to_vec()));
        }
    }
    names.sort_by(|a, b| a.as_bytes().cmp(b.as_bytes()));
    names
        .into_iter()
        .map(|name| {
            named_identity(fd, &name)
                .map_err(ApplyFailure::operational)?
                .map(|state| (name, state))
                .ok_or_else(|| ApplyFailure::usage("directory entry disappeared"))
        })
        .collect()
}

fn directory_temporary_residues(
    root: &PinnedRoot,
    pins: &BTreeMap<String, Identity>,
) -> Result<Vec<String>, ResidueFailure> {
    let mut residues = Vec::new();
    for (parent_path, directory_targets, file_targets) in [
        ("", &[".agents"][..], &["AGENTS.md"][..]),
        (".agents", &["memory"][..], &[][..]),
        (
            ".agents/memory",
            &["pages"][..],
            &["project-context.md", "SCHEMA.md", "INDEX.md"][..],
        ),
        (".agents/memory/pages", &[][..], &["project-context.md"][..]),
    ] {
        let directory = if parent_path.is_empty() {
            rustix::io::dup(root.fd.as_fd()).map_err(|error| ResidueFailure {
                failure: ApplyFailure::operational(errno_text(error)),
                path: ".".to_owned(),
            })
        } else {
            let state = capture_relative_pinned(root, pins, parent_path).map_err(|failure| {
                ResidueFailure {
                    failure,
                    path: parent_path.to_owned(),
                }
            })?;
            if state.prestate.kind == NodeKind::Missing {
                continue;
            }
            if state.prestate.kind != NodeKind::Directory {
                return Err(ResidueFailure {
                    failure: ApplyFailure::usage(format!(
                        "temporary namespace parent is not a directory: {parent_path}"
                    )),
                    path: parent_path.to_owned(),
                });
            }
            open_parent_pinned(root, pins, &format!("{parent_path}/.temporary-scan"))
                .map(|(directory, _)| directory)
                .map_err(|failure| ResidueFailure {
                    failure,
                    path: parent_path.to_owned(),
                })
        };
        let directory = directory?;
        let path = if parent_path.is_empty() {
            "."
        } else {
            parent_path
        };
        let entries =
            directory_signatures(directory.as_fd()).map_err(|failure| ResidueFailure {
                failure,
                path: path.to_owned(),
            })?;
        for (name, _) in entries {
            if exact_directory_temporary(&name, directory_targets)
                || exact_file_temporary(&name, file_targets)
            {
                let name = name.to_string_lossy();
                residues.push(if parent_path.is_empty() {
                    name.into_owned()
                } else {
                    format!("{parent_path}/{name}")
                });
            }
        }
    }
    residues.sort();
    residues.dedup();
    Ok(residues)
}

fn exact_directory_temporary(name: &OsStr, targets: &[&str]) -> bool {
    let bytes = name.as_bytes();
    targets.iter().any(|target| {
        let prefix = format!(".{target}.yams-dir-");
        bytes
            .strip_prefix(prefix.as_bytes())
            .and_then(|tail| tail.strip_suffix(b".tmp"))
            .is_some_and(|entropy| {
                entropy.len() == 32
                    && entropy
                        .iter()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(byte))
            })
    })
}

fn exact_file_temporary(name: &OsStr, targets: &[&str]) -> bool {
    let bytes = name.as_bytes();
    targets.iter().any(|target| {
        let prefix = format!(".{target}.yams-apply-");
        let Some(body) = bytes
            .strip_prefix(prefix.as_bytes())
            .and_then(|tail| tail.strip_suffix(b".tmp"))
        else {
            return false;
        };
        let Some(separator) = body.iter().position(|byte| *byte == b'-') else {
            return false;
        };
        let (pid, sequence_with_separator) = body.split_at(separator);
        let sequence = &sequence_with_separator[1..];
        !pid.is_empty()
            && !sequence.is_empty()
            && pid.iter().all(u8::is_ascii_digit)
            && sequence.iter().all(u8::is_ascii_digit)
    })
}

fn named_identity(parent: BorrowedFd<'_>, name: &OsStr) -> Result<Option<Identity>, String> {
    match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(state) => Ok(Some(Identity::from_stat(&state))),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(error.to_string()),
    }
}
fn descriptor_identity(fd: BorrowedFd<'_>) -> Result<Identity, String> {
    rfs::fstat(fd)
        .map(|s| Identity::from_stat(&s))
        .map_err(errno_text)
}
fn same_binding(left: Identity, right: Identity) -> bool {
    left.device == right.device && left.inode == right.inode && left.kind == right.kind
}
fn errno_text(error: Errno) -> String {
    error.to_string()
}
fn parent_access_failure(error: Errno, context: String) -> ApplyFailure {
    let message = format!("{context}: {error}");
    if matches!(error, Errno::NOENT | Errno::NOTDIR | Errno::LOOP) {
        ApplyFailure::usage(message)
    } else {
        ApplyFailure::operational(message)
    }
}
fn named_access_failure(error: Errno, context: String) -> ApplyFailure {
    parent_access_failure(error, context)
}
fn exclusive_target_failure(error: Errno, context: String) -> ApplyFailure {
    let message = format!("{context}: {error}");
    if matches!(
        error,
        Errno::EXIST | Errno::NOTEMPTY | Errno::NOENT | Errno::NOTDIR | Errno::LOOP
    ) {
        ApplyFailure::usage(message)
    } else {
        ApplyFailure::operational(message)
    }
}
fn classify_failed_captured_mutation(
    error: Errno,
    parent: BorrowedFd<'_>,
    name: &OsStr,
    path: &str,
    expected: &Captured,
    operation: &str,
) -> ApplyFailure {
    match capture_named(parent, name, path) {
        Ok(actual) if !captured_matches(actual.clone(), expected) => ApplyFailure::usage(format!(
            "target binding drifted before {operation} at {path}"
        )),
        Ok(_) => ApplyFailure::operational(format!("could not {operation} {path}: {error}")),
        Err(failure) => failure,
    }
}
fn timestamp_ns(seconds: i64, nanos: i64) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanos)
}
fn kind_tag(kind: FileType) -> u8 {
    if kind.is_file() {
        b'f'
    } else if kind.is_dir() {
        b'd'
    } else if kind.is_symlink() {
        b'l'
    } else {
        b'o'
    }
}

fn operation_rank(kind: OperationKind) -> u8 {
    match kind {
        OperationKind::CreateDirectory => 0,
        OperationKind::CreateFile | OperationKind::ReplaceFile => 1,
        OperationKind::RemoveFile => 2,
    }
}
fn sort_operations(operations: &mut [InitOperation]) {
    operations.sort_by(|a, b| {
        operation_rank(a.kind)
            .cmp(&operation_rank(b.kind))
            .then_with(|| {
                if a.kind == OperationKind::RemoveFile {
                    b.path
                        .split('/')
                        .count()
                        .cmp(&a.path.split('/').count())
                        .then_with(|| a.path.cmp(&b.path))
                } else {
                    a.path
                        .split('/')
                        .count()
                        .cmp(&b.path.split('/').count())
                        .then_with(|| a.path.cmp(&b.path))
                }
            })
    });
}
fn proposal_line(operation: &InitOperation) -> String {
    format!(
        "{} {}",
        match operation.kind {
            OperationKind::CreateDirectory => "CREATE DIR",
            OperationKind::CreateFile => "CREATE FILE",
            OperationKind::ReplaceFile => "REPLACE FILE",
            OperationKind::RemoveFile => "REMOVE FILE",
        },
        operation.path
    )
}
fn failed_result(manifest_sha256: String, final_layout: LayoutClass, error: String) -> ApplyResult {
    ApplyResult {
        ok: false,
        manifest_sha256,
        created: Vec::new(),
        changed: Vec::new(),
        removed: Vec::new(),
        restored: Vec::new(),
        unresolved: Vec::new(),
        final_layout,
        validated: false,
        error: Some(error),
        next: Vec::new(),
    }
}

fn failure_outcome(mut result: ApplyResult, failure: ApplyFailure) -> ApplyOutcome {
    result.error = Some(failure.message);
    ApplyOutcome {
        result,
        class: failure.class,
    }
}

fn normalize_result(result: &mut ApplyResult) {
    for list in [
        &mut result.created,
        &mut result.changed,
        &mut result.removed,
        &mut result.restored,
        &mut result.unresolved,
    ] {
        list.sort();
        list.dedup();
    }
}
fn account_journal(journal: &[JournalEntry], result: &mut ApplyResult) {
    for entry in journal {
        match entry {
            JournalEntry::CreatedFile {
                path, fd: Some(_), ..
            } => result.created.push(path.clone()),
            JournalEntry::CreatedFile { fd: None, .. } => {}
            JournalEntry::CreatedDirectory {
                path,
                location: DirectoryLocation::Installed,
                ..
            } => result.created.push(path.clone()),
            JournalEntry::CreatedDirectory { .. } => {}
            JournalEntry::Replaced {
                path,
                installed: true,
                ..
            } => result.changed.push(path.clone()),
            JournalEntry::Replaced {
                installed: false, ..
            } => {}
            JournalEntry::Removed {
                path,
                removed: true,
                ..
            } => result.removed.push(path.clone()),
            JournalEntry::Removed { removed: false, .. } => {}
            JournalEntry::ParentMode { .. } => {}
        }
    }
}
