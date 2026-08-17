use std::ffi::OsStr;
use std::fmt;
use std::fs;
use std::os::fd::{AsFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{self as rfs, AtFlags, FileType, FlockOperation, Mode, OFlags, Stat};
use rustix::io::Errno;
use rustix::process::geteuid;
use thiserror::Error;

const SLOT_NAMES: [&str; 2] = [".yams-model-load-0.lock", ".yams-model-load-1.lock"];
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const EXISTING_LOCK_FLAGS: OFlags = OFlags::RDWR
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
const CREATE_LOCK_FLAGS: OFlags = EXISTING_LOCK_FLAGS
    .union(OFlags::CREATE)
    .union(OFlags::EXCL);

/// Monotonic timing policy for acquiring one of the two model-load slots.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConstructionWait {
    /// Delay between nonblocking attempts.
    pub poll_interval: Duration,
    /// Elapsed wait before the caller receives one notice.
    pub notice_after: Duration,
    /// Hard bound on time spent waiting for a slot.
    pub timeout: Duration,
}

impl ConstructionWait {
    /// Creates an explicit wait policy.
    pub const fn new(poll_interval: Duration, notice_after: Duration, timeout: Duration) -> Self {
        Self {
            poll_interval,
            notice_after,
            timeout,
        }
    }
}

impl Default for ConstructionWait {
    fn default() -> Self {
        Self::new(
            Duration::from_millis(10),
            Duration::from_secs(10),
            Duration::from_secs(600),
        )
    }
}

/// A single progress notice emitted after ordinary contention becomes long-running.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConstructionNotice {
    /// Monotonic elapsed wait at the notice boundary.
    pub waited: Duration,
}

impl fmt::Display for ConstructionNotice {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "note: waiting for a model-construction slot ({:.0}s so far). Another process is loading the embedding model; an online first run downloads the model inside that slot.",
            self.waited.as_secs_f64()
        )
    }
}

/// A refusal or bounded-wait failure while acquiring a model-load slot.
#[derive(Debug, Error)]
pub enum ConstructionLockError {
    #[error("unsafe model-construction slot at {path}: {reason}")]
    Unsafe { path: PathBuf, reason: String },

    #[error("no model-construction slot after {timeout:?}: {paths:?} stayed held throughout")]
    Busy {
        paths: [PathBuf; 2],
        timeout: Duration,
    },

    #[error("model-construction slot binding changed at {path}")]
    Rebound { path: PathBuf },

    #[error("cannot provision the model-construction directory {path}: {reason}")]
    Unprovisionable { path: PathBuf, reason: String },
}

/// One held exclusive model-construction slot.
///
/// The persistent lock file remains on disk, while dropping the lease releases
/// the advisory `flock` and closes all pinned descriptors.
#[derive(Debug)]
pub struct ConstructionLease {
    directory: PinnedDirectory,
    lock_fd: OwnedFd,
    lock_identity: Identity,
    lock_path: PathBuf,
    slot: usize,
}

impl ConstructionLease {
    /// Acquires a slot with the production ten-minute bound and stderr notice.
    pub fn acquire(lock_dir: impl AsRef<Path>) -> Result<Self, ConstructionLockError> {
        Self::acquire_with_wait(lock_dir, ConstructionWait::default(), |notice| {
            eprintln!("{notice}");
        })
    }

    /// Acquires a slot with explicit monotonic timing and a one-shot notice sink.
    ///
    /// The lock directory is provisioned at most once, here at entry. Every
    /// later poll only opens what already exists, so a directory that
    /// disappears while this call waits fails closed instead of silently
    /// re-creating fresh slots outside the two-slot bound.
    pub fn acquire_with_wait(
        lock_dir: impl AsRef<Path>,
        wait: ConstructionWait,
        mut note: impl FnMut(ConstructionNotice),
    ) -> Result<Self, ConstructionLockError> {
        let lock_dir = absolute_path(lock_dir.as_ref())?;
        provision_lock_directory(&lock_dir)?;
        let started = Instant::now();
        let mut announced = false;
        loop {
            let directory = PinnedDirectory::open(&lock_dir)?;
            let slots = open_slots(&directory)?;
            let paths = slots.each_ref().map(|slot| slot.path.clone());
            for (slot, opened) in slots.into_iter().enumerate() {
                match rfs::flock(opened.fd.as_fd(), FlockOperation::NonBlockingLockExclusive) {
                    Ok(()) => {
                        if started.elapsed() >= wait.timeout {
                            let _ = rfs::flock(&opened.fd, FlockOperation::Unlock);
                            return Err(ConstructionLockError::Busy {
                                paths,
                                timeout: wait.timeout,
                            });
                        }
                        directory.revalidate()?;
                        opened.revalidate(&directory)?;
                        return Ok(Self {
                            directory,
                            lock_fd: opened.fd,
                            lock_identity: opened.identity,
                            lock_path: opened.path,
                            slot,
                        });
                    }
                    Err(error) if error == Errno::WOULDBLOCK || error == Errno::AGAIN => {}
                    Err(Errno::INTR) => {}
                    Err(error) => {
                        return Err(unsafe_lock(
                            opened.path,
                            format!("could not acquire exclusive lock: {error}"),
                        ));
                    }
                }
            }

            let elapsed = started.elapsed();
            if elapsed >= wait.timeout {
                return Err(ConstructionLockError::Busy {
                    paths,
                    timeout: wait.timeout,
                });
            }
            if !announced && elapsed >= wait.notice_after {
                announced = true;
                note(ConstructionNotice { waited: elapsed });
            }
            let remaining = wait.timeout.saturating_sub(elapsed);
            thread::sleep(wait.poll_interval.min(remaining));
        }
    }

    /// Returns the held zero-based slot number.
    pub const fn slot(&self) -> usize {
        self.slot
    }

    /// Returns the canonical persistent lock-file path.
    pub fn path(&self) -> &Path {
        &self.lock_path
    }

    /// Revalidates directory confinement plus descriptor and persistent-name identity.
    pub fn revalidate(&self) -> Result<(), ConstructionLockError> {
        self.directory.revalidate()?;
        let opened = rfs::fstat(&self.lock_fd).map_err(|error| {
            unsafe_lock(
                self.lock_path.clone(),
                format!("could not reinspect lock descriptor: {error}"),
            )
        })?;
        validate_lock_stat(&self.lock_path, &opened)?;
        if Identity::from_stat(&opened) != self.lock_identity {
            return Err(ConstructionLockError::Rebound {
                path: self.lock_path.clone(),
            });
        }
        verify_named_lock(
            &self.directory.fd,
            SLOT_NAMES[self.slot],
            &self.lock_path,
            self.lock_identity,
        )
    }
}

impl Drop for ConstructionLease {
    fn drop(&mut self) {
        let _ = rfs::flock(&self.lock_fd, FlockOperation::Unlock);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        }
    }
}

#[derive(Debug)]
struct PinnedDirectory {
    fd: OwnedFd,
    identity: Identity,
    requested_path: PathBuf,
    canonical_path: PathBuf,
}

impl PinnedDirectory {
    /// Opens an existing lock directory. This never creates anything: a
    /// directory that vanishes between polls must fail, not be re-provisioned.
    fn open(path: &Path) -> Result<Self, ConstructionLockError> {
        let requested_path = absolute_path(path)?;
        let canonical_path = fs::canonicalize(&requested_path).map_err(|error| {
            unsafe_lock(
                requested_path.clone(),
                format!("lock directory is unavailable: {error}"),
            )
        })?;
        let fd = rfs::open(&canonical_path, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
            unsafe_lock(
                canonical_path.clone(),
                format!("could not open lock directory without following links: {error}"),
            )
        })?;
        let stat = rfs::fstat(&fd).map_err(|error| {
            unsafe_lock(
                canonical_path.clone(),
                format!("could not inspect lock directory: {error}"),
            )
        })?;
        validate_directory_stat(&canonical_path, &stat)?;
        let directory = Self {
            fd,
            identity: Identity::from_stat(&stat),
            requested_path,
            canonical_path,
        };
        directory.revalidate()?;
        Ok(directory)
    }

    fn revalidate(&self) -> Result<(), ConstructionLockError> {
        let resolved =
            fs::canonicalize(&self.requested_path).map_err(|_| ConstructionLockError::Rebound {
                path: self.requested_path.clone(),
            })?;
        if resolved != self.canonical_path {
            return Err(ConstructionLockError::Rebound {
                path: self.requested_path.clone(),
            });
        }
        let opened = rfs::fstat(&self.fd).map_err(|error| {
            unsafe_lock(
                self.canonical_path.clone(),
                format!("could not reinspect lock directory descriptor: {error}"),
            )
        })?;
        validate_directory_stat(&self.canonical_path, &opened)?;
        if Identity::from_stat(&opened) != self.identity {
            return Err(ConstructionLockError::Rebound {
                path: self.canonical_path.clone(),
            });
        }
        let named =
            rfs::open(&self.canonical_path, DIRECTORY_FLAGS, Mode::empty()).map_err(|_| {
                ConstructionLockError::Rebound {
                    path: self.canonical_path.clone(),
                }
            })?;
        let named = rfs::fstat(&named).map_err(|_| ConstructionLockError::Rebound {
            path: self.canonical_path.clone(),
        })?;
        if Identity::from_stat(&named) != self.identity {
            return Err(ConstructionLockError::Rebound {
                path: self.canonical_path.clone(),
            });
        }
        Ok(())
    }
}

#[derive(Debug)]
struct OpenedSlot {
    fd: OwnedFd,
    identity: Identity,
    path: PathBuf,
    name: &'static str,
}

impl OpenedSlot {
    fn revalidate(&self, directory: &PinnedDirectory) -> Result<(), ConstructionLockError> {
        let stat = rfs::fstat(&self.fd).map_err(|error| {
            unsafe_lock(
                self.path.clone(),
                format!("could not reinspect lock descriptor: {error}"),
            )
        })?;
        validate_lock_stat(&self.path, &stat)?;
        if Identity::from_stat(&stat) != self.identity {
            return Err(ConstructionLockError::Rebound {
                path: self.path.clone(),
            });
        }
        verify_named_lock(&directory.fd, self.name, &self.path, self.identity)
    }
}

fn open_slots(directory: &PinnedDirectory) -> Result<[OpenedSlot; 2], ConstructionLockError> {
    Ok([
        open_slot(directory, SLOT_NAMES[0])?,
        open_slot(directory, SLOT_NAMES[1])?,
    ])
}

fn open_slot(
    directory: &PinnedDirectory,
    name: &'static str,
) -> Result<OpenedSlot, ConstructionLockError> {
    directory.revalidate()?;
    let path = directory.canonical_path.join(name);
    let (fd, created) = match rfs::openat(
        &directory.fd,
        name,
        CREATE_LOCK_FLAGS,
        Mode::RUSR | Mode::WUSR,
    ) {
        Ok(fd) => (fd, true),
        Err(Errno::EXIST) => {
            let fd = rfs::openat(&directory.fd, name, EXISTING_LOCK_FLAGS, Mode::empty()).map_err(
                |error| {
                    unsafe_lock(
                        path.clone(),
                        format!("could not open without following links: {error}"),
                    )
                },
            )?;
            (fd, false)
        }
        Err(error) => {
            return Err(unsafe_lock(
                path,
                format!("could not create without following links: {error}"),
            ));
        }
    };
    if created {
        rfs::fchmod(&fd, Mode::RUSR | Mode::WUSR).map_err(|error| {
            unsafe_lock(path.clone(), format!("could not set mode 0600: {error}"))
        })?;
    }
    let stat = rfs::fstat(&fd).map_err(|error| {
        unsafe_lock(
            path.clone(),
            format!("could not inspect descriptor: {error}"),
        )
    })?;
    validate_lock_stat(&path, &stat)?;
    let opened = OpenedSlot {
        fd,
        identity: Identity::from_stat(&stat),
        path,
        name,
    };
    opened.revalidate(directory)?;
    Ok(opened)
}

/// Provisions the resolved lock directory, and any missing ancestor below the
/// deepest existing one, as owner-private mode 0700 directories.
///
/// The upward search inspects candidate ancestors by pathname with
/// `AT_SYMLINK_NOFOLLOW` until it finds the deepest existing directory, the
/// attachment point. That is the only pathname ever opened; creation then
/// descends purely by descriptor, making each remaining component with
/// `mkdirat` and reopening it with `openat(O_NOFOLLOW | O_DIRECTORY)` relative
/// to the descriptor of the level above it. No level is re-resolved by pathname
/// once creation starts, so an ancestor that another process renames or
/// replaces mid-walk cannot redirect a later level.
///
/// The attachment point itself must be a directory owned by the effective user
/// that no other user can write, which keeps the whole chain out of
/// shared-writable locations. Existing components are only inspected — never
/// chmodded, replaced, or otherwise repaired.
fn provision_lock_directory(path: &Path) -> Result<(), ConstructionLockError> {
    match rfs::statat(rfs::CWD, path, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            return if FileType::from_raw_mode(stat.st_mode).is_dir() {
                Ok(())
            } else {
                Err(unsafe_lock(
                    path.to_path_buf(),
                    "lock path is not a directory (symlinks are never followed)",
                ))
            };
        }
        // ENOTDIR means some ancestor is not a directory; the walk below names
        // the offending component exactly.
        Err(Errno::NOENT | Errno::NOTDIR) => {}
        Err(error) => {
            return Err(unsafe_lock(
                path.to_path_buf(),
                format!("lock directory is unavailable: {error}"),
            ));
        }
    }

    let mut missing: Vec<&OsStr> = Vec::new();
    let mut attachment = path;
    let attachment_stat = loop {
        let (parent, name) = match (attachment.parent(), attachment.file_name()) {
            (Some(parent), Some(name)) => (parent, name),
            _ => {
                return Err(unprovisionable(
                    attachment.to_path_buf(),
                    "it has no parent directory",
                ));
            }
        };
        missing.push(name);
        attachment = parent;
        match rfs::statat(rfs::CWD, attachment, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) if FileType::from_raw_mode(stat.st_mode).is_dir() => break stat,
            Ok(_) => {
                return Err(unprovisionable(
                    attachment.to_path_buf(),
                    "it is not a directory (symlinks are never followed)",
                ));
            }
            Err(Errno::NOENT | Errno::NOTDIR) => {}
            Err(error) => {
                return Err(unprovisionable(
                    attachment.to_path_buf(),
                    format!("could not inspect it: {error}"),
                ));
            }
        }
    };

    let mut parent_fd = rfs::open(attachment, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
        unprovisionable(
            attachment.to_path_buf(),
            format!("could not open it without following links: {error}"),
        )
    })?;
    let opened = rfs::fstat(&parent_fd).map_err(|error| {
        unprovisionable(
            attachment.to_path_buf(),
            format!("could not inspect it: {error}"),
        )
    })?;
    validate_attachment_stat(attachment, &opened)?;
    if Identity::from_stat(&opened) != Identity::from_stat(&attachment_stat) {
        return Err(ConstructionLockError::Rebound {
            path: attachment.to_path_buf(),
        });
    }

    let mut parent_path = attachment.to_path_buf();
    let depth = missing.len();
    for (index, name) in missing.into_iter().rev().enumerate() {
        let child_path = parent_path.join(name);
        let is_lock_dir = index + 1 == depth;
        parent_fd = create_private_directory(&parent_fd, name, &child_path, is_lock_dir)?;
        parent_path = child_path;
    }
    Ok(())
}

/// Creates one owner-private directory relative to its pinned parent and
/// returns its descriptor, so the caller can descend without re-resolving any
/// pathname.
///
/// A concurrent creator winning the race (`EEXIST`) resolves to validating what
/// that process left behind, exactly as if the directory had already existed.
///
/// Failures that are merely errno-shaped — no space, read-only filesystem, no
/// permission, quota — are never safety verdicts, so they surface as
/// [`ConstructionLockError::Unprovisionable`] at every level, lock directory
/// included. Only a verdict about the lock directory's own safety, meaning a
/// symlink or vanished entry caught by `O_NOFOLLOW` or a rejected owner or
/// mode, keeps the [`ConstructionLockError::Unsafe`] wording.
fn create_private_directory(
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
    is_lock_dir: bool,
) -> Result<OwnedFd, ConstructionLockError> {
    let refuse = |reason: String| {
        if is_lock_dir {
            unsafe_lock(path.to_path_buf(), reason)
        } else {
            unprovisionable(path.to_path_buf(), reason)
        }
    };
    let unprovisionable_here = |reason: String| unprovisionable(path.to_path_buf(), reason);
    let created = match rfs::mkdirat(parent, name, Mode::RWXU) {
        Ok(()) => true,
        Err(Errno::EXIST) => false,
        Err(error) => {
            return Err(unprovisionable_here(format!(
                "could not create it: {error}"
            )));
        }
    };
    let fd = rfs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
        refuse(format!(
            "could not open it without following links: {error}"
        ))
    })?;
    if created {
        // mkdirat applies the process umask; the created directory is ours
        // alone, so restore the exact private mode through its descriptor.
        rfs::fchmod(&fd, Mode::RWXU)
            .map_err(|error| unprovisionable_here(format!("could not set mode 0700: {error}")))?;
    }
    let stat = rfs::fstat(&fd)
        .map_err(|error| unprovisionable_here(format!("could not inspect it: {error}")))?;
    let identity = Identity::from_stat(&stat);
    if is_lock_dir {
        validate_directory_stat(path, &stat)?;
    } else {
        validate_attachment_stat(path, &stat)?;
    }
    let named = rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|error| refuse(format!("could not inspect it by name: {error}")))?;
    if Identity::from_stat(&named) != identity {
        return Err(ConstructionLockError::Rebound {
            path: path.to_path_buf(),
        });
    }
    Ok(fd)
}

/// Applies the lock directory's privacy rules to a directory that the lock
/// directory is created under or through.
fn validate_attachment_stat(path: &Path, stat: &Stat) -> Result<(), ConstructionLockError> {
    if !FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Err(unprovisionable(
            path.to_path_buf(),
            "it is not a directory (symlinks are never followed)",
        ));
    }
    if owner_id(stat) != geteuid().as_raw() {
        return Err(unprovisionable(
            path.to_path_buf(),
            "it is not owned by the effective user",
        ));
    }
    if mode_bits(stat) & 0o022 != 0 {
        return Err(unprovisionable(
            path.to_path_buf(),
            format!("mode {:04o} permits another user to write", mode_bits(stat)),
        ));
    }
    Ok(())
}

fn validate_directory_stat(path: &Path, stat: &Stat) -> Result<(), ConstructionLockError> {
    if !FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Err(unsafe_lock(
            path.to_path_buf(),
            "lock path is not a directory",
        ));
    }
    if owner_id(stat) != geteuid().as_raw() {
        return Err(unsafe_lock(
            path.to_path_buf(),
            "lock directory is not owned by the effective user",
        ));
    }
    if mode_bits(stat) & 0o022 != 0 {
        return Err(unsafe_lock(
            path.to_path_buf(),
            format!(
                "lock directory mode {:04o} permits another user to write",
                mode_bits(stat)
            ),
        ));
    }
    Ok(())
}

fn validate_lock_stat(path: &Path, stat: &Stat) -> Result<(), ConstructionLockError> {
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(unsafe_lock(
            path.to_path_buf(),
            "lock is not a regular file",
        ));
    }
    if owner_id(stat) != geteuid().as_raw() {
        return Err(unsafe_lock(
            path.to_path_buf(),
            "lock is not owned by the effective user",
        ));
    }
    if stat.st_nlink != 1 {
        return Err(unsafe_lock(
            path.to_path_buf(),
            format!(
                "lock must have exactly one hard link, found {}",
                stat.st_nlink
            ),
        ));
    }
    if mode_bits(stat) != 0o600 {
        return Err(unsafe_lock(
            path.to_path_buf(),
            format!("lock mode must be 0600, found {:04o}", mode_bits(stat)),
        ));
    }
    if stat.st_size != 0 {
        return Err(unsafe_lock(path.to_path_buf(), "lock must be zero bytes"));
    }
    Ok(())
}

fn verify_named_lock(
    directory: &OwnedFd,
    name: &str,
    path: &Path,
    expected: Identity,
) -> Result<(), ConstructionLockError> {
    let named = match rfs::statat(directory, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => {
            return Err(ConstructionLockError::Rebound {
                path: path.to_path_buf(),
            });
        }
        Err(error) => {
            return Err(unsafe_lock(
                path.to_path_buf(),
                format!("could not inspect named lock: {error}"),
            ));
        }
    };
    validate_lock_stat(path, &named)?;
    if Identity::from_stat(&named) != expected {
        return Err(ConstructionLockError::Rebound {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, ConstructionLockError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|current| current.join(path))
        .map_err(|error| {
            unsafe_lock(
                path.to_path_buf(),
                format!("could not resolve path: {error}"),
            )
        })
}

#[allow(clippy::unnecessary_cast)]
fn owner_id(stat: &Stat) -> u32 {
    stat.st_uid as u32
}

fn mode_bits(stat: &Stat) -> u32 {
    stat.st_mode as u32 & 0o7777
}

fn unsafe_lock(path: PathBuf, reason: impl Into<String>) -> ConstructionLockError {
    ConstructionLockError::Unsafe {
        path,
        reason: reason.into(),
    }
}

fn unprovisionable(path: PathBuf, reason: impl Into<String>) -> ConstructionLockError {
    ConstructionLockError::Unprovisionable {
        path,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// Losing the creation race must resolve to validating the winner's
    /// directory, which is the same code path `EEXIST` takes.
    #[test]
    fn a_lost_creation_race_validates_the_winning_directory() {
        for is_lock_dir in [true, false] {
            let parent = tempfile::tempdir().unwrap();
            let path = parent.path().join("locks");
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            let parent_fd = rfs::open(parent.path(), DIRECTORY_FLAGS, Mode::empty()).unwrap();

            create_private_directory(&parent_fd, OsStr::new("locks"), &path, is_lock_dir).unwrap();

            let metadata = fs::symlink_metadata(&path).unwrap();
            assert!(metadata.is_dir());
            assert_eq!(metadata.permissions().mode() & 0o7777, 0o700);
        }
    }

    #[test]
    fn a_lost_creation_race_to_an_unsafe_directory_fails_closed() {
        for is_lock_dir in [true, false] {
            let parent = tempfile::tempdir().unwrap();
            let path = parent.path().join("locks");
            fs::create_dir(&path).unwrap();
            fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
            let parent_fd = rfs::open(parent.path(), DIRECTORY_FLAGS, Mode::empty()).unwrap();

            let error =
                create_private_directory(&parent_fd, OsStr::new("locks"), &path, is_lock_dir)
                    .unwrap_err();

            if is_lock_dir {
                assert!(matches!(error, ConstructionLockError::Unsafe { .. }));
            } else {
                assert!(matches!(
                    error,
                    ConstructionLockError::Unprovisionable { .. }
                ));
            }
            assert_eq!(
                fs::symlink_metadata(&path).unwrap().permissions().mode() & 0o7777,
                0o777,
                "a raced unsafe directory must never be repaired"
            );
        }
    }

    #[test]
    fn a_lost_creation_race_to_a_symlink_fails_closed() {
        for is_lock_dir in [true, false] {
            let parent = tempfile::tempdir().unwrap();
            let target = parent.path().join("target");
            fs::create_dir(&target).unwrap();
            let path = parent.path().join("locks");
            std::os::unix::fs::symlink(&target, &path).unwrap();
            let parent_fd = rfs::open(parent.path(), DIRECTORY_FLAGS, Mode::empty()).unwrap();

            let error =
                create_private_directory(&parent_fd, OsStr::new("locks"), &path, is_lock_dir)
                    .unwrap_err();

            assert!(matches!(
                error,
                ConstructionLockError::Unsafe { .. }
                    | ConstructionLockError::Unprovisionable { .. }
            ));
            assert!(fs::symlink_metadata(&path).unwrap().is_symlink());
            assert_eq!(fs::read_dir(&target).unwrap().count(), 0);
        }
    }
}
