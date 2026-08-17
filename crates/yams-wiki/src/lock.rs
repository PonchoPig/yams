use std::fmt;
use std::fs;
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{self as rfs, Access, AtFlags, FileType, FlockOperation, Mode, OFlags, Stat};
use rustix::io::Errno;
use rustix::process::getuid;
use thiserror::Error;

/// Persistent lock-file name shared by every wiki reader and writer.
pub const LOCK_NAME: &str = ".write.lock";
/// Default maximum time spent acquiring or safely rebinding the wiki lock.
pub const LOCK_TIMEOUT: Duration = Duration::from_secs(10);
const LOCK_POLL: Duration = Duration::from_millis(50);
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const EXISTING_LOCK_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
const CREATE_LOCK_FLAGS: OFlags = EXISTING_LOCK_FLAGS
    .union(OFlags::CREATE)
    .union(OFlags::EXCL);

/// Advisory lock mode for a wiki operation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LockMode {
    Shared,
    Exclusive,
}

impl fmt::Display for LockMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shared => formatter.write_str("shared"),
            Self::Exclusive => formatter.write_str("exclusive"),
        }
    }
}

/// A refusal to use the project lock.
#[derive(Debug, Error)]
pub enum LockError {
    #[error("wiki lock {path} stayed busy for {timeout:?} in {mode} mode")]
    Busy {
        path: PathBuf,
        mode: LockMode,
        timeout: Duration,
    },

    #[error("unsafe wiki lock at {path}: {reason}")]
    Unsafe { path: PathBuf, reason: String },
}

/// Why a corpus that cannot take writes may proceed without a lock file.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnisolatedReason {
    ReadOnlyFilesystem,
    UnwritableCorpus,
}

/// Evidence that neither the lock nor wiki pages can be written.
pub struct Unisolated {
    /// Canonical corpus path whose inability to take writes was verified.
    pub corpus: PathBuf,
    /// Evidence permitting an explicitly unisolated operation.
    pub reason: UnisolatedReason,
    corpus_fd: OwnedFd,
    corpus_identity: Identity,
    requested_path: PathBuf,
}

impl fmt::Debug for Unisolated {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Unisolated")
            .field("corpus", &self.corpus)
            .field("reason", &self.reason)
            .finish()
    }
}

impl PartialEq for Unisolated {
    fn eq(&self, other: &Self) -> bool {
        self.corpus == other.corpus && self.reason == other.reason
    }
}

impl Eq for Unisolated {}

impl Unisolated {
    pub(crate) fn corpus_fd(&self) -> BorrowedFd<'_> {
        self.corpus_fd.as_fd()
    }

    pub(crate) fn revalidate(&self) -> Result<(), LockError> {
        match revalidate_corpus(
            &self.requested_path,
            &self.corpus,
            &self.corpus_fd,
            self.corpus_identity,
        )? {
            Binding::Current => Ok(()),
            Binding::Rebound => Err(unsafe_lock(
                self.corpus.clone(),
                "unisolated wiki corpus identity changed during the read",
            )),
        }
    }
}

/// A held advisory lock tied to pinned corpus-directory and lock-file descriptors.
///
/// Acquisition verifies canonical names and descriptor identities before and
/// after `flock`. Advisory locking cannot stop a same-UID process with directory
/// write permission from unlinking or rebinding the persistent name afterward;
/// cooperating wiki processes must all acquire [`LOCK_NAME`].
#[derive(Debug)]
pub struct LockGuard {
    target: PinnedTarget,
    mode: LockMode,
}

impl LockGuard {
    /// Canonical corpus path bound to the pinned directory descriptor.
    pub fn corpus_path(&self) -> &Path {
        &self.target.corpus_path
    }

    /// Canonical persistent lock path bound to the held descriptor.
    pub fn lock_path(&self) -> &Path {
        &self.target.lock_path
    }

    /// Advisory mode held by this guard.
    pub const fn mode(&self) -> LockMode {
        self.mode
    }

    /// Borrow the pinned corpus directory for descriptor-relative wiki work.
    pub(crate) fn corpus_fd(&self) -> BorrowedFd<'_> {
        self.target.corpus_fd.as_fd()
    }

    /// Revalidate every pinned name and identity immediately before commit.
    ///
    /// A same-UID process with directory write permission can still replace a
    /// name after this check returns; callers must keep irreversible work as
    /// close to this boundary as possible.
    #[allow(
        dead_code,
        reason = "the Task 8/9 sibling mutation modules consume this boundary"
    )]
    pub(crate) fn revalidate_before_commit(&self) -> Result<(), LockError> {
        match revalidate_target(&self.target)? {
            Binding::Current => Ok(()),
            Binding::Rebound => Err(unsafe_lock(
                self.target.lock_path.clone(),
                "wiki corpus or lock identity changed before commit",
            )),
        }
    }
}

impl Drop for LockGuard {
    fn drop(&mut self) {
        let _pinned_corpus = self.corpus_fd();
        let _ = rfs::flock(&self.target.lock_fd, FlockOperation::Unlock);
    }
}

/// Either an isolated guard or explicit evidence that this corpus takes no writes.
#[derive(Debug)]
pub enum LockLease {
    Isolated(LockGuard),
    Unisolated(Unisolated),
}

/// Acquire a project lock using [`LOCK_TIMEOUT`].
pub fn acquire_lock(corpus: &Path, mode: LockMode) -> Result<LockLease, LockError> {
    acquire_lock_with_timeout(corpus, mode, LOCK_TIMEOUT)
}

/// Acquire a project lock within one monotonic deadline.
///
/// An existing safe lock is opened read-only, so a `0444` lock remains usable.
/// The lock file is persistent and is never unlinked on release.
pub fn acquire_lock_with_timeout(
    corpus: &Path,
    mode: LockMode,
    timeout: Duration,
) -> Result<LockLease, LockError> {
    let mut runtime = SystemRuntime::new();
    acquire_with_runtime(corpus, mode, timeout, &mut runtime)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    // rustix exposes different Stat field widths across supported targets.
    #[allow(clippy::unnecessary_cast)]
    fn from_stat(stat: &Stat) -> Self {
        Self {
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        }
    }
}

#[derive(Debug)]
struct PinnedTarget {
    corpus_fd: OwnedFd,
    corpus_identity: Identity,
    requested_path: PathBuf,
    corpus_path: PathBuf,
    lock_fd: OwnedFd,
    lock_identity: Identity,
    lock_path: PathBuf,
}

#[derive(Debug)]
enum OpenTarget {
    Isolated(PinnedTarget),
    Unisolated(Unisolated),
    Rebound(PathBuf),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Binding {
    Current,
    Rebound,
}

trait Runtime {
    fn now(&mut self) -> Duration;

    fn backoff(&mut self, duration: Duration) {
        thread::sleep(duration);
    }

    fn flock(&mut self, fd: BorrowedFd<'_>, operation: FlockOperation) -> Result<(), Errno> {
        rfs::flock(fd, operation)
    }

    fn open_lock(
        &mut self,
        directory: BorrowedFd<'_>,
        flags: OFlags,
        mode: Mode,
    ) -> Result<OwnedFd, Errno> {
        rfs::openat(directory, LOCK_NAME, flags, mode)
    }

    fn after_corpus_opened(&mut self, _requested: &Path, _canonical: &Path) {}

    fn after_lock_opened(&mut self, _lock_path: &Path) {}

    fn after_flock_succeeded(&mut self, _lock_path: &Path) {}
}

struct SystemRuntime {
    started: Instant,
}

impl SystemRuntime {
    fn new() -> Self {
        Self {
            started: Instant::now(),
        }
    }
}

impl Runtime for SystemRuntime {
    fn now(&mut self) -> Duration {
        self.started.elapsed()
    }
}

fn acquire_with_runtime(
    corpus: &Path,
    mode: LockMode,
    timeout: Duration,
    runtime: &mut impl Runtime,
) -> Result<LockLease, LockError> {
    let started = runtime.now();
    let deadline = started.checked_add(timeout).unwrap_or(Duration::MAX);

    'acquire: loop {
        let target = match open_target(corpus, runtime)? {
            OpenTarget::Isolated(target) => target,
            OpenTarget::Unisolated(unisolated) => {
                return Ok(LockLease::Unisolated(unisolated));
            }
            OpenTarget::Rebound(lock_path) => {
                wait_to_retry(runtime, deadline, lock_path, mode, timeout)?;
                continue;
            }
        };
        let operation = match mode {
            LockMode::Shared => FlockOperation::NonBlockingLockShared,
            LockMode::Exclusive => FlockOperation::NonBlockingLockExclusive,
        };

        loop {
            match runtime.flock(target.lock_fd.as_fd(), operation) {
                Ok(()) => {
                    runtime.after_flock_succeeded(&target.lock_path);
                    if runtime.now() >= deadline {
                        unlock(runtime, &target)?;
                        return Err(busy(target.lock_path, mode, timeout));
                    }
                    match revalidate_target(&target)? {
                        Binding::Current => {
                            if runtime.now() >= deadline {
                                unlock(runtime, &target)?;
                                return Err(busy(target.lock_path, mode, timeout));
                            }
                            return Ok(LockLease::Isolated(LockGuard { target, mode }));
                        }
                        Binding::Rebound => {
                            unlock(runtime, &target)?;
                            let lock_path = target.lock_path.clone();
                            drop(target);
                            wait_to_retry(runtime, deadline, lock_path, mode, timeout)?;
                            continue 'acquire;
                        }
                    }
                }
                Err(error) if error == Errno::WOULDBLOCK || error == Errno::AGAIN => {
                    wait_to_retry(runtime, deadline, target.lock_path.clone(), mode, timeout)?;
                    match revalidate_target(&target)? {
                        Binding::Current => {
                            if runtime.now() >= deadline {
                                return Err(busy(target.lock_path, mode, timeout));
                            }
                        }
                        Binding::Rebound => {
                            let lock_path = target.lock_path.clone();
                            // A failed nonblocking flock acquired no lock. Closing
                            // this descriptor is the stale-waiter release boundary.
                            drop(target);
                            if runtime.now() >= deadline {
                                return Err(busy(lock_path, mode, timeout));
                            }
                            continue 'acquire;
                        }
                    }
                }
                Err(Errno::INTR) => {
                    if runtime.now() >= deadline {
                        return Err(busy(target.lock_path, mode, timeout));
                    }
                }
                Err(error) => {
                    return Err(unsafe_lock(
                        target.lock_path,
                        format!("could not acquire {mode} lock: {error}"),
                    ));
                }
            }
        }
    }
}

fn open_target(corpus: &Path, runtime: &mut impl Runtime) -> Result<OpenTarget, LockError> {
    let requested_path = absolute_path(corpus)?;
    let corpus_path = fs::canonicalize(&requested_path).map_err(|error| {
        unsafe_lock(
            corpus.to_path_buf(),
            format!("no memory directory at {}: {error}", corpus.display()),
        )
    })?;
    let corpus_fd = rfs::open(&corpus_path, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
        unsafe_lock(
            corpus.to_path_buf(),
            format!("no memory directory at {}: {error}", corpus.display()),
        )
    })?;
    let corpus_stat = rfs::fstat(&corpus_fd).map_err(|error| {
        unsafe_lock(
            corpus.to_path_buf(),
            format!("could not inspect memory directory: {error}"),
        )
    })?;
    if !FileType::from_raw_mode(corpus_stat.st_mode).is_dir() {
        return Err(unsafe_lock(
            corpus.to_path_buf(),
            "memory corpus is not a directory",
        ));
    }
    let corpus_identity = Identity::from_stat(&corpus_stat);
    runtime.after_corpus_opened(&requested_path, &corpus_path);
    if revalidate_corpus(&requested_path, &corpus_path, &corpus_fd, corpus_identity)?
        == Binding::Rebound
    {
        return Ok(OpenTarget::Rebound(corpus_path.join(LOCK_NAME)));
    }

    let lock_path = corpus_path.join(LOCK_NAME);
    let (lock_fd, created) = match open_lock_file(runtime, corpus_fd.as_fd(), &lock_path) {
        Ok(opened) => opened,
        Err(OpenLockError::Unisolated(reason)) => {
            if revalidate_corpus(&requested_path, &corpus_path, &corpus_fd, corpus_identity)?
                == Binding::Rebound
            {
                return Ok(OpenTarget::Rebound(lock_path));
            }
            return Ok(OpenTarget::Unisolated(Unisolated {
                corpus: corpus_path,
                reason,
                corpus_fd,
                corpus_identity,
                requested_path,
            }));
        }
        Err(OpenLockError::Unsafe(reason)) => return Err(unsafe_lock(lock_path, reason)),
        Err(OpenLockError::Rebound) => return Ok(OpenTarget::Rebound(lock_path)),
    };
    if created {
        rfs::fchmod(&lock_fd, Mode::RUSR | Mode::WUSR).map_err(|error| {
            unsafe_lock(
                lock_path.clone(),
                format!("could not set new lock permissions to 0600: {error}"),
            )
        })?;
    }
    runtime.after_lock_opened(&lock_path);
    let lock_stat = validate_lock_descriptor(&lock_fd, &lock_path)?;
    if created && mode_bits(&lock_stat) != 0o600 {
        return Err(unsafe_lock(
            lock_path,
            format!(
                "new lock mode must be 0600, found {:04o}",
                mode_bits(&lock_stat)
            ),
        ));
    }
    let lock_identity = Identity::from_stat(&lock_stat);
    if verify_named_lock(&corpus_fd, &lock_path, lock_identity)? == Binding::Rebound {
        return Ok(OpenTarget::Rebound(lock_path));
    }

    Ok(OpenTarget::Isolated(PinnedTarget {
        corpus_fd,
        corpus_identity,
        requested_path,
        corpus_path,
        lock_fd,
        lock_identity,
        lock_path,
    }))
}

#[derive(Debug)]
enum OpenLockError {
    Unisolated(UnisolatedReason),
    Unsafe(String),
    Rebound,
}

fn open_lock_file(
    runtime: &mut impl Runtime,
    corpus_fd: BorrowedFd<'_>,
    lock_path: &Path,
) -> Result<(OwnedFd, bool), OpenLockError> {
    match runtime.open_lock(corpus_fd, CREATE_LOCK_FLAGS, Mode::RUSR | Mode::WUSR) {
        Ok(fd) => Ok((fd, true)),
        Err(Errno::EXIST) => {
            match runtime.open_lock(corpus_fd, EXISTING_LOCK_FLAGS, Mode::empty()) {
                Ok(fd) => Ok((fd, false)),
                Err(Errno::NOENT) => Err(OpenLockError::Rebound),
                Err(error) => classify_lock_open_error(corpus_fd, lock_path, error),
            }
        }
        Err(error) => classify_lock_open_error(corpus_fd, lock_path, error),
    }
}

fn wait_to_retry(
    runtime: &mut impl Runtime,
    deadline: Duration,
    lock_path: PathBuf,
    mode: LockMode,
    timeout: Duration,
) -> Result<(), LockError> {
    let now = runtime.now();
    if now >= deadline {
        return Err(busy(lock_path, mode, timeout));
    }
    runtime.backoff(LOCK_POLL.min(deadline - now));
    if runtime.now() >= deadline {
        return Err(busy(lock_path, mode, timeout));
    }
    Ok(())
}

fn classify_lock_open_error(
    corpus_fd: BorrowedFd<'_>,
    _lock_path: &Path,
    error: Errno,
) -> Result<(OwnedFd, bool), OpenLockError> {
    if error == Errno::ROFS && corpus_takes_no_writes(corpus_fd) {
        return Err(OpenLockError::Unisolated(
            UnisolatedReason::ReadOnlyFilesystem,
        ));
    }
    if (error == Errno::ACCESS || error == Errno::PERM) && corpus_takes_no_writes(corpus_fd) {
        return Err(OpenLockError::Unisolated(
            UnisolatedReason::UnwritableCorpus,
        ));
    }
    if error == Errno::NOENT {
        return Err(OpenLockError::Rebound);
    }
    Err(OpenLockError::Unsafe(format!(
        "could not open without following links: {error}"
    )))
}

fn corpus_takes_no_writes(corpus_fd: BorrowedFd<'_>) -> bool {
    if !matches!(
        rfs::statat(corpus_fd, LOCK_NAME, AtFlags::SYMLINK_NOFOLLOW),
        Err(Errno::NOENT)
    ) {
        return false;
    }
    rfs::accessat(corpus_fd, "pages", Access::WRITE_OK, AtFlags::EACCESS).is_err()
}

fn validate_lock_descriptor(fd: &OwnedFd, path: &Path) -> Result<Stat, LockError> {
    let stat = rfs::fstat(fd).map_err(|error| {
        unsafe_lock(
            path.to_path_buf(),
            format!("could not inspect lock: {error}"),
        )
    })?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(unsafe_lock(
            path.to_path_buf(),
            "lock is not a regular file",
        ));
    }
    let owner = stat.st_uid as u32;
    let real_uid = getuid().as_raw();
    if owner != real_uid && owner != 0 {
        return Err(unsafe_lock(
            path.to_path_buf(),
            format!("lock is owned by uid {owner}, expected real uid {real_uid} or root"),
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
    Ok(stat)
}

fn verify_named_lock(
    corpus_fd: &OwnedFd,
    path: &Path,
    identity: Identity,
) -> Result<Binding, LockError> {
    let named = match rfs::statat(corpus_fd, LOCK_NAME, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => stat,
        Err(Errno::NOENT) => return Ok(Binding::Rebound),
        Err(error) => {
            return Err(unsafe_lock(
                path.to_path_buf(),
                format!("could not inspect named lock: {error}"),
            ));
        }
    };
    if Identity::from_stat(&named) != identity {
        return Ok(Binding::Rebound);
    }
    Ok(Binding::Current)
}

fn revalidate_target(target: &PinnedTarget) -> Result<Binding, LockError> {
    if revalidate_corpus(
        &target.requested_path,
        &target.corpus_path,
        &target.corpus_fd,
        target.corpus_identity,
    )? == Binding::Rebound
    {
        return Ok(Binding::Rebound);
    }
    let stat = validate_lock_descriptor(&target.lock_fd, &target.lock_path)?;
    if Identity::from_stat(&stat) != target.lock_identity {
        return Ok(Binding::Rebound);
    }
    verify_named_lock(&target.corpus_fd, &target.lock_path, target.lock_identity)
}

fn revalidate_corpus(
    requested_path: &Path,
    corpus_path: &Path,
    corpus_fd: &OwnedFd,
    identity: Identity,
) -> Result<Binding, LockError> {
    let resolved = match fs::canonicalize(requested_path) {
        Ok(path) => path,
        Err(_) => return Ok(Binding::Rebound),
    };
    if resolved != corpus_path {
        return Ok(Binding::Rebound);
    }
    let pinned = rfs::fstat(corpus_fd).map_err(|error| {
        unsafe_lock(
            corpus_path.to_path_buf(),
            format!("could not reinspect pinned corpus directory: {error}"),
        )
    })?;
    if !FileType::from_raw_mode(pinned.st_mode).is_dir() || Identity::from_stat(&pinned) != identity
    {
        return Ok(Binding::Rebound);
    }
    let named = match rfs::open(corpus_path, DIRECTORY_FLAGS, Mode::empty()) {
        Ok(fd) => fd,
        Err(_) => return Ok(Binding::Rebound),
    };
    let named_stat = match rfs::fstat(&named) {
        Ok(stat) => stat,
        Err(_) => return Ok(Binding::Rebound),
    };
    if Identity::from_stat(&named_stat) != identity {
        return Ok(Binding::Rebound);
    }
    Ok(Binding::Current)
}

fn unlock(runtime: &mut impl Runtime, target: &PinnedTarget) -> Result<(), LockError> {
    runtime
        .flock(target.lock_fd.as_fd(), FlockOperation::Unlock)
        .map_err(|error| {
            unsafe_lock(
                target.lock_path.clone(),
                format!("could not release rebound lock before reopening: {error}"),
            )
        })
}

fn absolute_path(path: &Path) -> Result<PathBuf, LockError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .map_err(|error| {
            unsafe_lock(
                path.to_path_buf(),
                format!("could not resolve path: {error}"),
            )
        })
}

fn mode_bits(stat: &Stat) -> u32 {
    stat.st_mode as u32 & 0o7777
}

fn busy(path: PathBuf, mode: LockMode, timeout: Duration) -> LockError {
    LockError::Busy {
        path,
        mode,
        timeout,
    }
}

fn unsafe_lock(path: PathBuf, reason: impl Into<String>) -> LockError {
    LockError::Unsafe {
        path,
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs::{self, Permissions};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};

    use rustix::io::{FdFlags, fcntl_getfd};
    use tempfile::{TempDir, tempdir};

    use super::*;

    enum OpenBehavior {
        Normal,
        Errors(VecDeque<Errno>),
        Churn {
            calls: usize,
            step: Duration,
            panic_after: usize,
        },
    }

    type CorpusHook = Box<dyn FnMut(&Path, &Path)>;
    type LockHook = Box<dyn FnMut(&Path)>;

    struct TestRuntime {
        now: Duration,
        now_script: VecDeque<Duration>,
        backoffs: Vec<Duration>,
        flock_operations: Vec<FlockOperation>,
        force_busy: bool,
        flock_error: Option<Errno>,
        advance_after_flock: Option<Duration>,
        open_behavior: OpenBehavior,
        before_open_lock: Option<Box<dyn FnMut()>>,
        after_backoff: Option<Box<dyn FnMut()>>,
        after_corpus_opened: Option<CorpusHook>,
        after_lock_opened: Option<LockHook>,
        after_flock_succeeded: Option<LockHook>,
    }

    impl Default for TestRuntime {
        fn default() -> Self {
            Self {
                now: Duration::ZERO,
                now_script: VecDeque::new(),
                backoffs: Vec::new(),
                flock_operations: Vec::new(),
                force_busy: false,
                flock_error: None,
                advance_after_flock: None,
                open_behavior: OpenBehavior::Normal,
                before_open_lock: None,
                after_backoff: None,
                after_corpus_opened: None,
                after_lock_opened: None,
                after_flock_succeeded: None,
            }
        }
    }

    impl Runtime for TestRuntime {
        fn now(&mut self) -> Duration {
            if let Some(now) = self.now_script.pop_front() {
                self.now = now;
            }
            self.now
        }

        fn backoff(&mut self, duration: Duration) {
            self.backoffs.push(duration);
            self.now = self.now.saturating_add(duration);
            if let Some(mut hook) = self.after_backoff.take() {
                hook();
            }
        }

        fn flock(&mut self, fd: BorrowedFd<'_>, operation: FlockOperation) -> Result<(), Errno> {
            self.flock_operations.push(operation);
            if operation == FlockOperation::Unlock {
                return rfs::flock(fd, operation);
            }
            if let Some(error) = self.flock_error.take() {
                return Err(error);
            }
            if self.force_busy {
                return Err(Errno::WOULDBLOCK);
            }
            rfs::flock(fd, operation)
        }

        fn open_lock(
            &mut self,
            directory: BorrowedFd<'_>,
            flags: OFlags,
            mode: Mode,
        ) -> Result<OwnedFd, Errno> {
            if let Some(mut hook) = self.before_open_lock.take() {
                hook();
            }
            match &mut self.open_behavior {
                OpenBehavior::Normal => rfs::openat(directory, LOCK_NAME, flags, mode),
                OpenBehavior::Errors(errors) => match errors.pop_front() {
                    Some(error) => Err(error),
                    None => rfs::openat(directory, LOCK_NAME, flags, mode),
                },
                OpenBehavior::Churn {
                    calls,
                    step,
                    panic_after,
                } => {
                    *calls += 1;
                    assert!(
                        *calls <= *panic_after,
                        "lock-name churn exceeded its original deadline"
                    );
                    self.now = self.now.saturating_add(*step);
                    if flags.contains(OFlags::EXCL) {
                        Err(Errno::EXIST)
                    } else {
                        Err(Errno::NOENT)
                    }
                }
            }
        }

        fn after_corpus_opened(&mut self, requested: &Path, canonical: &Path) {
            if let Some(mut hook) = self.after_corpus_opened.take() {
                hook(requested, canonical);
            }
        }

        fn after_lock_opened(&mut self, lock_path: &Path) {
            if let Some(mut hook) = self.after_lock_opened.take() {
                hook(lock_path);
            }
        }

        fn after_flock_succeeded(&mut self, lock_path: &Path) {
            if let Some(now) = self.advance_after_flock.take() {
                self.now = now;
            }
            if let Some(mut hook) = self.after_flock_succeeded.take() {
                hook(lock_path);
            }
        }
    }

    fn writable_corpus() -> (TempDir, PathBuf) {
        let temporary = tempdir().unwrap();
        let corpus = temporary.path().join("memory");
        fs::create_dir(&corpus).unwrap();
        fs::create_dir(corpus.join("pages")).unwrap();
        (temporary, corpus)
    }

    fn isolated(lease: LockLease) -> LockGuard {
        match lease {
            LockLease::Isolated(guard) => guard,
            LockLease::Unisolated(unisolated) => {
                panic!("expected isolated lease, got {unisolated:?}")
            }
        }
    }

    fn assert_guard_is_bound_to_names(guard: &LockGuard) {
        let corpus = rfs::fstat(guard.corpus_fd()).unwrap();
        let named_corpus = rfs::stat(guard.corpus_path()).unwrap();
        assert_eq!(
            Identity::from_stat(&corpus),
            Identity::from_stat(&named_corpus)
        );

        let lock = rfs::fstat(&guard.target.lock_fd).unwrap();
        let named_lock = rfs::stat(guard.lock_path()).unwrap();
        assert_eq!(Identity::from_stat(&lock), Identity::from_stat(&named_lock));
    }

    fn replace_lock(path: &Path, stale_name: &str) {
        fs::rename(path, path.with_file_name(stale_name)).unwrap();
        fs::write(path, b"").unwrap();
        fs::set_permissions(path, Permissions::from_mode(0o600)).unwrap();
    }

    fn assert_path_identity_is_not_open(path: &Path) {
        let expected = fs::metadata(path).unwrap();
        let descriptor_directory = if Path::new("/dev/fd").is_dir() {
            Path::new("/dev/fd")
        } else {
            Path::new("/proc/self/fd")
        };
        for entry in fs::read_dir(descriptor_directory).unwrap() {
            let entry = entry.unwrap();
            let Ok(found) = fs::metadata(entry.path()) else {
                continue;
            };
            assert_ne!(
                (found.dev(), found.ino()),
                (expected.dev(), expected.ino()),
                "descriptor for {} remained open",
                path.display()
            );
        }
    }

    #[test]
    fn guard_exposes_its_pinned_corpus_fd_and_revalidates_current_names() {
        let (_temporary, corpus) = writable_corpus();
        let guard = isolated(acquire_lock(&corpus, LockMode::Exclusive).unwrap());

        assert!(
            fcntl_getfd(guard.corpus_fd())
                .unwrap()
                .contains(FdFlags::CLOEXEC)
        );
        guard.revalidate_before_commit().unwrap();
    }

    #[test]
    fn guard_revalidation_refuses_a_detached_corpus_tree() {
        let (_temporary, corpus) = writable_corpus();
        let guard = isolated(acquire_lock(&corpus, LockMode::Exclusive).unwrap());
        let expected_path = guard.lock_path().to_path_buf();
        let detached = corpus.with_file_name("memory-detached");
        fs::rename(&corpus, &detached).unwrap();
        fs::create_dir(&corpus).unwrap();
        fs::create_dir(corpus.join("pages")).unwrap();

        assert!(matches!(
            guard.revalidate_before_commit(),
            Err(LockError::Unsafe { path, .. }) if path == expected_path
        ));
    }

    #[test]
    fn guard_revalidation_refuses_a_rebound_requested_root() {
        let temporary = tempdir().unwrap();
        let first = temporary.path().join("first");
        let second = temporary.path().join("second");
        fs::create_dir(&first).unwrap();
        fs::create_dir(first.join("pages")).unwrap();
        fs::create_dir(&second).unwrap();
        fs::create_dir(second.join("pages")).unwrap();
        let requested = temporary.path().join("memory");
        symlink(&first, &requested).unwrap();
        let guard = isolated(acquire_lock(&requested, LockMode::Shared).unwrap());
        let expected_path = guard.lock_path().to_path_buf();
        fs::remove_file(&requested).unwrap();
        symlink(&second, &requested).unwrap();

        assert!(matches!(
            guard.revalidate_before_commit(),
            Err(LockError::Unsafe { path, .. }) if path == expected_path
        ));
    }

    #[test]
    fn guard_revalidation_refuses_a_replaced_named_lock() {
        let (_temporary, corpus) = writable_corpus();
        let guard = isolated(acquire_lock(&corpus, LockMode::Exclusive).unwrap());
        let expected_path = guard.lock_path().to_path_buf();
        replace_lock(&expected_path, ".write.lock-detached");

        assert!(matches!(
            guard.revalidate_before_commit(),
            Err(LockError::Unsafe { path, .. }) if path == expected_path
        ));
    }

    #[test]
    fn read_only_filesystem_is_an_explicit_unisolated_lease() {
        let (_temporary, corpus) = writable_corpus();
        fs::remove_dir(corpus.join("pages")).unwrap();
        let mut runtime = TestRuntime {
            open_behavior: OpenBehavior::Errors([Errno::ROFS].into()),
            ..TestRuntime::default()
        };

        let lease = acquire_with_runtime(
            &corpus,
            LockMode::Shared,
            Duration::from_secs(1),
            &mut runtime,
        )
        .unwrap();

        assert!(matches!(
            lease,
            LockLease::Unisolated(Unisolated {
                corpus: ref found,
                reason: UnisolatedReason::ReadOnlyFilesystem,
                ..
            }) if found == &corpus.canonicalize().unwrap()
        ));
        assert!(!corpus.join(LOCK_NAME).exists());
    }

    #[test]
    fn read_only_error_with_writable_pages_is_unsafe() {
        let (_temporary, corpus) = writable_corpus();
        let mut runtime = TestRuntime {
            open_behavior: OpenBehavior::Errors([Errno::ROFS].into()),
            ..TestRuntime::default()
        };

        let error = acquire_with_runtime(
            &corpus,
            LockMode::Shared,
            Duration::from_secs(1),
            &mut runtime,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            LockError::Unsafe { path, .. }
                if path == corpus.canonicalize().unwrap().join(LOCK_NAME)
        ));
        assert!(!corpus.join(LOCK_NAME).exists());
    }

    #[test]
    fn read_only_evidence_from_a_rebound_directory_never_degrades_the_new_tree() {
        let (_temporary, corpus) = writable_corpus();
        fs::remove_dir(corpus.join("pages")).unwrap();
        let stale = corpus.with_file_name("memory-stale");
        let corpus_for_hook = corpus.clone();
        let stale_for_hook = stale.clone();
        let mut runtime = TestRuntime {
            open_behavior: OpenBehavior::Errors([Errno::ROFS].into()),
            before_open_lock: Some(Box::new(move || {
                fs::rename(&corpus_for_hook, &stale_for_hook).unwrap();
                fs::create_dir(&corpus_for_hook).unwrap();
                fs::create_dir(corpus_for_hook.join("pages")).unwrap();
            })),
            ..TestRuntime::default()
        };

        let guard = isolated(
            acquire_with_runtime(
                &corpus,
                LockMode::Exclusive,
                Duration::from_secs(1),
                &mut runtime,
            )
            .unwrap(),
        );

        assert_guard_is_bound_to_names(&guard);
        assert!(!stale.join(LOCK_NAME).exists());
        assert!(corpus.join(LOCK_NAME).is_file());
    }

    #[test]
    fn final_backoff_is_shortened_to_the_original_deadline() {
        let (_temporary, corpus) = writable_corpus();
        let timeout = Duration::from_millis(125);
        let mut runtime = TestRuntime {
            force_busy: true,
            ..TestRuntime::default()
        };

        let error =
            acquire_with_runtime(&corpus, LockMode::Exclusive, timeout, &mut runtime).unwrap_err();

        assert!(matches!(
            error,
            LockError::Busy {
                path,
                mode: LockMode::Exclusive,
                timeout: found,
            } if path == corpus.canonicalize().unwrap().join(LOCK_NAME) && found == timeout
        ));
        assert_eq!(
            runtime.backoffs,
            [
                Duration::from_millis(50),
                Duration::from_millis(50),
                Duration::from_millis(25),
            ]
        );
        assert_eq!(runtime.now, timeout);
    }

    #[test]
    fn a_success_at_the_exact_deadline_is_busy_and_unlocked() {
        let (_temporary, corpus) = writable_corpus();
        let timeout = Duration::from_millis(100);
        let mut runtime = TestRuntime {
            advance_after_flock: Some(timeout),
            ..TestRuntime::default()
        };

        let error =
            acquire_with_runtime(&corpus, LockMode::Exclusive, timeout, &mut runtime).unwrap_err();

        assert!(matches!(error, LockError::Busy { timeout: found, .. } if found == timeout));
        assert_eq!(
            runtime.flock_operations,
            [
                FlockOperation::NonBlockingLockExclusive,
                FlockOperation::Unlock,
            ]
        );
        drop(isolated(
            acquire_lock_with_timeout(&corpus, LockMode::Exclusive, Duration::from_secs(1))
                .unwrap(),
        ));
    }

    #[test]
    fn validation_that_reaches_the_deadline_is_busy_and_unlocked() {
        let (_temporary, corpus) = writable_corpus();
        let timeout = Duration::from_millis(100);
        let mut runtime = TestRuntime {
            now_script: [
                Duration::ZERO,
                Duration::from_millis(99),
                Duration::from_millis(100),
            ]
            .into(),
            ..TestRuntime::default()
        };

        let error =
            acquire_with_runtime(&corpus, LockMode::Shared, timeout, &mut runtime).unwrap_err();

        assert!(matches!(error, LockError::Busy { timeout: found, .. } if found == timeout));
        assert_eq!(
            runtime.flock_operations,
            [
                FlockOperation::NonBlockingLockShared,
                FlockOperation::Unlock,
            ]
        );
    }

    #[test]
    fn a_waiter_never_retries_after_revalidation_reaches_the_deadline() {
        let (_temporary, corpus) = writable_corpus();
        let timeout = Duration::from_millis(100);
        let mut runtime = TestRuntime {
            now_script: [
                Duration::ZERO,
                Duration::ZERO,
                Duration::from_millis(99),
                timeout,
            ]
            .into(),
            force_busy: true,
            ..TestRuntime::default()
        };

        let error =
            acquire_with_runtime(&corpus, LockMode::Shared, timeout, &mut runtime).unwrap_err();

        assert!(matches!(error, LockError::Busy { timeout: found, .. } if found == timeout));
        assert_eq!(runtime.backoffs, [Duration::from_millis(50)]);
        assert_eq!(
            runtime.flock_operations,
            [FlockOperation::NonBlockingLockShared]
        );
    }

    #[test]
    fn an_unexpected_flock_error_is_typed_and_closes_open_descriptors() {
        let (_temporary, corpus) = writable_corpus();
        let mut runtime = TestRuntime {
            flock_error: Some(Errno::IO),
            ..TestRuntime::default()
        };

        let error = acquire_with_runtime(
            &corpus,
            LockMode::Shared,
            Duration::from_secs(1),
            &mut runtime,
        )
        .unwrap_err();

        assert!(matches!(
            error,
            LockError::Unsafe { path, reason }
                if path == corpus.canonicalize().unwrap().join(LOCK_NAME)
                    && reason.contains("could not acquire shared lock")
        ));
        assert_path_identity_is_not_open(&corpus);
        assert_path_identity_is_not_open(&corpus.join(LOCK_NAME));
    }

    #[test]
    fn replacing_the_corpus_leaf_after_open_restarts_on_the_named_directory() {
        let (_temporary, corpus) = writable_corpus();
        let stale = corpus.with_file_name("memory-stale");
        let stale_for_hook = stale.clone();
        let mut runtime = TestRuntime {
            after_corpus_opened: Some(Box::new(move |_requested, canonical| {
                fs::rename(canonical, &stale_for_hook).unwrap();
                fs::create_dir(canonical).unwrap();
                fs::create_dir(canonical.join("pages")).unwrap();
            })),
            ..TestRuntime::default()
        };

        let guard = isolated(
            acquire_with_runtime(
                &corpus,
                LockMode::Exclusive,
                Duration::from_secs(1),
                &mut runtime,
            )
            .unwrap(),
        );

        assert_guard_is_bound_to_names(&guard);
        assert!(!stale.join(LOCK_NAME).exists());
        assert!(corpus.join(LOCK_NAME).is_file());
    }

    #[test]
    fn replacing_an_ancestor_after_open_restarts_within_the_new_tree() {
        let temporary = tempdir().unwrap();
        let ancestor = temporary.path().join("ancestor");
        let corpus = ancestor.join("memory");
        fs::create_dir_all(corpus.join("pages")).unwrap();
        let stale_ancestor = temporary.path().join("ancestor-stale");
        let ancestor_for_hook = ancestor.clone();
        let stale_for_hook = stale_ancestor.clone();
        let mut runtime = TestRuntime {
            after_corpus_opened: Some(Box::new(move |_requested, _canonical| {
                fs::rename(&ancestor_for_hook, &stale_for_hook).unwrap();
                fs::create_dir(&ancestor_for_hook).unwrap();
                fs::create_dir(ancestor_for_hook.join("memory")).unwrap();
                fs::create_dir(ancestor_for_hook.join("memory/pages")).unwrap();
            })),
            ..TestRuntime::default()
        };

        let guard = isolated(
            acquire_with_runtime(
                &corpus,
                LockMode::Shared,
                Duration::from_secs(1),
                &mut runtime,
            )
            .unwrap(),
        );

        assert_guard_is_bound_to_names(&guard);
        assert!(!stale_ancestor.join("memory").join(LOCK_NAME).exists());
        assert!(corpus.join(LOCK_NAME).is_file());
    }

    #[test]
    fn replacing_the_lock_after_open_restarts_on_the_named_inode() {
        let (_temporary, corpus) = writable_corpus();
        let mut runtime = TestRuntime {
            after_lock_opened: Some(Box::new(|path| replace_lock(path, ".write.lock-stale"))),
            ..TestRuntime::default()
        };

        let guard = isolated(
            acquire_with_runtime(
                &corpus,
                LockMode::Exclusive,
                Duration::from_secs(1),
                &mut runtime,
            )
            .unwrap(),
        );

        assert_guard_is_bound_to_names(&guard);
        assert!(corpus.join(".write.lock-stale").is_file());
    }

    #[test]
    fn replacing_the_lock_after_flock_unlocks_and_reopens_the_named_inode() {
        let (_temporary, corpus) = writable_corpus();
        let mut runtime = TestRuntime {
            after_flock_succeeded: Some(Box::new(|path| replace_lock(path, ".write.lock-stale"))),
            ..TestRuntime::default()
        };

        let guard = isolated(
            acquire_with_runtime(
                &corpus,
                LockMode::Exclusive,
                Duration::from_secs(1),
                &mut runtime,
            )
            .unwrap(),
        );

        assert_guard_is_bound_to_names(&guard);
        assert_eq!(
            runtime
                .flock_operations
                .iter()
                .filter(|operation| **operation == FlockOperation::NonBlockingLockExclusive)
                .count(),
            2
        );
        assert_eq!(
            runtime
                .flock_operations
                .iter()
                .filter(|operation| **operation == FlockOperation::Unlock)
                .count(),
            1
        );
    }

    #[test]
    fn replacing_the_corpus_after_flock_unlocks_and_reopens_the_named_tree() {
        let (_temporary, corpus) = writable_corpus();
        let stale = corpus.with_file_name("memory-stale");
        let stale_for_hook = stale.clone();
        let mut runtime = TestRuntime {
            after_flock_succeeded: Some(Box::new(move |lock_path| {
                let named_corpus = lock_path.parent().unwrap();
                fs::rename(named_corpus, &stale_for_hook).unwrap();
                fs::create_dir(named_corpus).unwrap();
                fs::create_dir(named_corpus.join("pages")).unwrap();
            })),
            ..TestRuntime::default()
        };

        let guard = isolated(
            acquire_with_runtime(
                &corpus,
                LockMode::Shared,
                Duration::from_secs(1),
                &mut runtime,
            )
            .unwrap(),
        );

        assert_guard_is_bound_to_names(&guard);
        assert!(stale.join(LOCK_NAME).is_file());
        assert!(corpus.join(LOCK_NAME).is_file());
        assert_eq!(
            runtime
                .flock_operations
                .iter()
                .filter(|operation| **operation == FlockOperation::Unlock)
                .count(),
            1
        );
    }

    #[test]
    fn a_waiter_reopens_a_replaced_free_lock_inode_after_backoff() {
        let (_temporary, corpus) = writable_corpus();
        let old_holder = isolated(acquire_lock(&corpus, LockMode::Exclusive).unwrap());
        let lock_path = corpus.canonicalize().unwrap().join(LOCK_NAME);
        let lock_for_hook = lock_path.clone();
        let mut runtime = TestRuntime {
            after_backoff: Some(Box::new(move || {
                replace_lock(&lock_for_hook, ".write.lock-held-old")
            })),
            ..TestRuntime::default()
        };

        let current_guard = isolated(
            acquire_with_runtime(
                &corpus,
                LockMode::Exclusive,
                Duration::from_millis(200),
                &mut runtime,
            )
            .unwrap(),
        );

        assert_guard_is_bound_to_names(&current_guard);
        assert_eq!(runtime.backoffs, [Duration::from_millis(50)]);
        assert!(corpus.join(".write.lock-held-old").is_file());
        drop(current_guard);
        drop(old_holder);
    }

    #[test]
    fn a_waiter_reopens_a_replaced_corpus_after_backoff() {
        let (_temporary, corpus) = writable_corpus();
        let old_holder = isolated(acquire_lock(&corpus, LockMode::Exclusive).unwrap());
        let detached = corpus.with_file_name("memory-held-old");
        let corpus_for_hook = corpus.clone();
        let detached_for_hook = detached.clone();
        let mut runtime = TestRuntime {
            after_backoff: Some(Box::new(move || {
                fs::rename(&corpus_for_hook, &detached_for_hook).unwrap();
                fs::create_dir(&corpus_for_hook).unwrap();
                fs::create_dir(corpus_for_hook.join("pages")).unwrap();
            })),
            ..TestRuntime::default()
        };

        let current_guard = isolated(
            acquire_with_runtime(
                &corpus,
                LockMode::Exclusive,
                Duration::from_millis(200),
                &mut runtime,
            )
            .unwrap(),
        );

        assert_guard_is_bound_to_names(&current_guard);
        assert_eq!(runtime.backoffs, [Duration::from_millis(50)]);
        assert!(detached.join(LOCK_NAME).is_file());
        assert!(corpus.join(LOCK_NAME).is_file());
        drop(current_guard);
        drop(old_holder);
    }

    #[test]
    fn lock_name_inode_churn_cannot_extend_the_original_deadline() {
        let (_temporary, corpus) = writable_corpus();
        let timeout = Duration::from_millis(25);
        let mut runtime = TestRuntime {
            open_behavior: OpenBehavior::Churn {
                calls: 0,
                step: Duration::from_millis(10),
                panic_after: 12,
            },
            ..TestRuntime::default()
        };

        let error =
            acquire_with_runtime(&corpus, LockMode::Exclusive, timeout, &mut runtime).unwrap_err();

        assert!(matches!(
            error,
            LockError::Busy { path, timeout: found, .. }
                if path == corpus.canonicalize().unwrap().join(LOCK_NAME) && found == timeout
        ));
        let OpenBehavior::Churn { calls, .. } = runtime.open_behavior else {
            unreachable!()
        };
        assert!(calls <= 4, "used {calls} open attempts after the deadline");
    }

    #[test]
    fn pinned_descriptors_are_close_on_exec() {
        let (_temporary, corpus) = writable_corpus();
        let guard = isolated(acquire_lock(&corpus, LockMode::Shared).unwrap());

        assert!(
            fcntl_getfd(guard.corpus_fd())
                .unwrap()
                .contains(FdFlags::CLOEXEC)
        );
        assert!(
            fcntl_getfd(&guard.target.lock_fd)
                .unwrap()
                .contains(FdFlags::CLOEXEC)
        );
    }
}
