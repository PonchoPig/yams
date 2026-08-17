use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::Read;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use rustix::process::getuid;

use super::{
    InitConflict, InitError, InitInspection, InitMode, LayoutClass, MEMORY_GITIGNORE, NodeKind,
    NodePrestate, SCHEMA, inspect_policy, sha256,
};
use crate::check::compat_snapshot;
use crate::{CapturedPage, CapturedPageOutcome, WikiSnapshot, parse_wiki_page, validate_wiki};

const RUNTIME_LOCK_PATH: &str = ".agents/memory/.write.lock";
const GITDIR_POINTER_MAX_BYTES: usize = 4096;
const BASE_PATHS: [&str; 9] = [
    "AGENTS.md",
    ".agents",
    ".agents/memory",
    ".agents/memory/project-context.md",
    ".agents/memory/.gitignore",
    ".agents/memory/SCHEMA.md",
    ".agents/memory/INDEX.md",
    ".agents/memory/pages",
    RUNTIME_LOCK_PATH,
];
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct NodeState {
    device: u64,
    inode: u64,
    mode: u32,
    kind: FileType,
    nlink: u64,
    uid: u32,
    size: u64,
    modified_ns: i128,
    changed_ns: i128,
}

impl NodeState {
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
            mode: stat.st_mode as u32,
            kind: FileType::from_raw_mode(stat.st_mode),
            nlink: stat.st_nlink as u64,
            uid: stat.st_uid as u32,
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

trait InventoryHooks {
    fn after_directory_opened(&mut self, _path: &str) {}
    fn after_directory_enumerated(&mut self, _path: &str) {}
    fn after_file_read(&mut self, _path: &str) {}
    fn after_runtime_lock_captured(&mut self) {}
    fn after_first_git_status(&mut self) {}
}

struct SystemInventoryHooks;

impl InventoryHooks for SystemInventoryHooks {}

struct Inventory {
    prestates: Vec<NodePrestate>,
    contents: BTreeMap<String, Vec<u8>>,
    conflicts: Vec<InitConflict>,
    repository_binding: Vec<u8>,
    consistent: bool,
}

pub(crate) struct RepositorySnapshot {
    pub inspection: InitInspection,
    pub contents: BTreeMap<String, Vec<u8>>,
}

pub fn inspect_repository(root: &Path) -> Result<InitInspection, InitError> {
    let mut hooks = SystemInventoryHooks;
    inspect_repository_with_hooks(root, &mut hooks)
}

pub(crate) fn capture_repository(root: &Path) -> Result<RepositorySnapshot, InitError> {
    let mut hooks = SystemInventoryHooks;
    capture_repository_with_hooks(root, &mut hooks)
}

fn recommended_mode(attainable: &[InitMode]) -> Option<InitMode> {
    if attainable.contains(&InitMode::Full) {
        Some(InitMode::Full)
    } else {
        attainable.first().copied()
    }
}

fn inspect_repository_with_hooks(
    root: &Path,
    hooks: &mut impl InventoryHooks,
) -> Result<InitInspection, InitError> {
    Ok(capture_repository_with_hooks(root, hooks)?.inspection)
}

fn capture_repository_with_hooks(
    root: &Path,
    hooks: &mut impl InventoryHooks,
) -> Result<RepositorySnapshot, InitError> {
    let root = canonical_repository_root(root)?;
    let root_text = root.to_str().ok_or_else(|| {
        InitError::InvalidRoot("canonical repository path is not UTF-8".to_owned())
    })?;
    let (mut inventory, dirty_paths) = inventory_repository(&root, root_text, hooks)?;

    let agents_kind = kind_at(&inventory.prestates, "AGENTS.md");
    let memory_kind = kind_at(&inventory.prestates, ".agents/memory");
    let flat_kind = kind_at(&inventory.prestates, ".agents/memory/project-context.md");
    let schema_kind = kind_at(&inventory.prestates, ".agents/memory/SCHEMA.md");
    let index_kind = kind_at(&inventory.prestates, ".agents/memory/INDEX.md");
    let pages_kind = kind_at(&inventory.prestates, ".agents/memory/pages");

    validate_expected_kinds(&inventory.prestates, &mut inventory.conflicts);

    let agents_source = inventory
        .contents
        .get("AGENTS.md")
        .map(|bytes| std::str::from_utf8(bytes));
    if agents_source.as_ref().is_some_and(Result::is_err) {
        push_conflict(
            &mut inventory.conflicts,
            "AGENTS.md",
            "invalid-agents-utf8",
            "AGENTS.md is not valid UTF-8.",
        );
    }
    let policy = agents_source
        .and_then(Result::ok)
        .map(inspect_policy)
        .unwrap_or(super::PolicyInspection {
            heading_count: 0,
            exact: false,
        });
    match policy.heading_count {
        0 => {}
        1 if policy.exact => {}
        1 => push_conflict(
            &mut inventory.conflicts,
            "AGENTS.md",
            "noncanonical-policy",
            "The Project memory section differs from the canonical policy.",
        ),
        _ => push_conflict(
            &mut inventory.conflicts,
            "AGENTS.md",
            "duplicate-policy-heading",
            "AGENTS.md contains more than one Project memory heading.",
        ),
    }

    let flat_valid = validate_flat_page(
        flat_kind,
        inventory.contents.get(".agents/memory/project-context.md"),
        &mut inventory.conflicts,
    );
    let schema_exact = validate_schema(
        schema_kind,
        inventory.contents.get(".agents/memory/SCHEMA.md"),
        &mut inventory.conflicts,
    );
    let _gitignore_exact = validate_memory_gitignore(
        kind_at(&inventory.prestates, ".agents/memory/.gitignore"),
        inventory.contents.get(".agents/memory/.gitignore"),
        &mut inventory.conflicts,
    );
    let mut full_conflicts = Vec::new();
    let full_valid = validate_full_layout(
        &root,
        policy.exact,
        flat_kind,
        schema_exact,
        index_kind,
        pages_kind,
        &inventory,
        &mut full_conflicts,
    );
    inventory.conflicts.extend(full_conflicts);

    let structured = schema_kind != NodeKind::Missing
        || index_kind != NodeKind::Missing
        || pages_kind != NodeKind::Missing;
    let artifact_present = policy.heading_count > 0
        || memory_kind != NodeKind::Missing
        || agents_kind == NodeKind::Symlink
        || !inventory.conflicts.is_empty();

    normalize_conflicts(&mut inventory.conflicts);
    inventory
        .prestates
        .sort_by(|left, right| left.path.cmp(&right.path));

    let layout = if !artifact_present {
        LayoutClass::Absent
    } else if inventory.conflicts.is_empty() && policy.exact && flat_valid && !structured {
        LayoutClass::Minimal
    } else if inventory.conflicts.is_empty() && full_valid {
        LayoutClass::Full
    } else {
        LayoutClass::Partial
    };

    let attainable = if !inventory.conflicts.is_empty() {
        Vec::new()
    } else {
        match layout {
            LayoutClass::Absent | LayoutClass::Minimal => {
                vec![InitMode::Minimal, InitMode::Full]
            }
            LayoutClass::Full => vec![InitMode::Full],
            LayoutClass::Partial if !structured => {
                vec![InitMode::Minimal, InitMode::Full]
            }
            LayoutClass::Partial => vec![InitMode::Full],
        }
    };

    let mut inspection = InitInspection {
        ok: true,
        root: root_text.to_owned(),
        layout,
        recommended_mode: recommended_mode(&attainable),
        attainable,
        inspection_sha256: String::new(),
        dirty_paths,
        prestates: inventory.prestates,
        conflicts: inventory.conflicts,
    };
    // This digest is an opaque, local approval token rather than a digest that
    // callers can reconstruct from the public JSON fields alone. Bind the
    // pinned repository and `.git` identities so an identically shaped
    // repository swapped into the same pathname cannot inherit approval. Raw
    // device and inode values deliberately remain outside the serialized API.
    let mut encoded = serde_json::to_vec(&inspection)?;
    encoded.extend_from_slice(b"\0yams-repository-binding-v2\0");
    encoded.extend_from_slice(&inventory.repository_binding);
    inspection.inspection_sha256 = sha256(&encoded);
    Ok(RepositorySnapshot {
        inspection,
        contents: inventory.contents,
    })
}

fn canonical_repository_root(root: &Path) -> Result<PathBuf, InitError> {
    let canonical = root.canonicalize().map_err(|_| {
        InitError::InvalidRoot("repository root cannot be canonicalized".to_owned())
    })?;
    let root_metadata = fs::metadata(&canonical)
        .map_err(|_| InitError::InvalidRoot("repository root is not accessible".to_owned()))?;
    if !root_metadata.is_dir() {
        return Err(InitError::InvalidRoot(
            "repository root is not a directory".to_owned(),
        ));
    }
    if canonical.to_str().is_none() {
        return Err(InitError::InvalidRoot(
            "canonical repository path is not UTF-8".to_owned(),
        ));
    }
    Ok(canonical)
}

struct CapturedDirectory {
    fd: OwnedFd,
    state: NodeState,
    signatures: Vec<EntrySignature>,
    name: OsString,
    path: String,
}

enum GitBinding {
    Directory {
        entry_state: NodeState,
        fd: OwnedFd,
    },
    Pointer {
        entry_state: NodeState,
        file: File,
        bytes: Vec<u8>,
        admin_path: PathBuf,
        admin_state: NodeState,
        admin_fd: OwnedFd,
    },
}

struct CapturedNode {
    prestate: NodePrestate,
    content: Option<Vec<u8>>,
    state: Option<NodeState>,
    directory: Option<CapturedDirectory>,
    conflicts: Vec<InitConflict>,
}

fn inventory_repository(
    root: &Path,
    root_text: &str,
    hooks: &mut impl InventoryHooks,
) -> Result<(Inventory, Vec<String>), InitError> {
    let root_candidate = path_state(root).map_err(|_| {
        InitError::InvalidRoot("repository root is not safely accessible".to_owned())
    })?;
    if !root_candidate.kind.is_dir() {
        return Err(InitError::InvalidRoot(
            "repository root is not a directory".to_owned(),
        ));
    }
    let root_fd = rfs::open(root, DIRECTORY_FLAGS, Mode::empty()).map_err(|_| {
        InitError::InvalidRoot("repository root is not safely accessible".to_owned())
    })?;
    let root_opened = descriptor_state(&root_fd, "", "inspect opened repository root")?;
    if root_opened != root_candidate || !root_opened.kind.is_dir() {
        return Err(stable_race(""));
    }

    let git_binding = capture_git_binding(root, root_fd.as_fd())?;

    let mut inventory = Inventory {
        prestates: Vec::new(),
        contents: BTreeMap::new(),
        conflicts: Vec::new(),
        repository_binding: repository_binding(root_candidate, &git_binding),
        consistent: false,
    };
    let agents = capture_node(root_fd.as_fd(), OsStr::new("AGENTS.md"), "AGENTS.md", hooks)?;
    let agents_file_state = agents.state;
    let agents_file_directory = record_node(agents, &mut inventory);

    let agents_dir_node = capture_node(root_fd.as_fd(), OsStr::new(".agents"), ".agents", hooks)?;
    let agents_directory_state = agents_dir_node.state;
    let agents_dir = record_node(agents_dir_node, &mut inventory);

    let mut memory_dir = None;
    let mut pages_dir = None;
    let mut runtime_lock_state = None;
    if let Some(ref agents_directory) = agents_dir {
        let memory = capture_node(
            agents_directory.fd.as_fd(),
            OsStr::new("memory"),
            ".agents/memory",
            hooks,
        )?;
        memory_dir = record_node(memory, &mut inventory);
    } else {
        push_missing_descendants(&mut inventory, &BASE_PATHS[2..]);
    }

    if let Some(ref memory_directory) = memory_dir {
        for (name, relative) in [
            ("project-context.md", ".agents/memory/project-context.md"),
            (".gitignore", ".agents/memory/.gitignore"),
            ("SCHEMA.md", ".agents/memory/SCHEMA.md"),
            ("INDEX.md", ".agents/memory/INDEX.md"),
        ] {
            let node = capture_node(
                memory_directory.fd.as_fd(),
                OsStr::new(name),
                relative,
                hooks,
            )?;
            record_node(node, &mut inventory);
        }
        let runtime_lock = capture_runtime_lock(memory_directory.fd.as_fd())?;
        runtime_lock_state = runtime_lock.state;
        record_node(runtime_lock, &mut inventory);
        hooks.after_runtime_lock_captured();
        let pages = capture_node(
            memory_directory.fd.as_fd(),
            OsStr::new("pages"),
            ".agents/memory/pages",
            hooks,
        )?;
        pages_dir = record_node(pages, &mut inventory);
    } else if agents_dir.is_some() {
        push_missing_descendants(&mut inventory, &BASE_PATHS[3..]);
    }

    if let Some(ref pages_directory) = pages_dir {
        for signature in &pages_directory.signatures {
            let Some(name) = signature.name.to_str() else {
                push_conflict(
                    &mut inventory.conflicts,
                    ".agents/memory/pages",
                    "non-utf8-page-name",
                    "The pages directory contains a non-UTF-8 entry name.",
                );
                continue;
            };
            let relative = format!(".agents/memory/pages/{name}");
            let node = capture_node(
                pages_directory.fd.as_fd(),
                &signature.name,
                &relative,
                hooks,
            )?;
            record_node(node, &mut inventory);
        }
    }

    let first_status = git_status_output(root_text)?;
    hooks.after_first_git_status();
    revalidate_inventory_bindings(
        root,
        root_fd.as_fd(),
        root_candidate,
        &git_binding,
        agents_file_state,
        agents_file_directory.as_ref(),
        agents_directory_state,
        agents_dir.as_ref(),
        memory_dir.as_ref(),
        pages_dir.as_ref(),
        runtime_lock_state,
    )?;
    let second_status = git_status_output(root_text)?;
    revalidate_inventory_bindings(
        root,
        root_fd.as_fd(),
        root_candidate,
        &git_binding,
        agents_file_state,
        agents_file_directory.as_ref(),
        agents_directory_state,
        agents_dir.as_ref(),
        memory_dir.as_ref(),
        pages_dir.as_ref(),
        runtime_lock_state,
    )?;
    if first_status != second_status {
        return Err(InitError::Git(
            "git status changed during repository inspection".to_owned(),
        ));
    }
    let dirty_paths = parse_git_status(&first_status, &mut inventory.conflicts)?;
    inventory.consistent = true;
    Ok((inventory, dirty_paths))
}

fn repository_binding(root: NodeState, git: &GitBinding) -> Vec<u8> {
    let mut binding = Vec::new();
    append_binding_identity(&mut binding, root);
    match git {
        GitBinding::Directory { entry_state, .. } => {
            binding.push(b'd');
            append_binding_identity(&mut binding, *entry_state);
        }
        GitBinding::Pointer {
            entry_state,
            bytes,
            admin_state,
            ..
        } => {
            binding.push(b'f');
            append_binding_identity(&mut binding, *entry_state);
            binding.extend_from_slice(&(bytes.len() as u64).to_be_bytes());
            binding.extend_from_slice(bytes);
            append_binding_identity(&mut binding, *admin_state);
        }
    }
    binding
}

fn append_binding_identity(binding: &mut Vec<u8>, state: NodeState) {
    binding.extend_from_slice(&state.device.to_be_bytes());
    binding.extend_from_slice(&state.inode.to_be_bytes());
}

fn capture_git_binding(root: &Path, root_fd: BorrowedFd<'_>) -> Result<GitBinding, InitError> {
    let entry_state = named_state_optional(root_fd, OsStr::new(".git"), ".git")
        .map_err(|_| invalid_git_root())?
        .ok_or_else(invalid_git_root)?;
    if entry_state.kind.is_dir() {
        let fd = rfs::openat(root_fd, ".git", DIRECTORY_FLAGS, Mode::empty())
            .map_err(|_| invalid_git_root())?;
        let opened = descriptor_state(&fd, ".git", "inspect opened .git directory")
            .map_err(|_| invalid_git_root())?;
        if opened != entry_state
            || named_state_optional(root_fd, OsStr::new(".git"), ".git")
                .map_err(|_| invalid_git_root())?
                != Some(entry_state)
        {
            return Err(stable_race(".git"));
        }
        return Ok(GitBinding::Directory { entry_state, fd });
    }
    if !entry_state.kind.is_file()
        || entry_state.size > GITDIR_POINTER_MAX_BYTES as u64
        || entry_state.nlink != 1
    {
        return Err(invalid_git_root());
    }

    let fd =
        rfs::openat(root_fd, ".git", FILE_FLAGS, Mode::empty()).map_err(|_| invalid_git_root())?;
    let opened = descriptor_state(&fd, ".git", "inspect opened .git pointer")
        .map_err(|_| invalid_git_root())?;
    if opened != entry_state || !opened.kind.is_file() {
        return Err(stable_race(".git"));
    }
    let file = File::from(fd);
    let bytes = read_git_pointer(&file).map_err(|_| invalid_git_root())?;
    let after = descriptor_state(&file, ".git", "reinspect .git pointer")
        .map_err(|_| invalid_git_root())?;
    let named = named_state_optional(root_fd, OsStr::new(".git"), ".git")
        .map_err(|_| invalid_git_root())?;
    if after != entry_state || named != Some(entry_state) || bytes.len() as u64 != entry_state.size
    {
        return Err(stable_race(".git"));
    }

    let admin_path = parse_gitdir_pointer(root, &bytes)?;
    let admin_state = path_state(&admin_path).map_err(|_| invalid_git_root())?;
    if !admin_state.kind.is_dir() {
        return Err(invalid_git_root());
    }
    let admin_fd =
        rfs::open(&admin_path, DIRECTORY_FLAGS, Mode::empty()).map_err(|_| invalid_git_root())?;
    let admin_opened = descriptor_state(&admin_fd, ".git administrative directory", "inspect")
        .map_err(|_| invalid_git_root())?;
    if admin_opened != admin_state
        || path_state(&admin_path).map_err(|_| invalid_git_root())? != admin_state
    {
        return Err(stable_race(".git administrative directory"));
    }
    Ok(GitBinding::Pointer {
        entry_state,
        file,
        bytes,
        admin_path,
        admin_state,
        admin_fd,
    })
}

fn read_git_pointer(file: &File) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    let mut offset = 0_u64;
    loop {
        let mut buffer = [0_u8; 1024];
        let read = match file.read_at(&mut buffer, offset) {
            Ok(read) => read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        if read == 0 {
            break;
        }
        if bytes.len() + read > GITDIR_POINTER_MAX_BYTES {
            return Err(std::io::Error::other(".git pointer exceeds its size bound"));
        }
        bytes.extend_from_slice(&buffer[..read]);
        offset += read as u64;
    }
    Ok(bytes)
}

fn parse_gitdir_pointer(root: &Path, bytes: &[u8]) -> Result<PathBuf, InitError> {
    decode_gitdir_pointer(root, bytes)?
        .canonicalize()
        .map_err(|_| invalid_git_root())
}

fn decode_gitdir_pointer(root: &Path, bytes: &[u8]) -> Result<PathBuf, InitError> {
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(invalid_git_root());
    }
    let payload = bytes
        .strip_prefix(b"gitdir: ")
        .ok_or_else(invalid_git_root)?;
    // A Git pointer may end at EOF or use LF/CRLF. Accept exactly one line
    // terminator, while rejecting embedded or additional breaks as garbage.
    let payload = payload
        .strip_suffix(b"\r\n")
        .or_else(|| payload.strip_suffix(b"\n"))
        .unwrap_or(payload);
    if payload.is_empty() || payload.contains(&b'\n') || payload.contains(&b'\r') {
        return Err(invalid_git_root());
    }
    let pointer = PathBuf::from(OsString::from_vec(payload.to_vec()));
    Ok(if pointer.is_absolute() {
        pointer
    } else {
        root.join(pointer)
    })
}

fn invalid_git_root() -> InitError {
    InitError::InvalidRoot("repository root has no safe .git object".to_owned())
}

fn revalidate_git_binding(
    root: &Path,
    root_fd: BorrowedFd<'_>,
    git: &GitBinding,
) -> Result<(), InitError> {
    match git {
        GitBinding::Directory { entry_state, fd } => {
            let descriptor = descriptor_state(fd, ".git", "reinspect .git directory")?;
            let named = named_state_optional(root_fd, OsStr::new(".git"), ".git")?;
            if descriptor != *entry_state || named != Some(*entry_state) {
                return Err(stable_race(".git"));
            }
        }
        GitBinding::Pointer {
            entry_state,
            file,
            bytes,
            admin_path,
            admin_state,
            admin_fd,
        } => {
            let descriptor = descriptor_state(file, ".git", "reinspect .git pointer")?;
            let named = named_state_optional(root_fd, OsStr::new(".git"), ".git")?;
            let current_bytes = read_git_pointer(file)
                .map_err(|source| io_error("reread .git pointer", ".git", source))?;
            let current_admin = parse_gitdir_pointer(root, &current_bytes)?;
            let admin_descriptor =
                descriptor_state(admin_fd, ".git administrative directory", "reinspect")?;
            let admin_named = path_state(admin_path)?;
            if descriptor != *entry_state
                || named != Some(*entry_state)
                || current_bytes != *bytes
                || current_admin != *admin_path
                || admin_descriptor != *admin_state
                || admin_named != *admin_state
            {
                return Err(stable_race(".git"));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn revalidate_inventory_bindings(
    root: &Path,
    root_fd: BorrowedFd<'_>,
    root_candidate: NodeState,
    git_binding: &GitBinding,
    agents_file_state: Option<NodeState>,
    agents_file_directory: Option<&CapturedDirectory>,
    agents_directory_state: Option<NodeState>,
    agents_dir: Option<&CapturedDirectory>,
    memory_dir: Option<&CapturedDirectory>,
    pages_dir: Option<&CapturedDirectory>,
    runtime_lock_state: Option<NodeState>,
) -> Result<(), InitError> {
    if let Some(memory) = memory_dir {
        verify_named_expected(
            memory.fd.as_fd(),
            OsStr::new(".write.lock"),
            RUNTIME_LOCK_PATH,
            runtime_lock_state,
        )?;
    } else if runtime_lock_state.is_some() {
        return Err(stable_race(RUNTIME_LOCK_PATH));
    }
    if let Some(directory) = pages_dir {
        let memory_parent = memory_dir.ok_or_else(|| stable_race(".agents/memory/pages"))?;
        verify_directory_binding(memory_parent.fd.as_fd(), directory)?;
    }
    if let Some(directory) = memory_dir {
        let agents_parent = agents_dir.ok_or_else(|| stable_race(".agents/memory"))?;
        verify_directory_binding(agents_parent.fd.as_fd(), directory)?;
    }
    if let Some(directory) = agents_dir {
        verify_directory_binding(root_fd, directory)?;
    } else {
        verify_named_expected(
            root_fd,
            OsStr::new(".agents"),
            ".agents",
            agents_directory_state,
        )?;
    }
    if let Some(directory) = agents_file_directory {
        verify_directory_binding(root_fd, directory)?;
    } else {
        verify_named_expected(
            root_fd,
            OsStr::new("AGENTS.md"),
            "AGENTS.md",
            agents_file_state,
        )?;
    }
    revalidate_git_binding(root, root_fd, git_binding)?;
    let root_descriptor = descriptor_state(root_fd, "", "reinspect repository root")?;
    let root_named = path_state(root)?;
    if root_descriptor != root_candidate || root_named != root_candidate {
        return Err(stable_race(""));
    }
    Ok(())
}

fn capture_node(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    relative: &str,
    hooks: &mut impl InventoryHooks,
) -> Result<CapturedNode, InitError> {
    let Some(candidate) = named_state_optional(parent, name, relative)? else {
        return Ok(CapturedNode {
            prestate: missing_prestate(relative),
            content: None,
            state: None,
            directory: None,
            conflicts: Vec::new(),
        });
    };
    let kind = node_kind(candidate.kind);
    let mut prestate = NodePrestate {
        path: relative.to_owned(),
        kind,
        mode: Some(candidate.mode & 0o7777),
        sha256: None,
        entries_sha256: None,
    };
    let mut content = None;
    let mut directory = None;
    let mut conflicts = Vec::new();
    match kind {
        NodeKind::File if candidate.size > yams_core::MAX_FILE_BYTES => push_conflict(
            &mut conflicts,
            relative,
            "oversized-file",
            "The owned file exceeds the supported size limit.",
        ),
        NodeKind::File => {
            let bytes = capture_file(parent, name, relative, candidate, hooks)?;
            prestate.sha256 = Some(sha256(&bytes));
            content = Some(bytes);
        }
        NodeKind::Directory => {
            let captured = capture_directory(parent, name, relative, candidate, hooks)?;
            prestate.entries_sha256 = Some(directory_entries_digest(&captured.signatures));
            directory = Some(captured);
        }
        NodeKind::Missing | NodeKind::Symlink | NodeKind::Other => {}
    }
    Ok(CapturedNode {
        prestate,
        content,
        state: Some(candidate),
        directory,
        conflicts,
    })
}

fn capture_runtime_lock(parent: BorrowedFd<'_>) -> Result<CapturedNode, InitError> {
    let Some(state) = named_state_optional(parent, OsStr::new(".write.lock"), RUNTIME_LOCK_PATH)?
    else {
        return Ok(CapturedNode {
            prestate: missing_prestate(RUNTIME_LOCK_PATH),
            content: None,
            state: None,
            directory: None,
            conflicts: Vec::new(),
        });
    };
    let kind = node_kind(state.kind);
    let mut conflicts = Vec::new();
    if kind != NodeKind::File {
        push_conflict(
            &mut conflicts,
            RUNTIME_LOCK_PATH,
            "unsafe-runtime-lock",
            "The Yams runtime lock is not a regular file.",
        );
    } else {
        let real_uid = getuid().as_raw();
        if state.uid != real_uid && state.uid != 0 {
            push_conflict(
                &mut conflicts,
                RUNTIME_LOCK_PATH,
                "unsafe-runtime-lock",
                "The Yams runtime lock has an unsafe owner.",
            );
        }
        if state.nlink != 1 {
            push_conflict(
                &mut conflicts,
                RUNTIME_LOCK_PATH,
                "unsafe-runtime-lock",
                "The Yams runtime lock must have exactly one hard link.",
            );
        }
    }
    Ok(CapturedNode {
        prestate: NodePrestate {
            path: RUNTIME_LOCK_PATH.to_owned(),
            kind,
            mode: Some(state.mode & 0o7777),
            sha256: None,
            entries_sha256: None,
        },
        content: None,
        state: Some(state),
        directory: None,
        conflicts,
    })
}

fn capture_file(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    relative: &str,
    candidate: NodeState,
    hooks: &mut impl InventoryHooks,
) -> Result<Vec<u8>, InitError> {
    let fd = match rfs::openat(parent, name, FILE_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(error) => {
            if named_state_optional(parent, name, relative)? != Some(candidate) {
                return Err(stable_race(relative));
            }
            return Err(io_errno(
                "open file without following links",
                relative,
                error,
            ));
        }
    };
    let opened = descriptor_state(&fd, relative, "inspect opened file")?;
    if opened != candidate || !opened.kind.is_file() {
        return Err(stable_race(relative));
    }
    let mut file = File::from(fd);
    let mut bytes = Vec::new();
    file.by_ref()
        .take(yams_core::MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| io_error("read file", relative, source))?;
    hooks.after_file_read(relative);
    let after = descriptor_state(&file, relative, "reinspect read file")?;
    let named = named_state_optional(parent, name, relative)?;
    if after != candidate
        || named != Some(candidate)
        || after.size != bytes.len() as u64
        || bytes.len() as u64 > yams_core::MAX_FILE_BYTES
    {
        return Err(stable_race(relative));
    }
    Ok(bytes)
}

fn capture_directory(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    relative: &str,
    candidate: NodeState,
    hooks: &mut impl InventoryHooks,
) -> Result<CapturedDirectory, InitError> {
    let fd = match rfs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(error) => {
            if named_state_optional(parent, name, relative)? != Some(candidate) {
                return Err(stable_race(relative));
            }
            return Err(io_errno(
                "open directory without following links",
                relative,
                error,
            ));
        }
    };
    let opened = descriptor_state(&fd, relative, "inspect opened directory")?;
    if opened != candidate || !opened.kind.is_dir() {
        return Err(stable_race(relative));
    }
    hooks.after_directory_opened(relative);
    let signatures = enumerate_directory(fd.as_fd(), relative)?;
    hooks.after_directory_enumerated(relative);
    let captured = CapturedDirectory {
        fd,
        state: candidate,
        signatures,
        name: name.to_os_string(),
        path: relative.to_owned(),
    };
    verify_directory_binding(parent, &captured)?;
    Ok(captured)
}

fn enumerate_directory(
    directory: BorrowedFd<'_>,
    relative: &str,
) -> Result<Vec<EntrySignature>, InitError> {
    let mut stream = Dir::read_from(directory)
        .map_err(|error| io_errno("open directory stream", relative, error))?;
    let mut names = Vec::new();
    for entry in &mut stream {
        let entry = entry.map_err(|error| io_errno("read directory entry", relative, error))?;
        let bytes = entry.file_name().to_bytes();
        if bytes != b"." && bytes != b".." {
            names.push(OsString::from_vec(bytes.to_vec()));
        }
    }
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    names
        .into_iter()
        .map(|name| {
            let path = if relative.is_empty() {
                name.to_string_lossy().into_owned()
            } else {
                format!("{relative}/{}", name.to_string_lossy())
            };
            named_state_required(directory, &name, &path)
                .map(|state| EntrySignature { name, state })
        })
        .collect()
}

fn verify_directory_binding(
    parent: BorrowedFd<'_>,
    directory: &CapturedDirectory,
) -> Result<(), InitError> {
    let descriptor = descriptor_state(
        &directory.fd,
        &directory.path,
        "reinspect directory descriptor",
    )?;
    let named = named_state_optional(parent, &directory.name, &directory.path)?;
    let signatures = enumerate_directory(directory.fd.as_fd(), &directory.path)?;
    if descriptor != directory.state
        || named != Some(directory.state)
        || signatures != directory.signatures
    {
        return Err(stable_race(&directory.path));
    }
    Ok(())
}

fn verify_named_expected(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    relative: &str,
    expected: Option<NodeState>,
) -> Result<(), InitError> {
    if named_state_optional(parent, name, relative)? != expected {
        return Err(stable_race(relative));
    }
    Ok(())
}

fn record_node(node: CapturedNode, inventory: &mut Inventory) -> Option<CapturedDirectory> {
    if let Some(content) = node.content {
        inventory
            .contents
            .insert(node.prestate.path.clone(), content);
    }
    inventory.conflicts.extend(node.conflicts);
    inventory.prestates.push(node.prestate);
    node.directory
}

fn push_missing_descendants(inventory: &mut Inventory, paths: &[&str]) {
    inventory
        .prestates
        .extend(paths.iter().map(|path| missing_prestate(path)));
}

fn directory_entries_digest(signatures: &[EntrySignature]) -> String {
    let mut encoded = Vec::new();
    for signature in signatures {
        let name = signature.name.as_bytes();
        encoded.extend_from_slice(&(name.len() as u64).to_be_bytes());
        encoded.extend_from_slice(name);
        encoded.push(kind_tag(node_kind(signature.state.kind)));
        encoded.extend_from_slice(&(signature.state.mode & 0o7777).to_be_bytes());
    }
    sha256(&encoded)
}

fn path_state(path: &Path) -> Result<NodeState, InitError> {
    rfs::statat(rfs::CWD, path, AtFlags::SYMLINK_NOFOLLOW)
        .map(|stat| NodeState::from_stat(&stat))
        .map_err(|error| io_errno("inspect repository root", ".", error))
}

fn named_state_optional(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    relative: &str,
) -> Result<Option<NodeState>, InitError> {
    match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(NodeState::from_stat(&stat))),
        Err(Errno::NOENT) => Ok(None),
        Err(error) => Err(io_errno("inspect", relative, error)),
    }
}

fn named_state_required(
    parent: BorrowedFd<'_>,
    name: &OsStr,
    relative: &str,
) -> Result<NodeState, InitError> {
    named_state_optional(parent, name, relative)?.ok_or_else(|| stable_race(relative))
}

fn descriptor_state(
    fd: impl AsFd,
    relative: &str,
    operation: &'static str,
) -> Result<NodeState, InitError> {
    rfs::fstat(fd)
        .map(|stat| NodeState::from_stat(&stat))
        .map_err(|error| io_errno(operation, relative, error))
}

fn timestamp_ns(seconds: i64, nanoseconds: i64) -> i128 {
    i128::from(seconds) * 1_000_000_000 + i128::from(nanoseconds)
}

fn validate_expected_kinds(prestates: &[NodePrestate], conflicts: &mut Vec<InitConflict>) {
    for prestate in prestates {
        if prestate.kind == NodeKind::Missing {
            continue;
        }
        if prestate.path == RUNTIME_LOCK_PATH {
            continue;
        }
        if prestate.kind == NodeKind::Symlink {
            push_conflict(
                conflicts,
                &prestate.path,
                "unsafe-symlink",
                "An owned repository boundary is a symlink and was not followed.",
            );
            continue;
        }
        let expected_directory = matches!(
            prestate.path.as_str(),
            ".agents" | ".agents/memory" | ".agents/memory/pages"
        );
        let expected_file = prestate.path == "AGENTS.md"
            || prestate.path == ".agents/memory/project-context.md"
            || prestate.path == ".agents/memory/.gitignore"
            || prestate.path == ".agents/memory/SCHEMA.md"
            || prestate.path == ".agents/memory/INDEX.md"
            || prestate.path.starts_with(".agents/memory/pages/");
        let valid = (expected_directory && prestate.kind == NodeKind::Directory)
            || (expected_file && prestate.kind == NodeKind::File);
        if !valid {
            push_conflict(
                conflicts,
                &prestate.path,
                "unexpected-node-kind",
                "An owned repository path has an unexpected filesystem kind.",
            );
        }
        if prestate.path.starts_with(".agents/memory/pages/") && !prestate.path.ends_with(".md") {
            push_conflict(
                conflicts,
                &prestate.path,
                "unexpected-node-kind",
                "The pages directory contains an unexpected entry.",
            );
        }
    }
}

fn validate_flat_page(
    kind: NodeKind,
    bytes: Option<&Vec<u8>>,
    conflicts: &mut Vec<InitConflict>,
) -> bool {
    if kind != NodeKind::File {
        return false;
    }
    if page_has_slug(bytes, "project-context") {
        true
    } else {
        push_conflict(
            conflicts,
            ".agents/memory/project-context.md",
            "invalid-project-page",
            "The flat project context is not a valid project-context wiki page.",
        );
        false
    }
}

fn validate_memory_gitignore(
    kind: NodeKind,
    bytes: Option<&Vec<u8>>,
    conflicts: &mut Vec<InitConflict>,
) -> bool {
    if kind != NodeKind::File {
        return false;
    }
    if bytes.is_some_and(|bytes| bytes.as_slice() == MEMORY_GITIGNORE.as_bytes()) {
        true
    } else if bytes.is_some() {
        push_conflict(
            conflicts,
            ".agents/memory/.gitignore",
            "gitignore-mismatch",
            "The memory gitignore differs from the embedded lock-ignore file.",
        );
        false
    } else {
        false
    }
}

fn validate_schema(
    kind: NodeKind,
    bytes: Option<&Vec<u8>>,
    conflicts: &mut Vec<InitConflict>,
) -> bool {
    if kind != NodeKind::File {
        return false;
    }
    if bytes.is_some_and(|bytes| bytes.as_slice() == SCHEMA.as_bytes()) {
        true
    } else if bytes.is_some() {
        push_conflict(
            conflicts,
            ".agents/memory/SCHEMA.md",
            "schema-mismatch",
            "SCHEMA.md differs from the embedded layout schema.",
        );
        false
    } else {
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn validate_full_layout(
    root: &Path,
    policy_exact: bool,
    flat_kind: NodeKind,
    schema_exact: bool,
    index_kind: NodeKind,
    pages_kind: NodeKind,
    inventory: &Inventory,
    conflicts: &mut Vec<InitConflict>,
) -> bool {
    let structured_present =
        schema_exact || index_kind != NodeKind::Missing || pages_kind != NodeKind::Missing;
    if !structured_present {
        return false;
    }

    let page_prefix = ".agents/memory/pages/";
    let page_nodes: Vec<_> = inventory
        .prestates
        .iter()
        .filter(|node| node.path.starts_with(page_prefix))
        .collect();
    let mut all_pages_valid = true;
    let mut canonical_project = false;
    let mut captured_pages = Vec::new();
    for node in page_nodes {
        if node.kind != NodeKind::File || !node.path.ends_with(".md") {
            all_pages_valid = false;
            continue;
        }
        let name = &node.path[page_prefix.len()..];
        let slug = name.strip_suffix(".md").unwrap_or(name);
        let bytes = inventory.contents.get(&node.path);
        if !page_has_slug(bytes, slug) {
            let (code, detail) = if name == "project-context.md" {
                (
                    "invalid-project-page",
                    "The structured project context is not a valid project-context wiki page.",
                )
            } else {
                (
                    "invalid-full-wiki",
                    "A structured wiki page is invalid or does not match its filename.",
                )
            };
            push_conflict(conflicts, &node.path, code, detail);
            all_pages_valid = false;
            continue;
        }
        if name == "project-context.md" {
            canonical_project = true;
        }
        if let Some(bytes) = bytes {
            captured_pages.push(CapturedPage {
                name: OsStr::new(name).to_os_string(),
                outcome: CapturedPageOutcome::Present(bytes.clone()),
            });
        }
    }

    let shape_complete = policy_exact
        && flat_kind == NodeKind::Missing
        && schema_exact
        && index_kind == NodeKind::File
        && pages_kind == NodeKind::Directory
        && canonical_project
        && all_pages_valid;
    if !shape_complete {
        return false;
    }
    let Some(index) = inventory.contents.get(".agents/memory/INDEX.md") else {
        return false;
    };
    let snapshot = WikiSnapshot {
        corpus: root.join(".agents/memory"),
        index: index.clone(),
        pages: captured_pages,
        isolated: inventory.consistent,
        isolation_note: None,
    };
    let report = validate_wiki(&snapshot);
    if !report.failures.is_empty() || !compat_snapshot(&snapshot).violations.is_empty() {
        push_conflict(
            conflicts,
            ".agents/memory",
            "invalid-full-wiki",
            "The structured wiki fails canonical validation or compatibility checks.",
        );
        return false;
    }
    true
}

fn page_has_slug(bytes: Option<&Vec<u8>>, expected_slug: &str) -> bool {
    bytes
        .and_then(|bytes| std::str::from_utf8(bytes).ok())
        .and_then(|source| parse_wiki_page(source).ok())
        .is_some_and(|page| page.slug == expected_slug)
}

fn git_status_output(root_text: &str) -> Result<Vec<u8>, InitError> {
    let mut command = Command::new("git");
    command.env_clear();
    command.env(
        "PATH",
        std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
    );
    for name in ["HOME", "XDG_CONFIG_HOME"] {
        if let Some(value) = std::env::var_os(name) {
            command.env(name, value);
        }
    }
    let output = command
        .args([
            "--no-optional-locks",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
            "-C",
            root_text,
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--",
            "AGENTS.md",
            ".agents/memory",
        ])
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .output()
        .map_err(|_| InitError::Git("could not run git status".to_owned()))?;
    if !output.status.success() {
        return Err(InitError::Git(
            "git status exited unsuccessfully".to_owned(),
        ));
    }
    Ok(output.stdout)
}

fn parse_git_status(
    output: &[u8],
    conflicts: &mut Vec<InitConflict>,
) -> Result<Vec<String>, InitError> {
    let mut paths = BTreeSet::new();
    let mut cursor = 0;
    while cursor < output.len() {
        let end = output[cursor..]
            .iter()
            .position(|byte| *byte == 0)
            .map(|offset| cursor + offset)
            .ok_or_else(|| InitError::Git("git status returned invalid data".to_owned()))?;
        let record = &output[cursor..end];
        if record.len() < 4
            || record[2] != b' '
            || !valid_porcelain_status(record[0])
            || !valid_porcelain_status(record[1])
        {
            return Err(InitError::Git(
                "git status returned invalid data".to_owned(),
            ));
        }
        insert_git_path(&mut paths, conflicts, &record[3..])?;
        cursor = end + 1;
        if matches!(record[0], b'R' | b'C') || matches!(record[1], b'R' | b'C') {
            let end = output[cursor..]
                .iter()
                .position(|byte| *byte == 0)
                .map(|offset| cursor + offset)
                .ok_or_else(|| InitError::Git("git status returned invalid data".to_owned()))?;
            insert_git_path(&mut paths, conflicts, &output[cursor..end])?;
            cursor = end + 1;
        }
    }
    paths.remove(RUNTIME_LOCK_PATH);
    Ok(paths.into_iter().collect())
}

const fn valid_porcelain_status(byte: u8) -> bool {
    matches!(
        byte,
        b' ' | b'M' | b'T' | b'A' | b'D' | b'R' | b'C' | b'U' | b'?' | b'!'
    )
}

/// Records a path parsed from `git status --porcelain=v1 -z` output.
///
/// A record with an empty path or an embedded NUL indicates the porcelain
/// stream itself is malformed and inspection cannot trust any of it, so that
/// remains a hard error. A path that fails UTF-8 decoding is different: the
/// filesystem entry is real (the directory-scan layer can and does capture
/// it independently, see the `non-utf8-page-name` conflict above), so
/// failing the whole inspection over it would make an otherwise-diagnosable
/// repository state unreadable. Instead this pushes a dedicated blocking
/// conflict — mirroring how non-UTF-8 page names are already surfaced — and
/// leaves the path out of the returned `dirty_paths` set, since a lossy
/// rendering could collide with, or be mistaken for, an unrelated real path.
fn insert_git_path(
    paths: &mut BTreeSet<String>,
    conflicts: &mut Vec<InitConflict>,
    bytes: &[u8],
) -> Result<(), InitError> {
    if bytes.is_empty() || bytes.contains(&0) {
        return Err(InitError::Git(
            "git status returned invalid data".to_owned(),
        ));
    }
    match std::str::from_utf8(bytes) {
        Ok(path) => {
            paths.insert(path.to_owned());
        }
        Err(_) => {
            push_conflict(
                conflicts,
                ".agents/memory",
                "non-utf8-git-status-path",
                &format!(
                    "git status reported a non-UTF-8 path under the managed memory tree \
                     (lossy: {}).",
                    String::from_utf8_lossy(bytes)
                ),
            );
        }
    }
    Ok(())
}

fn kind_at(prestates: &[NodePrestate], path: &str) -> NodeKind {
    prestates
        .iter()
        .find(|prestate| prestate.path == path)
        .map_or(NodeKind::Missing, |prestate| prestate.kind)
}

fn node_kind(file_type: FileType) -> NodeKind {
    if file_type.is_file() {
        NodeKind::File
    } else if file_type.is_dir() {
        NodeKind::Directory
    } else if file_type.is_symlink() {
        NodeKind::Symlink
    } else {
        NodeKind::Other
    }
}

const fn kind_tag(kind: NodeKind) -> u8 {
    match kind {
        NodeKind::Missing => 0,
        NodeKind::File => 1,
        NodeKind::Directory => 2,
        NodeKind::Symlink => 3,
        NodeKind::Other => 4,
    }
}

fn missing_prestate(path: &str) -> NodePrestate {
    NodePrestate {
        path: path.to_owned(),
        kind: NodeKind::Missing,
        mode: None,
        sha256: None,
        entries_sha256: None,
    }
}

fn push_conflict(conflicts: &mut Vec<InitConflict>, path: &str, code: &str, detail: &str) {
    conflicts.push(InitConflict {
        path: path.to_owned(),
        code: code.to_owned(),
        detail: detail.to_owned(),
    });
}

fn normalize_conflicts(conflicts: &mut Vec<InitConflict>) {
    conflicts.sort_by(|left, right| {
        (&left.path, &left.code, &left.detail).cmp(&(&right.path, &right.code, &right.detail))
    });
    conflicts.dedup();
}

fn io_error(operation: &'static str, path: &str, source: std::io::Error) -> InitError {
    InitError::Io {
        operation,
        path: PathBuf::from(path),
        source,
    }
}

fn io_errno(operation: &'static str, path: &str, error: Errno) -> InitError {
    io_error(
        operation,
        if path.is_empty() { "." } else { path },
        std::io::Error::from_raw_os_error(error.raw_os_error()),
    )
}

fn stable_race(path: &str) -> InitError {
    io_error(
        "capture stable inventory",
        if path.is_empty() { "." } else { path },
        std::io::Error::other("repository path changed during inspection"),
    )
}

#[cfg(test)]
include!("tests.rs");
