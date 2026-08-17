use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rustix::fs::{self as rfs, AtFlags, FileType, FlockOperation, Mode, OFlags, Stat};
use rustix::io::Errno;
use rustix::process::geteuid;

use crate::home::StoreHome;
use crate::project::StoreError;

const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const EXISTING_FILE_FLAGS: OFlags = OFlags::RDWR
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);
const CREATE_FILE_FLAGS: OFlags = EXISTING_FILE_FLAGS
    .union(OFlags::CREATE)
    .union(OFlags::EXCL);
const SIDECAR_SUFFIXES: &[&str] = &["-wal", "-shm", "-journal"];
const SQLITE_INITIALIZATION_LOCK_SUFFIX: &str = "-init.lock";
const VECTOR_MUTATION_LOCK_NAME: &str = "vectors.sqlite3-mutation.lock";
const SQLITE_COORDINATION_LOCK_TIMEOUT: Duration = Duration::from_secs(5);
const SQLITE_COORDINATION_LOCK_RETRY: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DatabaseState {
    Empty,
    Existing,
}

pub(crate) trait OpenHooks {
    fn before_vector_initialization_lock(&mut self, _path: &Path) {}
    fn before_sqlite_open(&mut self, _path: &Path) {}
    fn after_sqlite_open(&mut self, _path: &Path) {}
    fn after_vector_sidecars_pinned(&mut self, _path: &Path) {}
    fn after_project_migration_table_creation(&mut self, _path: &Path) {}
}

pub(crate) struct NoHooks;

impl OpenHooks for NoHooks {}

pub(crate) fn immutable_uri(path: &Path) -> String {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";

    let mut uri = String::from("file:");
    for byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(*byte));
        } else {
            uri.push('%');
            uri.push(char::from(HEX[usize::from(byte >> 4)]));
            uri.push(char::from(HEX[usize::from(byte & 0x0f)]));
        }
    }
    uri.push_str("?mode=ro&immutable=1");
    uri
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
struct PinnedComponent {
    fd: OwnedFd,
    identity: Identity,
    path: PathBuf,
    name_in_parent: OsString,
}

/// A descriptor-pinned path through the private portion of a store home.
///
/// `rust-v1` and, for project stores, `indexes` are owned by the effective
/// user and mode 0700. This prevents other operating-system users from
/// replacing their children while SQLite opens them by pathname. The
/// canonical StoreHome base must itself live in a hierarchy where untrusted
/// users lack rename authority. A process running as the same effective user
/// remains inside the trust boundary; descriptor checks detect replacement at
/// each checked boundary but do not claim to defeat an indefinitely racing
/// peer with authority over the base or private directories.
#[derive(Debug)]
pub(crate) struct SecureStoreDirectory {
    base_fd: OwnedFd,
    base_identity: Identity,
    base_path: PathBuf,
    version: PinnedComponent,
    leaf: Option<PinnedComponent>,
}

impl SecureStoreDirectory {
    pub(crate) fn for_vectors(home: &StoreHome) -> Result<Self, StoreError> {
        Self::open(home, false)
    }

    pub(crate) fn for_project(home: &StoreHome) -> Result<Self, StoreError> {
        Self::open(home, true)
    }

    fn open(home: &StoreHome, with_indexes: bool) -> Result<Self, StoreError> {
        let base = home
            .version_dir()
            .parent()
            .expect("StoreHome always appends rust-v1");
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o700);
        builder
            .create(base)
            .map_err(|source| StoreError::CreateDirectory {
                path: base.to_path_buf(),
                source,
            })?;
        let base_path = fs::canonicalize(base).map_err(|source| StoreError::InspectPath {
            operation: "resolve store home",
            path: base.to_path_buf(),
            source,
        })?;
        let base_fd = rfs::open(&base_path, DIRECTORY_FLAGS, Mode::empty())
            .map_err(|error| io_error("open store home", &base_path, error))?;
        let base_stat = rfs::fstat(&base_fd)
            .map_err(|error| io_error("inspect store home", &base_path, error))?;
        validate_base_directory(&base_path, &base_stat)?;
        let base_identity = Identity::from_stat(&base_stat);

        let version = open_private_directory(&base_fd, &base_path, "rust-v1")?;
        let leaf = if with_indexes {
            Some(open_private_directory(
                &version.fd,
                &version.path,
                "indexes",
            )?)
        } else {
            None
        };
        let directory = Self {
            base_fd,
            base_identity,
            base_path,
            version,
            leaf,
        };
        directory.revalidate()?;
        Ok(directory)
    }

    pub(crate) fn path(&self) -> &Path {
        self.leaf
            .as_ref()
            .map_or(self.version.path.as_path(), |leaf| leaf.path.as_path())
    }

    fn fd(&self) -> &OwnedFd {
        self.leaf.as_ref().map_or(&self.version.fd, |leaf| &leaf.fd)
    }

    pub(crate) fn prepare_database_without_sidecar_check(
        &self,
        name: &OsStr,
    ) -> Result<PinnedDatabase, StoreError> {
        self.revalidate()?;

        let path = self.path().join(name);
        let (fd, created) = open_or_create_database(self.fd(), name, &path)?;
        if created {
            rfs::fchmod(&fd, Mode::RUSR | Mode::WUSR)
                .map_err(|error| io_error("set private database permissions", &path, error))?;
        }
        let stat = rfs::fstat(&fd)
            .map_err(|error| io_error("inspect database descriptor", &path, error))?;
        validate_database_file(&path, &stat)?;
        let identity = Identity::from_stat(&stat);
        verify_named_identity(self.fd(), name, &path, identity)?;

        Ok(PinnedDatabase {
            fd,
            identity,
            name: name.to_owned(),
            path,
            state: if stat.st_size == 0 {
                DatabaseState::Empty
            } else {
                DatabaseState::Existing
            },
        })
    }

    /// Serializes project inspection, migration, and schema initialization.
    ///
    /// This BSD lock is held on the pinned owner-private `indexes` directory,
    /// not on a SQLite database inode. That keeps it independent of SQLite's
    /// POSIX `fcntl` locks on macOS while avoiding filesystem mutations.
    pub(crate) fn lock_project_indexes(
        &self,
        database_path: &Path,
    ) -> Result<SqliteInitializationGuard<'_>, StoreError> {
        self.lock_project_indexes_for(database_path, SQLITE_COORDINATION_LOCK_TIMEOUT)
    }

    fn lock_project_indexes_for(
        &self,
        database_path: &Path,
        timeout: Duration,
    ) -> Result<SqliteInitializationGuard<'_>, StoreError> {
        let indexes = self
            .leaf
            .as_ref()
            .expect("project stores always pin an indexes directory");
        lock_fd_for(
            &indexes.fd,
            timeout,
            "inspect and initialize project SQLite schema",
            database_path,
            "acquire project-index coordination lock",
            &indexes.path,
        )
    }

    pub(crate) fn revalidate(&self) -> Result<(), StoreError> {
        let base_stat = rfs::fstat(&self.base_fd)
            .map_err(|error| io_error("reinspect store home", &self.base_path, error))?;
        validate_base_directory(&self.base_path, &base_stat)?;
        if Identity::from_stat(&base_stat) != self.base_identity {
            return Err(StoreError::RacedStorePath {
                path: self.base_path.clone(),
            });
        }
        let named_base =
            rfs::open(&self.base_path, DIRECTORY_FLAGS, Mode::empty()).map_err(|_| {
                StoreError::RacedStorePath {
                    path: self.base_path.clone(),
                }
            })?;
        let named_base_stat = rfs::fstat(&named_base)
            .map_err(|error| io_error("reinspect named store home", &self.base_path, error))?;
        if Identity::from_stat(&named_base_stat) != self.base_identity {
            return Err(StoreError::RacedStorePath {
                path: self.base_path.clone(),
            });
        }
        validate_base_directory(&self.base_path, &named_base_stat)?;
        revalidate_component(&self.base_fd, &self.version)?;
        if let Some(leaf) = &self.leaf {
            revalidate_component(&self.version.fd, leaf)?;
        }
        Ok(())
    }

    pub(crate) fn refuse_sidecars(&self, name: &OsStr) -> Result<(), StoreError> {
        for suffix in SIDECAR_SUFFIXES {
            self.refuse_sidecar(name, suffix)?;
        }
        Ok(())
    }

    pub(crate) fn sqlite_sidecars_absent(&self, name: &OsStr) -> Result<bool, StoreError> {
        self.revalidate()?;
        for suffix in SIDECAR_SUFFIXES {
            let sidecar_name = appended_name(name, suffix);
            match rfs::statat(self.fd(), &sidecar_name, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => return Ok(false),
                Err(Errno::NOENT) => {}
                Err(error) => {
                    return Err(io_error(
                        "inspect SQLite sidecar",
                        &self.path().join(sidecar_name),
                        error,
                    ));
                }
            }
        }
        Ok(true)
    }

    pub(crate) fn refuse_sidecar(&self, name: &OsStr, suffix: &str) -> Result<(), StoreError> {
        let sidecar_name = appended_name(name, suffix);
        match rfs::statat(self.fd(), &sidecar_name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(_) => Err(StoreError::UnsafeSidecar {
                path: self.path().join(sidecar_name),
            }),
            Err(Errno::NOENT) => Ok(()),
            Err(error) => Err(io_error(
                "inspect SQLite sidecar",
                &self.path().join(sidecar_name),
                error,
            )),
        }
    }

    pub(crate) fn secure_sqlite_sidecar(
        &self,
        database_name: &OsStr,
        suffix: &str,
    ) -> Result<(), StoreError> {
        let name = appended_name(database_name, suffix);
        let path = self.path().join(&name);
        let fd = rfs::openat(self.fd(), &name, EXISTING_FILE_FLAGS, Mode::empty())
            .map_err(|error| io_error("open SQLite-created sidecar", &path, error))?;
        let before = rfs::fstat(&fd)
            .map_err(|error| io_error("inspect SQLite-created sidecar", &path, error))?;
        validate_owned_single_link_regular(&path, &before)?;
        rfs::fchmod(&fd, Mode::RUSR | Mode::WUSR)
            .map_err(|error| io_error("set private sidecar permissions", &path, error))?;
        let after = rfs::fstat(&fd)
            .map_err(|error| io_error("reinspect SQLite-created sidecar", &path, error))?;
        validate_database_file(&path, &after)?;
        verify_named_identity(self.fd(), &name, &path, Identity::from_stat(&after))
    }

    pub(crate) fn pin_empty_vector_reader_sidecars(
        &self,
        database_name: &OsStr,
    ) -> Result<Option<PinnedVectorReaderSidecars>, StoreError> {
        self.revalidate()?;
        self.refuse_sidecar(database_name, "-journal")?;
        let wal = self.pin_exact_sidecar(database_name, "-wal", 0)?;
        let shm = self.pin_exact_sidecar(database_name, "-shm", 32_768)?;
        match (wal, shm) {
            (None, None) => Ok(None),
            (Some(wal), Some(shm)) => Ok(Some(PinnedVectorReaderSidecars { wal, shm })),
            (Some(sidecar), None) | (None, Some(sidecar)) => {
                Err(StoreError::UnsafeSidecar { path: sidecar.path })
            }
        }
    }

    fn pin_exact_sidecar(
        &self,
        database_name: &OsStr,
        suffix: &str,
        expected_size: u64,
    ) -> Result<Option<PinnedSidecar>, StoreError> {
        let name = appended_name(database_name, suffix);
        let path = self.path().join(&name);
        let fd = match rfs::openat(self.fd(), &name, EXISTING_FILE_FLAGS, Mode::empty()) {
            Ok(fd) => fd,
            Err(Errno::NOENT) => return Ok(None),
            Err(_) => return Err(StoreError::UnsafeSidecar { path }),
        };
        let stat = rfs::fstat(&fd).map_err(|_| StoreError::UnsafeSidecar { path: path.clone() })?;
        validate_exact_vector_reader_sidecar(&path, &stat, expected_size)?;
        let identity = Identity::from_stat(&stat);
        verify_exact_sidecar_name(self.fd(), &name, &path, identity, expected_size)?;
        Ok(Some(PinnedSidecar {
            fd,
            identity,
            name,
            path,
            expected_size,
        }))
    }

    pub(crate) fn prepare_sqlite_initialization_lock(
        &self,
        database_name: &OsStr,
    ) -> Result<PinnedSqliteInitializationLock, StoreError> {
        self.revalidate()?;
        let name = appended_name(database_name, SQLITE_INITIALIZATION_LOCK_SUFFIX);
        let path = self.path().join(&name);
        let database_path = self.path().join(database_name);
        let (fd, created) = open_or_create_database(self.fd(), &name, &path)?;
        if created {
            rfs::fchmod(&fd, Mode::RUSR | Mode::WUSR)
                .map_err(|error| io_error("set private lock-file permissions", &path, error))?;
        }
        let stat = rfs::fstat(&fd)
            .map_err(|error| io_error("inspect lock-file descriptor", &path, error))?;
        validate_database_file(&path, &stat)?;
        let identity = Identity::from_stat(&stat);
        verify_named_identity(self.fd(), &name, &path, identity)?;

        Ok(PinnedSqliteInitializationLock {
            fd,
            identity,
            name,
            path,
            database_path,
        })
    }

    fn prepare_vector_mutation_lock(&self) -> Result<PinnedVectorMutationLock, StoreError> {
        self.revalidate()?;
        let name = OsString::from(VECTOR_MUTATION_LOCK_NAME);
        let path = self.path().join(&name);
        let database_path = self.path().join("vectors.sqlite3");
        let (fd, created) = open_or_create_database(self.fd(), &name, &path)?;
        if created {
            rfs::fchmod(&fd, Mode::RUSR | Mode::WUSR)
                .map_err(|error| io_error("set private lock-file permissions", &path, error))?;
        }
        let stat = rfs::fstat(&fd)
            .map_err(|error| io_error("inspect lock-file descriptor", &path, error))?;
        validate_database_file(&path, &stat)?;
        let identity = Identity::from_stat(&stat);
        verify_named_identity(self.fd(), &name, &path, identity)?;
        Ok(PinnedVectorMutationLock {
            fd,
            identity,
            name,
            path,
            database_path,
        })
    }
}

#[derive(Debug)]
pub(crate) struct PinnedVectorReaderSidecars {
    wal: PinnedSidecar,
    shm: PinnedSidecar,
}

impl PinnedVectorReaderSidecars {
    pub(crate) fn revalidate(&self, directory: &SecureStoreDirectory) -> Result<(), StoreError> {
        directory.revalidate()?;
        self.wal.revalidate(directory)?;
        self.shm.revalidate(directory)
    }
}

#[derive(Debug)]
struct PinnedSidecar {
    fd: OwnedFd,
    identity: Identity,
    name: OsString,
    path: PathBuf,
    expected_size: u64,
}

impl PinnedSidecar {
    fn revalidate(&self, directory: &SecureStoreDirectory) -> Result<(), StoreError> {
        let stat = rfs::fstat(&self.fd).map_err(|_| StoreError::UnsafeSidecar {
            path: self.path.clone(),
        })?;
        validate_exact_vector_reader_sidecar(&self.path, &stat, self.expected_size)?;
        if Identity::from_stat(&stat) != self.identity {
            return Err(StoreError::UnsafeSidecar {
                path: self.path.clone(),
            });
        }
        verify_exact_sidecar_name(
            directory.fd(),
            &self.name,
            &self.path,
            self.identity,
            self.expected_size,
        )
    }
}

#[derive(Debug)]
pub(crate) struct PinnedDatabase {
    fd: OwnedFd,
    identity: Identity,
    name: OsString,
    path: PathBuf,
    state: DatabaseState,
}

impl PinnedDatabase {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn name(&self) -> &OsStr {
        &self.name
    }

    pub(crate) fn state(&self) -> DatabaseState {
        self.state
    }

    pub(crate) fn sqlite_journal_versions(&self) -> Result<(u8, u8), StoreError> {
        let mut header = [0_u8; 20];
        let read = rustix::io::pread(&self.fd, &mut header[..], 0)
            .map_err(|error| io_error("read SQLite header journal versions", &self.path, error))?;
        if read != header.len() || &header[..16] != b"SQLite format 3\0" {
            return Err(StoreError::Integrity {
                path: self.path.clone(),
                detail: "database has no complete SQLite format-3 header".to_owned(),
            });
        }
        Ok((header[18], header[19]))
    }

    pub(crate) fn revalidate(&self, directory: &SecureStoreDirectory) -> Result<(), StoreError> {
        directory.revalidate()?;
        let stat = rfs::fstat(&self.fd)
            .map_err(|error| io_error("reinspect database descriptor", &self.path, error))?;
        validate_database_file(&self.path, &stat)?;
        if Identity::from_stat(&stat) != self.identity {
            return Err(StoreError::RacedStorePath {
                path: self.path.clone(),
            });
        }
        verify_named_identity(directory.fd(), &self.name, &self.path, self.identity)
    }
}

#[derive(Debug)]
pub(crate) struct PinnedSqliteInitializationLock {
    fd: OwnedFd,
    identity: Identity,
    name: OsString,
    path: PathBuf,
    database_path: PathBuf,
}

impl PinnedSqliteInitializationLock {
    /// Serializes the short SQLite schema-to-WAL initialization window.
    ///
    /// The bundled SQLite VFS uses POSIX `fcntl` locks on SQLite files. This
    /// separate BSD `flock` is advisory and anchored on its own pinned 0600
    /// inode, so cooperating Rust openers coordinate without interfering with
    /// SQLite. Non-cooperating same-UID peers remain inside the documented
    /// trust boundary.
    pub(crate) fn lock(&self) -> Result<SqliteInitializationGuard<'_>, StoreError> {
        self.lock_for(SQLITE_COORDINATION_LOCK_TIMEOUT)
    }

    fn lock_for(&self, timeout: Duration) -> Result<SqliteInitializationGuard<'_>, StoreError> {
        lock_fd_for(
            &self.fd,
            timeout,
            "initialize vector SQLite schema and WAL",
            &self.database_path,
            "acquire vector SQLite initialization lock",
            &self.path,
        )
    }

    pub(crate) fn revalidate(&self, directory: &SecureStoreDirectory) -> Result<(), StoreError> {
        directory.revalidate()?;
        let stat = rfs::fstat(&self.fd)
            .map_err(|error| io_error("reinspect lock-file descriptor", &self.path, error))?;
        validate_database_file(&self.path, &stat)?;
        if Identity::from_stat(&stat) != self.identity {
            return Err(StoreError::RacedStorePath {
                path: self.path.clone(),
            });
        }
        verify_named_identity(directory.fd(), &self.name, &self.path, self.identity)
    }
}

#[derive(Debug)]
pub(crate) struct SqliteInitializationGuard<'a> {
    fd: &'a OwnedFd,
}

impl Drop for SqliteInitializationGuard<'_> {
    fn drop(&mut self) {
        let _ = rfs::flock(self.fd, FlockOperation::Unlock);
    }
}

#[derive(Debug)]
struct PinnedVectorMutationLock {
    fd: OwnedFd,
    identity: Identity,
    name: OsString,
    path: PathBuf,
    database_path: PathBuf,
}

impl PinnedVectorMutationLock {
    fn revalidate(&self, directory: &SecureStoreDirectory) -> Result<(), StoreError> {
        directory.revalidate()?;
        let stat = rfs::fstat(&self.fd)
            .map_err(|error| io_error("reinspect vector mutation lock", &self.path, error))?;
        validate_database_file(&self.path, &stat)?;
        if Identity::from_stat(&stat) != self.identity {
            return Err(StoreError::RacedStorePath {
                path: self.path.clone(),
            });
        }
        verify_named_identity(directory.fd(), &self.name, &self.path, self.identity)
    }
}

/// Owned, descriptor-pinned exclusion for vector reference and sweep mutations.
#[derive(Debug)]
pub(crate) struct HeldVectorMutationLock {
    directory: SecureStoreDirectory,
    lock: PinnedVectorMutationLock,
}

impl HeldVectorMutationLock {
    pub(crate) fn acquire(home: &StoreHome) -> Result<Self, StoreError> {
        Self::acquire_for(home, SQLITE_COORDINATION_LOCK_TIMEOUT, false)
    }

    pub(crate) fn acquire_shared(home: &StoreHome) -> Result<Self, StoreError> {
        Self::acquire_for(home, SQLITE_COORDINATION_LOCK_TIMEOUT, true)
    }

    #[cfg(test)]
    pub(crate) fn acquire_without_waiting(home: &StoreHome) -> Result<Self, StoreError> {
        Self::acquire_for(home, Duration::ZERO, false)
    }

    #[cfg(test)]
    pub(crate) fn acquire_shared_without_waiting(home: &StoreHome) -> Result<Self, StoreError> {
        Self::acquire_for(home, Duration::ZERO, true)
    }

    fn acquire_for(home: &StoreHome, timeout: Duration, shared: bool) -> Result<Self, StoreError> {
        let directory = SecureStoreDirectory::for_vectors(home)?;
        let lock = directory.prepare_vector_mutation_lock()?;
        flock_for(
            &lock.fd,
            timeout,
            if shared {
                FlockOperation::NonBlockingLockShared
            } else {
                FlockOperation::NonBlockingLockExclusive
            },
            "coordinate vector references and garbage collection",
            &lock.database_path,
            "acquire vector mutation lock",
            &lock.path,
        )?;
        if let Err(error) = lock.revalidate(&directory) {
            let _ = rfs::flock(&lock.fd, FlockOperation::Unlock);
            return Err(error);
        }
        Ok(Self { directory, lock })
    }

    pub(crate) fn revalidate(&self) -> Result<(), StoreError> {
        self.lock.revalidate(&self.directory)
    }

    pub(crate) fn database_path(&self) -> &Path {
        &self.lock.database_path
    }
}

impl Drop for HeldVectorMutationLock {
    fn drop(&mut self) {
        let _ = rfs::flock(&self.lock.fd, FlockOperation::Unlock);
    }
}

fn lock_fd_for<'a>(
    fd: &'a OwnedFd,
    timeout: Duration,
    busy_operation: &'static str,
    busy_path: &Path,
    error_operation: &'static str,
    error_path: &Path,
) -> Result<SqliteInitializationGuard<'a>, StoreError> {
    flock_exclusive_for(
        fd,
        timeout,
        busy_operation,
        busy_path,
        error_operation,
        error_path,
    )?;
    Ok(SqliteInitializationGuard { fd })
}

fn flock_exclusive_for(
    fd: &OwnedFd,
    timeout: Duration,
    busy_operation: &'static str,
    busy_path: &Path,
    error_operation: &'static str,
    error_path: &Path,
) -> Result<(), StoreError> {
    flock_for(
        fd,
        timeout,
        FlockOperation::NonBlockingLockExclusive,
        busy_operation,
        busy_path,
        error_operation,
        error_path,
    )
}

fn flock_for(
    fd: &OwnedFd,
    timeout: Duration,
    operation: FlockOperation,
    busy_operation: &'static str,
    busy_path: &Path,
    error_operation: &'static str,
    error_path: &Path,
) -> Result<(), StoreError> {
    let deadline = Instant::now() + timeout;
    loop {
        match rfs::flock(fd, operation) {
            Ok(()) => return Ok(()),
            Err(error) if error == Errno::WOULDBLOCK || error == Errno::AGAIN => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(StoreError::Busy {
                        operation: busy_operation,
                        path: busy_path.to_path_buf(),
                    });
                }
                thread::sleep(SQLITE_COORDINATION_LOCK_RETRY.min(remaining));
            }
            Err(Errno::INTR) => {
                if Instant::now() >= deadline {
                    return Err(StoreError::Busy {
                        operation: busy_operation,
                        path: busy_path.to_path_buf(),
                    });
                }
            }
            Err(error) => return Err(io_error(error_operation, error_path, error)),
        }
    }
}

fn open_private_directory(
    parent: &OwnedFd,
    parent_path: &Path,
    name: &str,
) -> Result<PinnedComponent, StoreError> {
    let path = parent_path.join(name);
    let (created, named_before) = match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            if !FileType::from_raw_mode(stat.st_mode).is_dir() {
                return Err(StoreError::UnsafeStoreDirectory {
                    path,
                    reason: "path is not a directory (symlinks are never followed)".to_owned(),
                });
            }
            (false, Some(Identity::from_stat(&stat)))
        }
        Err(Errno::NOENT) => match rfs::mkdirat(parent, name, Mode::RWXU) {
            Ok(()) => (true, None),
            Err(Errno::EXIST) => {
                let stat = rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(|error| io_error("reinspect raced store directory", &path, error))?;
                if !FileType::from_raw_mode(stat.st_mode).is_dir() {
                    return Err(StoreError::UnsafeStoreDirectory {
                        path,
                        reason: "raced path is not a directory (symlinks are never followed)"
                            .to_owned(),
                    });
                }
                (false, Some(Identity::from_stat(&stat)))
            }
            Err(error) => return Err(io_error("create private store directory", &path, error)),
        },
        Err(error) => return Err(io_error("inspect private store directory", &path, error)),
    };
    let fd = rfs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
        StoreError::UnsafeStoreDirectory {
            path: path.clone(),
            reason: format!("could not open without following links: {error}"),
        }
    })?;
    if created {
        rfs::fchmod(&fd, Mode::RWXU)
            .map_err(|error| io_error("set private directory permissions", &path, error))?;
    }
    let stat = rfs::fstat(&fd)
        .map_err(|error| io_error("inspect private directory descriptor", &path, error))?;
    validate_private_directory(&path, &stat)?;
    let identity = Identity::from_stat(&stat);
    if named_before.is_some_and(|before| before != identity) {
        return Err(StoreError::RacedStorePath { path });
    }
    verify_named_directory(parent, OsStr::new(name), &path, identity)?;
    Ok(PinnedComponent {
        fd,
        identity,
        path,
        name_in_parent: name.into(),
    })
}

fn open_or_create_database(
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
) -> Result<(OwnedFd, bool), StoreError> {
    match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => {
            validate_database_file(path, &stat)?;
            let expected = Identity::from_stat(&stat);
            let fd =
                rfs::openat(parent, name, EXISTING_FILE_FLAGS, Mode::empty()).map_err(|error| {
                    io_error(
                        "open existing database without following links",
                        path,
                        error,
                    )
                })?;
            let opened = rfs::fstat(&fd)
                .map_err(|error| io_error("inspect opened database", path, error))?;
            if Identity::from_stat(&opened) != expected {
                return Err(StoreError::RacedStorePath {
                    path: path.to_path_buf(),
                });
            }
            Ok((fd, false))
        }
        Err(Errno::NOENT) => {
            match rfs::openat(parent, name, CREATE_FILE_FLAGS, Mode::RUSR | Mode::WUSR) {
                Ok(fd) => Ok((fd, true)),
                Err(Errno::EXIST) => {
                    let stat = rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
                        .map_err(|error| io_error("reinspect raced database path", path, error))?;
                    validate_database_file(path, &stat)?;
                    let expected = Identity::from_stat(&stat);
                    let fd = rfs::openat(parent, name, EXISTING_FILE_FLAGS, Mode::empty())
                        .map_err(|error| {
                            io_error("open raced database path safely", path, error)
                        })?;
                    let opened = rfs::fstat(&fd).map_err(|error| {
                        io_error("inspect raced database descriptor", path, error)
                    })?;
                    if Identity::from_stat(&opened) != expected {
                        return Err(StoreError::RacedStorePath {
                            path: path.to_path_buf(),
                        });
                    }
                    Ok((fd, false))
                }
                Err(error) => Err(io_error("atomically create private database", path, error)),
            }
        }
        Err(error) => Err(io_error("inspect database path", path, error)),
    }
}

fn validate_base_directory(path: &Path, stat: &Stat) -> Result<(), StoreError> {
    if !FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Err(StoreError::UnsafeStoreDirectory {
            path: path.to_path_buf(),
            reason: "store home is not a directory".to_owned(),
        });
    }
    if owner_id(stat) != geteuid().as_raw() {
        return Err(StoreError::UnsafeStoreDirectory {
            path: path.to_path_buf(),
            reason: "store home is not owned by the effective user".to_owned(),
        });
    }
    if mode_bits(stat) & 0o022 != 0 {
        return Err(StoreError::UnsafeStoreDirectory {
            path: path.to_path_buf(),
            reason: format!(
                "store home mode {:04o} permits another user to write",
                mode_bits(stat)
            ),
        });
    }
    Ok(())
}

fn validate_private_directory(path: &Path, stat: &Stat) -> Result<(), StoreError> {
    if !FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Err(StoreError::UnsafeStoreDirectory {
            path: path.to_path_buf(),
            reason: "path is not a directory".to_owned(),
        });
    }
    if owner_id(stat) != geteuid().as_raw() {
        return Err(StoreError::UnsafeStoreDirectory {
            path: path.to_path_buf(),
            reason: "directory is not owned by the effective user".to_owned(),
        });
    }
    if mode_bits(stat) != 0o700 {
        return Err(StoreError::UnsafeStoreDirectory {
            path: path.to_path_buf(),
            reason: format!("directory mode must be 0700, found {:04o}", mode_bits(stat)),
        });
    }
    Ok(())
}

fn validate_owned_single_link_regular(path: &Path, stat: &Stat) -> Result<(), StoreError> {
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(StoreError::NotRegular {
            path: path.to_path_buf(),
        });
    }
    if owner_id(stat) != geteuid().as_raw() {
        return Err(StoreError::UnsafeStoreFile {
            path: path.to_path_buf(),
            reason: "file is not owned by the effective user".to_owned(),
        });
    }
    if stat.st_nlink != 1 {
        return Err(StoreError::UnsafeStoreFile {
            path: path.to_path_buf(),
            reason: format!(
                "file must have exactly one hard link, found {}",
                stat.st_nlink
            ),
        });
    }
    Ok(())
}

fn validate_database_file(path: &Path, stat: &Stat) -> Result<(), StoreError> {
    validate_owned_single_link_regular(path, stat)?;
    if mode_bits(stat) != 0o600 {
        return Err(StoreError::UnsafeStoreFile {
            path: path.to_path_buf(),
            reason: format!("file mode must be 0600, found {:04o}", mode_bits(stat)),
        });
    }
    Ok(())
}

fn validate_exact_vector_reader_sidecar(
    path: &Path,
    stat: &Stat,
    expected_size: u64,
) -> Result<(), StoreError> {
    #[allow(clippy::unnecessary_cast)]
    let size = stat.st_size as u64;
    if !FileType::from_raw_mode(stat.st_mode).is_file()
        || owner_id(stat) != geteuid().as_raw()
        || stat.st_nlink != 1
        || mode_bits(stat) != 0o600
        || size != expected_size
    {
        return Err(StoreError::UnsafeSidecar {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn verify_exact_sidecar_name(
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
    expected: Identity,
    expected_size: u64,
) -> Result<(), StoreError> {
    let named = rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW).map_err(|_| {
        StoreError::UnsafeSidecar {
            path: path.to_path_buf(),
        }
    })?;
    if Identity::from_stat(&named) != expected {
        return Err(StoreError::UnsafeSidecar {
            path: path.to_path_buf(),
        });
    }
    validate_exact_vector_reader_sidecar(path, &named, expected_size)
}

fn revalidate_component(parent: &OwnedFd, component: &PinnedComponent) -> Result<(), StoreError> {
    let stat = rfs::fstat(&component.fd).map_err(|error| {
        io_error(
            "reinspect private directory descriptor",
            &component.path,
            error,
        )
    })?;
    validate_private_directory(&component.path, &stat)?;
    if Identity::from_stat(&stat) != component.identity {
        return Err(StoreError::RacedStorePath {
            path: component.path.clone(),
        });
    }
    verify_named_directory(
        parent,
        &component.name_in_parent,
        &component.path,
        component.identity,
    )
}

fn verify_named_directory(
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
    expected: Identity,
) -> Result<(), StoreError> {
    let named = match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) => named,
        Err(Errno::NOENT) => {
            return Err(StoreError::RacedStorePath {
                path: path.to_path_buf(),
            });
        }
        Err(error) => return Err(io_error("reinspect named private directory", path, error)),
    };
    if Identity::from_stat(&named) != expected {
        return Err(StoreError::RacedStorePath {
            path: path.to_path_buf(),
        });
    }
    validate_private_directory(path, &named)?;
    Ok(())
}

fn verify_named_identity(
    parent: &OwnedFd,
    name: &OsStr,
    path: &Path,
    expected: Identity,
) -> Result<(), StoreError> {
    let named = match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(named) => named,
        Err(Errno::NOENT) => {
            return Err(StoreError::RacedStorePath {
                path: path.to_path_buf(),
            });
        }
        Err(error) => return Err(io_error("reinspect named database", path, error)),
    };
    if Identity::from_stat(&named) != expected {
        return Err(StoreError::RacedStorePath {
            path: path.to_path_buf(),
        });
    }
    validate_database_file(path, &named)?;
    Ok(())
}

pub(crate) fn appended_name(name: &OsStr, suffix: &str) -> OsString {
    let mut value = name.to_owned();
    value.push(suffix);
    value
}

fn mode_bits(stat: &Stat) -> u32 {
    stat.st_mode as u32 & 0o7777
}

// rustix exposes different Stat field widths across supported targets.
#[allow(clippy::unnecessary_cast)]
fn owner_id(stat: &Stat) -> u32 {
    stat.st_uid as u32
}

fn io_error(operation: &'static str, path: &Path, error: Errno) -> StoreError {
    StoreError::InspectPath {
        operation,
        path: path.to_path_buf(),
        source: io::Error::from(error),
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    use std::process::Command;
    use std::time::Duration;

    use rusqlite::{Connection, TransactionBehavior};
    use rustix::io::{FdFlags, fcntl_getfd};
    use tempfile::tempdir;

    use super::{
        HeldVectorMutationLock, SecureStoreDirectory, validate_exact_vector_reader_sidecar,
    };
    use crate::{StoreError, StoreHome};

    #[test]
    fn project_indexes_lock_has_bounded_contention_and_releases_across_descriptors() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let first_directory = SecureStoreDirectory::for_project(&home).unwrap();
        let database = first_directory
            .prepare_database_without_sidecar_check(OsStr::new("project.sqlite3"))
            .unwrap();
        let second_directory = SecureStoreDirectory::for_project(&home).unwrap();
        let indexes = first_directory.leaf.as_ref().unwrap();
        assert!(fcntl_getfd(&indexes.fd).unwrap().contains(FdFlags::CLOEXEC));

        let held = first_directory
            .lock_project_indexes_for(database.path(), Duration::ZERO)
            .unwrap();
        let error = second_directory
            .lock_project_indexes_for(database.path(), Duration::ZERO)
            .unwrap_err();
        assert!(matches!(
            error,
            StoreError::Busy {
                operation: "inspect and initialize project SQLite schema",
                ..
            }
        ));

        drop(held);
        second_directory
            .lock_project_indexes_for(database.path(), Duration::ZERO)
            .unwrap();
    }

    #[test]
    fn sqlite_transaction_succeeds_while_project_indexes_lock_is_held() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let directory = SecureStoreDirectory::for_project(&home).unwrap();
        let database = directory
            .prepare_database_without_sidecar_check(OsStr::new("project.sqlite3"))
            .unwrap();
        let _held = directory
            .lock_project_indexes_for(database.path(), Duration::ZERO)
            .unwrap();
        let mut connection = Connection::open(database.path()).unwrap();

        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        transaction
            .execute_batch("CREATE TABLE proof (value INTEGER) STRICT;")
            .unwrap();
        transaction.commit().unwrap();

        let exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'proof')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists);
    }

    #[test]
    fn sqlite_initialization_lock_contends_across_independent_descriptors() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let directory = SecureStoreDirectory::for_vectors(&home).unwrap();
        let first = directory
            .prepare_sqlite_initialization_lock(OsStr::new("vectors.sqlite3"))
            .unwrap();
        let second = directory
            .prepare_sqlite_initialization_lock(OsStr::new("vectors.sqlite3"))
            .unwrap();

        let held = first.lock_for(Duration::ZERO).unwrap();
        let error = second.lock_for(Duration::ZERO).unwrap_err();

        assert!(matches!(
            error,
            StoreError::Busy {
                operation: "initialize vector SQLite schema and WAL",
                ..
            }
        ));

        drop(held);
        second.lock_for(Duration::ZERO).unwrap();
    }

    #[test]
    fn sqlite_initialization_lock_revalidation_detects_named_replacement() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let directory = SecureStoreDirectory::for_vectors(&home).unwrap();
        let lock = directory
            .prepare_sqlite_initialization_lock(OsStr::new("vectors.sqlite3"))
            .unwrap();
        let backup = tmp.path().join("pinned-init.lock");
        std::fs::rename(&lock.path, &backup).unwrap();
        std::fs::write(&lock.path, b"").unwrap();
        std::fs::set_permissions(&lock.path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let _held = lock.lock_for(Duration::ZERO).unwrap();
        let error = lock.revalidate(&directory).unwrap_err();

        assert!(matches!(error, StoreError::RacedStorePath { .. }));
        assert_eq!(std::fs::metadata(backup).unwrap().len(), 0);
    }

    #[test]
    fn vector_mutation_lock_has_bounded_contention_and_releases() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let first = HeldVectorMutationLock::acquire_for(&home, Duration::ZERO, false).unwrap();

        let error = HeldVectorMutationLock::acquire_for(&home, Duration::ZERO, false).unwrap_err();
        assert!(matches!(
            error,
            StoreError::Busy {
                operation: "coordinate vector references and garbage collection",
                ..
            }
        ));

        drop(first);
        HeldVectorMutationLock::acquire_for(&home, Duration::ZERO, false).unwrap();
    }

    #[test]
    fn vector_mutation_lock_revalidation_detects_named_replacement() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let held = HeldVectorMutationLock::acquire_for(&home, Duration::ZERO, false).unwrap();
        let backup = tmp.path().join("pinned-vector-mutation.lock");
        std::fs::rename(&held.lock.path, &backup).unwrap();
        std::fs::write(&held.lock.path, b"").unwrap();
        std::fs::set_permissions(&held.lock.path, std::fs::Permissions::from_mode(0o600)).unwrap();

        let error = held.revalidate().unwrap_err();

        assert!(matches!(error, StoreError::RacedStorePath { .. }));
        assert_eq!(std::fs::metadata(backup).unwrap().len(), 0);
    }

    #[test]
    fn vector_mutation_lock_contends_across_processes() {
        const PROBE_ENV: &str = "YAMS_VECTOR_MUTATION_PROBE_HOME";

        let tmp = tempdir().unwrap();
        let base = tmp.path().join("state");
        let home = StoreHome::new(&base);
        let _held = HeldVectorMutationLock::acquire_for(&home, Duration::ZERO, false).unwrap();
        let output = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "secure::tests::vector_mutation_lock_child_probe",
                "--nocapture",
            ])
            .env(PROBE_ENV, &base)
            .output()
            .unwrap();

        assert!(
            output.status.success(),
            "child probe failed:\n{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn vector_mutation_lock_child_probe() {
        const PROBE_ENV: &str = "YAMS_VECTOR_MUTATION_PROBE_HOME";

        let Some(base) = std::env::var_os(PROBE_ENV) else {
            return;
        };
        let error =
            HeldVectorMutationLock::acquire_for(&StoreHome::new(base), Duration::ZERO, false)
                .unwrap_err();
        assert!(matches!(
            error,
            StoreError::Busy {
                operation: "coordinate vector references and garbage collection",
                ..
            }
        ));
    }

    #[test]
    fn vector_reader_sidecar_validation_refuses_a_foreign_owner() {
        let tmp = tempdir().unwrap();
        let path = tmp.path().join("vectors.sqlite3-wal");
        std::fs::write(&path, b"").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let mut stat = rustix::fs::stat(&path).unwrap();
        stat.st_uid = rustix::process::geteuid().as_raw().wrapping_add(1);

        let error = validate_exact_vector_reader_sidecar(&path, &stat, 0).unwrap_err();

        assert!(matches!(error, StoreError::UnsafeSidecar { .. }));
        assert_eq!(std::fs::read(path).unwrap(), b"");
    }
}
