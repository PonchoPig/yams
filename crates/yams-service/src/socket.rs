//! Fail-closed ownership and lifecycle handling for the private service socket.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Component, Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use rustix::fs::{self, AtFlags, FileType, FlockOperation, Mode, OFlags};
use rustix::process::{geteuid, umask};
use thiserror::Error;

const PRIVATE_SOCKET_MODE: u32 = 0o600;
const PRIVATE_ANCESTOR_FORBIDDEN_BITS: u32 = 0o022;
const DIRECTORY_OPEN_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK);

static UMASK_BIND_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// How the service socket path was selected.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketProvenance {
    /// The caller omitted `--socket`, so Yams computed its private default.
    ComputedDefault,
    /// The caller supplied `--socket`, including a value equal to the default.
    Explicit,
}

/// A service socket path was unsafe or could not be proven to be owned.
#[derive(Debug, Error)]
pub enum SocketError {
    /// The socket path is not an absolute, normalized path.
    #[error("invalid service socket path: {0}")]
    InvalidPath(String),
    /// An ancestor or path entry failed the private filesystem policy.
    #[error("unsafe service socket path {path}: {reason}")]
    UnsafePath {
        /// The path rejected by the safety policy.
        path: PathBuf,
        /// A stable category explaining the refusal.
        reason: &'static str,
    },
    /// A private socket already has a live listener.
    #[error("service is already running at {0}")]
    AlreadyRunning(PathBuf),
    /// Cleanup found a different path entry and therefore did not remove it.
    #[error("service socket identity changed at {0}")]
    IdentityChanged(PathBuf),
    /// The operating system rejected a lifecycle operation.
    #[error("service socket I/O: {0}")]
    Io(#[from] io::Error),
}

impl From<rustix::io::Errno> for SocketError {
    fn from(error: rustix::io::Errno) -> Self {
        Self::Io(error.into())
    }
}

/// The filesystem identity recorded immediately after a successful bind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnedSocket {
    /// The normalized absolute socket path.
    pub path: PathBuf,
    /// Device number from `stat(2)`.
    pub device: u64,
    /// Inode number from `stat(2)`.
    pub inode: u64,
    provenance: SocketProvenance,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DefaultRuntimeMetadata {
    device: u64,
    inode: u64,
    uid: u32,
    mode: u32,
    is_directory: bool,
}

impl DefaultRuntimeMetadata {
    fn from_stat(stat: &rustix::fs::Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino,
            uid: stat.st_uid,
            mode: stat.st_mode as u32,
            is_directory: FileType::from_raw_mode(stat.st_mode) == FileType::Directory,
        }
    }
}

fn default_runtime_dir_is_safe(
    by_fd: DefaultRuntimeMetadata,
    by_name: DefaultRuntimeMetadata,
    effective_uid: u32,
) -> bool {
    default_runtime_dir_has_trusted_identity(by_fd, by_name, effective_uid)
        && (by_fd.mode & 0o7777) == 0o700
        && (by_name.mode & 0o7777) == 0o700
}

fn default_runtime_dir_has_trusted_identity(
    by_fd: DefaultRuntimeMetadata,
    by_name: DefaultRuntimeMetadata,
    effective_uid: u32,
) -> bool {
    by_fd.device == by_name.device
        && by_fd.inode == by_name.inode
        && by_fd.uid == effective_uid
        && by_name.uid == effective_uid
        && by_fd.is_directory
        && by_name.is_directory
}

fn default_runtime_entry_is_owned_directory(
    metadata: DefaultRuntimeMetadata,
    effective_uid: u32,
) -> bool {
    metadata.uid == effective_uid && metadata.is_directory
}

/// Return the default service socket beneath a temporary directory.
pub fn computed_default_socket(temporary_directory: &Path) -> PathBuf {
    temporary_directory
        .join(format!("yams-{}", rustix::process::getuid().as_raw()))
        .join("service.sock")
}

/// Securely create or verify the computed default runtime directory.
///
/// The directory is opened descriptor-relatively without following a symlink
/// at its final component, then checked for stable identity, effective-user
/// ownership, and mode `0700`.
pub fn prepare_default_runtime_dir(dir: &Path) -> Result<(), SocketError> {
    let requested_dir = validate_path(dir)?;
    let requested_parent = requested_dir
        .parent()
        .ok_or_else(|| SocketError::InvalidPath(requested_dir.display().to_string()))?;
    let name = requested_dir
        .file_name()
        .ok_or_else(|| SocketError::InvalidPath(requested_dir.display().to_string()))?
        .to_owned();
    let parent = std::fs::canonicalize(requested_parent).map_err(|_| {
        unsafe_path(
            &requested_dir,
            "the default runtime parent cannot be resolved safely",
        )
    })?;
    let dir = validate_path(&parent.join(&name))?;
    let parent_fd = open_safe_parent_with_policy(&parent, &dir, AncestorPolicy::AllowStickyShared)?;

    let created = match fs::mkdirat(&parent_fd, &name, Mode::from_raw_mode(0o700)) {
        Ok(()) => true,
        Err(rustix::io::Errno::EXIST) => false,
        Err(error) => return Err(error.into()),
    };

    if created {
        let before = DefaultRuntimeMetadata::from_stat(&fs::statat(
            &parent_fd,
            &name,
            AtFlags::SYMLINK_NOFOLLOW,
        )?);
        let effective_uid = geteuid().as_raw();
        if !default_runtime_entry_is_owned_directory(before, effective_uid) {
            return Err(unsafe_path(
                &dir,
                "the newly created default runtime entry changed before mode setup",
            ));
        }
        match fs::chmodat(
            &parent_fd,
            &name,
            Mode::from_raw_mode(0o700),
            AtFlags::SYMLINK_NOFOLLOW,
        ) {
            Ok(()) => {}
            Err(error)
                if error == rustix::io::Errno::OPNOTSUPP || error == rustix::io::Errno::NOTSUP =>
            {
                // Linux does not implement AT_SYMLINK_NOFOLLOW for chmodat.
                // The entry was just created and its ancestor chain permits
                // foreign mutation only under sticky directories owned by
                // root or this process's effective user. The identity check
                // below detects any same-user replacement.
                fs::chmodat(
                    &parent_fd,
                    &name,
                    Mode::from_raw_mode(0o700),
                    AtFlags::empty(),
                )?;
            }
            Err(error) => return Err(error.into()),
        }
        let after = DefaultRuntimeMetadata::from_stat(&fs::statat(
            &parent_fd,
            &name,
            AtFlags::SYMLINK_NOFOLLOW,
        )?);
        if before.device != after.device
            || before.inode != after.inode
            || !default_runtime_entry_is_owned_directory(after, effective_uid)
            || (after.mode & 0o7777) != 0o700
        {
            return Err(unsafe_path(
                &dir,
                "the newly created default runtime entry changed during mode setup",
            ));
        }
    }

    let dir_fd = fs::openat(
        &parent_fd,
        &name,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| {
        unsafe_path(
            &dir,
            "the default runtime directory is not a safe directory",
        )
    })?;
    let opened_by_fd = DefaultRuntimeMetadata::from_stat(&fs::fstat(&dir_fd)?);
    let opened_by_name = DefaultRuntimeMetadata::from_stat(&fs::statat(
        &parent_fd,
        &name,
        AtFlags::SYMLINK_NOFOLLOW,
    )?);
    let effective_uid = geteuid().as_raw();
    if !default_runtime_dir_has_trusted_identity(opened_by_fd, opened_by_name, effective_uid) {
        return Err(unsafe_path(
            &dir,
            "the default runtime directory has unsafe ownership or identity",
        ));
    }
    let by_fd = DefaultRuntimeMetadata::from_stat(&fs::fstat(&dir_fd)?);
    let by_name = DefaultRuntimeMetadata::from_stat(&fs::statat(
        &parent_fd,
        &name,
        AtFlags::SYMLINK_NOFOLLOW,
    )?);
    if !default_runtime_dir_is_safe(by_fd, by_name, effective_uid) {
        return Err(unsafe_path(
            &dir,
            "the default runtime directory has unsafe ownership, mode, or identity",
        ));
    }
    Ok(())
}

/// Prepare the computed default directory when authorized, then bind.
///
/// An explicit path never creates a missing parent, even when its text is
/// identical to the computed default path.
pub fn bind_with_provenance(
    path: &Path,
    provenance: SocketProvenance,
) -> Result<(UnixListener, OwnedSocket), SocketError> {
    if provenance == SocketProvenance::ComputedDefault {
        let parent = path
            .parent()
            .ok_or_else(|| SocketError::InvalidPath(path.display().to_string()))?;
        prepare_default_runtime_dir(parent)?;
        return bind_prepared_default_listener(path);
    }
    bind_listener(path)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EntryIdentity {
    device: u64,
    inode: u64,
    mode: u32,
    uid: u32,
    file_type: FileType,
}

struct LockedParent {
    fd: OwnedFd,
    path: PathBuf,
    name: PathBuf,
}

#[derive(Clone, Copy)]
enum AncestorPolicy {
    Private,
    AllowStickyShared,
}

fn ancestor_permissions_are_safe(
    mode: u32,
    owner_uid: u32,
    effective_uid: u32,
    policy: AncestorPolicy,
) -> bool {
    if mode & PRIVATE_ANCESTOR_FORBIDDEN_BITS == 0 {
        return true;
    }
    matches!(policy, AncestorPolicy::AllowStickyShared)
        && mode & 0o1000 != 0
        && (owner_uid == 0 || owner_uid == effective_uid)
}

impl LockedParent {
    fn socket_name(&self) -> &Path {
        &self.name
    }
}

impl Drop for LockedParent {
    fn drop(&mut self) {
        // Closing an fd releases flock; there is no fallible work to do in Drop.
        let _ = rustix::fs::flock(&self.fd, FlockOperation::Unlock);
    }
}

/// Bind a private Unix listener and record the exact filesystem identity.
pub fn bind_listener(path: &Path) -> Result<(UnixListener, OwnedSocket), SocketError> {
    let locked = lock_parent(path)?;
    bind_locked(locked, SocketProvenance::Explicit)
}

fn bind_prepared_default_listener(path: &Path) -> Result<(UnixListener, OwnedSocket), SocketError> {
    let locked = lock_prepared_default_parent(path)?;
    bind_locked(locked, SocketProvenance::ComputedDefault)
}

fn bind_locked(
    locked: LockedParent,
    provenance: SocketProvenance,
) -> Result<(UnixListener, OwnedSocket), SocketError> {
    prepare_existing(&locked)?;

    let listener = {
        let _guard = UMASK_BIND_LOCK
            .get_or_init(|| Mutex::new(()))
            .lock()
            .map_err(|_| SocketError::InvalidPath("umask lock was poisoned".into()))?;
        let previous = umask(Mode::from_raw_mode(0o177));
        let result = UnixListener::bind(&locked.path);
        let _ = umask(previous);
        result?
    };

    let identity = match stat_entry(&locked) {
        Ok(identity) => identity,
        Err(error) => {
            drop(listener);
            return Err(error);
        }
    };
    if !is_private_socket(identity) {
        drop(listener);
        let _ = remove_if_identity(&locked, identity);
        return Err(unsafe_path(
            &locked.path,
            "bind did not create a private socket",
        ));
    }

    // The umask is defense in depth. Set and then inspect the mode through the
    // already-open parent descriptor so a path replacement cannot redirect it.
    fs::chmodat(
        &locked.fd,
        locked.socket_name(),
        Mode::from_raw_mode(PRIVATE_SOCKET_MODE as _),
        AtFlags::empty(),
    )?;
    let after_chmod = stat_entry(&locked)?;
    if !is_private_socket(after_chmod)
        || after_chmod.device != identity.device
        || after_chmod.inode != identity.inode
    {
        drop(listener);
        let _ = remove_if_identity(&locked, identity);
        return Err(unsafe_path(
            &locked.path,
            "socket identity changed during setup",
        ));
    }

    let owned = OwnedSocket {
        path: locked.path.clone(),
        device: after_chmod.device,
        inode: after_chmod.inode,
        provenance,
    };
    drop(locked);
    Ok((listener, owned))
}

/// Remove only the socket whose recorded identity is still present.
pub fn cleanup_owned_socket(owned: &OwnedSocket) -> Result<(), SocketError> {
    let locked = match owned.provenance {
        SocketProvenance::ComputedDefault => lock_prepared_default_parent(&owned.path)?,
        SocketProvenance::Explicit => lock_parent(&owned.path)?,
    };
    let current = match stat_entry(&locked) {
        Ok(identity) => identity,
        Err(SocketError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if current.file_type != FileType::Socket
        || current.device != owned.device
        || current.inode != owned.inode
    {
        return Err(SocketError::IdentityChanged(owned.path.clone()));
    }
    if !is_private_socket(current) {
        return Err(SocketError::IdentityChanged(owned.path.clone()));
    }
    remove_if_identity(&locked, current)
}

fn lock_parent(path: &Path) -> Result<LockedParent, SocketError> {
    let path = validate_path(path)?;
    let parent = path
        .parent()
        .ok_or_else(|| SocketError::InvalidPath(path.display().to_string()))?;
    let name = path
        .file_name()
        .ok_or_else(|| SocketError::InvalidPath(path.display().to_string()))?
        .to_owned();
    let fd = open_safe_parent(parent, &path)?;
    rustix::fs::flock(&fd, FlockOperation::LockExclusive)?;
    let locked = LockedParent {
        fd,
        path,
        name: PathBuf::from(name),
    };
    verify_parent_identity(&locked)?;
    Ok(locked)
}

fn lock_prepared_default_parent(path: &Path) -> Result<LockedParent, SocketError> {
    let requested_path = validate_path(path)?;
    let requested_parent = requested_path
        .parent()
        .ok_or_else(|| SocketError::InvalidPath(requested_path.display().to_string()))?;
    let name = requested_path
        .file_name()
        .ok_or_else(|| SocketError::InvalidPath(requested_path.display().to_string()))?
        .to_owned();
    // Resolve aliases such as macOS `/tmp` -> `/private/tmp` after the private
    // runtime directory exists. Binding through the canonical parent keeps a
    // mutable ancestor symlink out of the later pathname operation.
    let parent = std::fs::canonicalize(requested_parent).map_err(|_| {
        unsafe_path(
            &requested_path,
            "the prepared default runtime directory cannot be resolved safely",
        )
    })?;
    let path = validate_path(&parent.join(&name))?;
    let fd = open_safe_parent_with_policy(&parent, &path, AncestorPolicy::AllowStickyShared)?;
    rustix::fs::flock(&fd, FlockOperation::LockExclusive)?;
    let locked = LockedParent {
        fd,
        path,
        name: PathBuf::from(name),
    };
    verify_prepared_default_parent(&locked)?;
    Ok(locked)
}

fn verify_prepared_default_parent(locked: &LockedParent) -> Result<(), SocketError> {
    let parent = locked
        .path
        .parent()
        .ok_or_else(|| SocketError::InvalidPath(locked.path.display().to_string()))?;
    let by_fd = DefaultRuntimeMetadata::from_stat(&fs::fstat(&locked.fd)?);
    let visible = std::fs::symlink_metadata(parent).map_err(|_| {
        unsafe_path(
            &locked.path,
            "the prepared default runtime directory is unavailable",
        )
    })?;
    let by_name = DefaultRuntimeMetadata {
        device: visible.dev(),
        inode: visible.ino(),
        uid: visible.uid(),
        mode: visible.mode(),
        is_directory: visible.file_type().is_dir(),
    };
    if !visible.file_type().is_dir()
        || !default_runtime_dir_is_safe(by_fd, by_name, geteuid().as_raw())
    {
        return Err(unsafe_path(
            &locked.path,
            "the prepared default runtime directory changed or became unsafe",
        ));
    }
    Ok(())
}

fn validate_path(path: &Path) -> Result<PathBuf, SocketError> {
    if !path.is_absolute() {
        return Err(SocketError::InvalidPath(
            "socket path must be absolute".into(),
        ));
    }
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::RootDir => normalized.push(Path::new("/")),
            Component::Normal(value) => normalized.push(value),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                return Err(SocketError::InvalidPath(
                    "socket path must be normalized".into(),
                ));
            }
        }
    }
    if normalized != path {
        return Err(SocketError::InvalidPath(
            "socket path must be normalized".into(),
        ));
    }
    Ok(normalized)
}

fn open_safe_parent(parent: &Path, path: &Path) -> Result<OwnedFd, SocketError> {
    open_safe_parent_with_policy(parent, path, AncestorPolicy::Private)
}

fn open_safe_parent_with_policy(
    parent: &Path,
    path: &Path,
    policy: AncestorPolicy,
) -> Result<OwnedFd, SocketError> {
    let mut current = rustix::fs::openat(
        rustix::fs::CWD,
        Path::new("/"),
        DIRECTORY_OPEN_FLAGS,
        Mode::empty(),
    )
    .map_err(|_| unsafe_path(path, "an ancestor cannot be opened safely"))?;
    let mut visible = PathBuf::from("/");
    validate_directory_fd(&current, &visible, path, policy)?;
    for component in parent.components() {
        let Component::Normal(component) = component else {
            continue;
        };
        let next =
            match rustix::fs::openat(&current, component, DIRECTORY_OPEN_FLAGS, Mode::empty()) {
                Ok(next) => next,
                Err(_) => return Err(unsafe_path(path, "an ancestor cannot be opened safely")),
            };
        visible.push(component);
        validate_directory_fd(&next, &visible, path, policy)?;
        current = next;
    }
    let stat = rustix::fs::fstat(&current)?;
    let uid = geteuid().as_raw();
    if matches!(policy, AncestorPolicy::Private) && stat.st_uid != uid {
        return Err(unsafe_path(path, "the socket parent has the wrong owner"));
    }
    // `parent` is already reached descriptor-relatively; this final visible
    // check detects a rename/swap that happened while the descriptors opened.
    let metadata = std::fs::symlink_metadata(parent)
        .map_err(|_| unsafe_path(path, "the socket parent changed while opening"))?;
    if metadata.dev() != stat.st_dev as u64 || metadata.ino() != stat.st_ino as u64 {
        return Err(unsafe_path(path, "the socket parent changed while opening"));
    }
    Ok(current)
}

fn validate_directory_fd(
    fd: &OwnedFd,
    visible_path: &Path,
    socket_path: &Path,
    policy: AncestorPolicy,
) -> Result<(), SocketError> {
    let stat = rustix::fs::fstat(fd)?;
    if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
        return Err(unsafe_path(socket_path, "an ancestor is not a directory"));
    }
    let visible = std::fs::symlink_metadata(visible_path)
        .map_err(|_| unsafe_path(socket_path, "an ancestor is unavailable"))?;
    if !visible.file_type().is_dir()
        || visible.dev() != stat.st_dev as u64
        || visible.ino() != stat.st_ino as u64
    {
        return Err(unsafe_path(
            socket_path,
            "ancestor identity changed while opening",
        ));
    }
    let effective_uid = geteuid().as_raw();
    if !ancestor_permissions_are_safe(stat.st_mode as u32, stat.st_uid, effective_uid, policy)
        || !ancestor_permissions_are_safe(visible.mode(), visible.uid(), effective_uid, policy)
    {
        return Err(unsafe_path(
            socket_path,
            "an ancestor is writable by another user",
        ));
    }
    Ok(())
}

fn verify_parent_identity(locked: &LockedParent) -> Result<(), SocketError> {
    let stat = rustix::fs::fstat(&locked.fd)?;
    let visible = std::fs::symlink_metadata(locked.path.parent().unwrap())?;
    if visible.dev() != stat.st_dev as u64 || visible.ino() != stat.st_ino as u64 {
        return Err(unsafe_path(
            &locked.path,
            "parent identity changed while locking",
        ));
    }
    Ok(())
}

fn stat_entry(locked: &LockedParent) -> Result<EntryIdentity, SocketError> {
    let stat = fs::statat(&locked.fd, locked.socket_name(), AtFlags::SYMLINK_NOFOLLOW)?;
    Ok(EntryIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
        mode: stat.st_mode as u32,
        uid: stat.st_uid as u32,
        file_type: FileType::from_raw_mode(stat.st_mode),
    })
}

fn prepare_existing(locked: &LockedParent) -> Result<(), SocketError> {
    let original = match stat_entry(locked) {
        Ok(identity) => identity,
        Err(SocketError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !is_private_socket(original) {
        return Err(unsafe_path(
            &locked.path,
            "existing entry is not a private Unix socket",
        ));
    }

    match UnixStream::connect(&locked.path) {
        Ok(_) => return Err(SocketError::AlreadyRunning(locked.path.clone())),
        Err(error)
            if error.kind() == io::ErrorKind::ConnectionRefused
                || error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(SocketError::Io(error));
        }
    }
    let current = match stat_entry(locked) {
        Ok(identity) => identity,
        Err(SocketError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if current != original {
        return Err(SocketError::IdentityChanged(locked.path.clone()));
    }
    remove_if_identity(locked, current)
}

fn remove_if_identity(locked: &LockedParent, expected: EntryIdentity) -> Result<(), SocketError> {
    let current = match stat_entry(locked) {
        Ok(identity) => identity,
        Err(SocketError::Io(error)) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if current != expected {
        return Err(SocketError::IdentityChanged(locked.path.clone()));
    }
    fs::unlinkat(&locked.fd, locked.socket_name(), AtFlags::empty())?;
    Ok(())
}

fn is_private_socket(identity: EntryIdentity) -> bool {
    identity.file_type == FileType::Socket
        && identity.uid == geteuid().as_raw()
        && (identity.mode & 0o777) == PRIVATE_SOCKET_MODE
}

fn unsafe_path(path: &Path, reason: &'static str) -> SocketError {
    SocketError::UnsafePath {
        path: path.to_path_buf(),
        reason,
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;

    use super::{
        AncestorPolicy, DefaultRuntimeMetadata, Mode, ancestor_permissions_are_safe,
        default_runtime_dir_is_safe, prepare_default_runtime_dir, umask,
    };

    #[test]
    fn foreign_default_runtime_directory_owner_is_rejected() {
        let expected_uid = 1_000;
        let foreign = DefaultRuntimeMetadata {
            device: 11,
            inode: 22,
            uid: expected_uid + 1,
            mode: 0o700,
            is_directory: true,
        };

        assert!(!default_runtime_dir_is_safe(foreign, foreign, expected_uid));
    }

    #[test]
    fn newly_created_default_runtime_dir_is_repaired_after_restrictive_umask() {
        const CHILD_ENV: &str = "YAMS_SERVICE_UMASK_CHILD";
        if std::env::var_os(CHILD_ENV).is_none() {
            let output = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "socket::tests::newly_created_default_runtime_dir_is_repaired_after_restrictive_umask",
                    "--nocapture",
                    "--test-threads=1",
                ])
                .env(CHILD_ENV, "1")
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "umask child failed:\nstdout:\n{}\nstderr:\n{}",
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }

        let temp = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let runtime = temp.path().join("runtime");
        let _ = umask(Mode::from_raw_mode(0o777));

        let result = prepare_default_runtime_dir(&runtime);

        result.unwrap();
        let mode = std::fs::metadata(&runtime).unwrap().permissions().mode() & 0o7777;
        assert_eq!(mode, 0o700);
    }

    #[test]
    fn sticky_shared_ancestor_requires_root_or_effective_user_ownership() {
        let effective_uid = 1_000;

        assert!(ancestor_permissions_are_safe(
            0o1777,
            0,
            effective_uid,
            AncestorPolicy::AllowStickyShared,
        ));
        assert!(ancestor_permissions_are_safe(
            0o1777,
            effective_uid,
            effective_uid,
            AncestorPolicy::AllowStickyShared,
        ));
        assert!(!ancestor_permissions_are_safe(
            0o1777,
            effective_uid + 1,
            effective_uid,
            AncestorPolicy::AllowStickyShared,
        ));
    }
}
