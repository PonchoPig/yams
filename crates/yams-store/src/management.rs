//! Read-only index inventory and conservative store maintenance.
//!
//! Management reads deliberately use a separate opener from [`open_project`]:
//! inspection must never create a missing database or run a migration.  A
//! caller can therefore use these APIs for `--stats` and `--projects` without
//! changing index or vector-cache contents. `--stats` and `--gc` take the
//! vector mutation lease before observing project references and cached
//! vectors. `--stats` takes a shared snapshot lease so concurrent stats
//! readers do not serialize each other; exclusive publication and `--gc`
//! still wait. `--gc` only creates vector-cache state once a cache already exists.

use std::collections::BTreeSet;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use thiserror::Error;

use crate::secure::appended_name;
use crate::{
    StoreHome, SyncError, SyncMode, SyncReport, VectorCache, VectorError, VectorKey,
    VectorMutationLease, synchronize,
};

const SQLITE_HEADER: &[u8] = b"SQLite format 3\0";
const MAX_QUARANTINES: usize = 20;

/// A validated, read-only view of one Rust project index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexInventory {
    path: PathBuf,
    root: Option<PathBuf>,
    generation: i64,
    pages: usize,
    chunks: usize,
    bytes: u64,
}

impl IndexInventory {
    /// Index database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Root recorded by the index metadata, if present.
    pub fn root(&self) -> Option<&Path> {
        self.root.as_deref()
    }

    /// Last committed synchronization generation.
    pub const fn generation(&self) -> i64 {
        self.generation
    }

    /// Number of indexed pages.
    pub const fn page_count(&self) -> usize {
        self.pages
    }

    /// Number of indexed chunks.
    pub const fn chunk_count(&self) -> usize {
        self.chunks
    }

    /// On-disk database size in bytes.
    pub const fn bytes(&self) -> u64 {
        self.bytes
    }
}

/// A read-only project index handle.
pub struct OpenIndex {
    connection: Connection,
    inventory: IndexInventory,
}

impl OpenIndex {
    /// Borrows the read-only SQLite connection for a retrieval snapshot.
    ///
    /// The connection is owned by this handle and was opened as an ordinary
    /// read-only, `NOFOLLOW`-guarded connection. Project indexes are never
    /// opened with SQLite `immutable=1`: concurrent updates violate that
    /// option's precondition. Callers cannot use this accessor to create or
    /// migrate an index.
    pub fn connection(&self) -> &rusqlite::Connection {
        &self.connection
    }

    /// Starts a transactionally consistent retrieval view over this index.
    pub fn retrieval_snapshot(
        &self,
    ) -> Result<crate::RetrievalSnapshot<'_>, crate::RetrievalError> {
        crate::RetrievalSnapshot::begin(&self.connection)
    }

    /// Metadata view for this handle.
    pub fn inventory(&self) -> &IndexInventory {
        &self.inventory
    }

    /// Recorded project root.
    pub fn root(&self) -> Option<&Path> {
        self.inventory.root()
    }

    /// Last committed generation.
    pub const fn generation(&self) -> i64 {
        self.inventory.generation()
    }

    /// Number of indexed pages.
    pub const fn page_count(&self) -> usize {
        self.inventory.page_count()
    }

    /// Number of indexed chunks.
    pub const fn chunk_count(&self) -> usize {
        self.inventory.chunk_count()
    }

    /// Database path.
    pub fn path(&self) -> &Path {
        self.inventory.path()
    }

    /// Returns every vector key referenced by this index.
    pub fn vector_keys(&self) -> Result<BTreeSet<VectorKey>, ManagementError> {
        let mut statement = self
            .connection
            .prepare("SELECT vector_hash FROM chunks ORDER BY vector_hash")
            .map_err(|source| ManagementError::Database {
                operation: "prepare project vector-key inventory",
                path: self.path().to_path_buf(),
                source,
            })?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|source| ManagementError::Database {
                operation: "query project vector-key inventory",
                path: self.path().to_path_buf(),
                source,
            })?;
        let mut keys = BTreeSet::new();
        for row in rows {
            let value = row.map_err(|source| ManagementError::Database {
                operation: "read project vector-key inventory",
                path: self.path().to_path_buf(),
                source,
            })?;
            let key = value
                .parse()
                .map_err(|source| ManagementError::InvalidVectorKey {
                    path: self.path().to_path_buf(),
                    value,
                    source,
                })?;
            keys.insert(key);
        }
        Ok(keys)
    }
}

/// Convenience alias for [`inventory`], useful to callers that name the
/// operation after its index scope.
pub fn index_inventory(home: &StoreHome) -> Result<Vec<IndexInventory>, ManagementError> {
    inventory(home)
}

/// A project entry in a store inventory.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectRecord {
    /// Root recorded in the project index.
    pub root: PathBuf,
    /// Whether this is the caller-selected project.
    pub current: bool,
    /// Index path.
    pub index: IndexInventory,
}

/// Valid project indexes and paths whose identity could not be established.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectInventory {
    /// Valid indexes with a recorded root.
    pub projects: Vec<ProjectRecord>,
    /// Valid SQLite files with no root metadata.
    pub unrecorded: Vec<PathBuf>,
    /// Paths which may be indexes but could not safely be read.
    pub unreadable: Vec<PathBuf>,
}

/// Selected-project statistics.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Stats {
    /// Project index metadata.
    pub index: IndexInventory,
    /// Shared vector-cache row count, if the cache exists.
    pub vectors: usize,
    /// Vector database bytes, if it exists.
    pub vector_bytes: u64,
}

impl Stats {
    /// Number of indexed pages.
    pub const fn page_count(&self) -> usize {
        self.index.page_count()
    }

    /// Number of indexed chunks.
    pub const fn chunk_count(&self) -> usize {
        self.index.chunk_count()
    }

    /// Last committed generation.
    pub const fn generation(&self) -> i64 {
        self.index.generation()
    }
}

/// Failures from non-search management operations.
#[derive(Debug, Error)]
pub enum ManagementError {
    #[error("index directory is not readable: {path}: {source}")]
    ReadIndexDirectory { path: PathBuf, source: io::Error },

    #[error("index is missing: {path}")]
    MissingIndex { path: PathBuf },

    #[error("index path is not a regular non-symlink file: {path}")]
    UnsafeIndexPath { path: PathBuf },

    #[error("index has SQLite sidecar {path}; refusing to inspect an in-flight database")]
    UnsafeSidecar { path: PathBuf },

    #[error("index is not a SQLite database: {path}")]
    NotSqlite { path: PathBuf },

    #[error("could not {operation} index {path}: {source}")]
    Database {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("index {path} has incompatible or incomplete metadata")]
    InvalidMetadata { path: PathBuf },

    #[error("index {path} contains invalid vector key {value:?}: {source}")]
    InvalidVectorKey {
        path: PathBuf,
        value: String,
        #[source]
        source: crate::VectorKeyParseError,
    },

    #[error("vector cache operation failed: {0}")]
    Vector(#[from] VectorError),

    #[error("synchronization failed: {0}")]
    Sync(#[from] SyncError),

    #[error("{operation} requires complete project inventory; unreadable paths: {paths:?}")]
    IncompleteInventory {
        operation: &'static str,
        paths: Vec<PathBuf>,
    },

    #[error("vector cache is missing: {path}")]
    MissingVectorCache { path: PathBuf },

    #[error("could not inspect vector cache {path}: {source}")]
    VectorDatabase {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("could not quarantine vector cache {path}: {source}")]
    Quarantine { path: PathBuf, source: io::Error },

    #[error("vector cache quarantine limit reached at {path}")]
    QuarantineLimit { path: PathBuf },

    #[error("vector cache is readable and does not need quarantine: {path}")]
    VectorCacheNotCorrupt { path: PathBuf },
}

impl ManagementError {
    /// True when inspection refused an in-flight writer rather than lasting damage.
    pub fn is_transient_contention(&self) -> bool {
        match self {
            Self::UnsafeSidecar { .. } => true,
            Self::Vector(error) => error.is_transient_contention(),
            Self::Sync(error) => error.is_transient_contention(),
            _ => false,
        }
    }
}

/// Opens an existing project index read-only without creating or migrating it.
pub fn open_index(path: &Path) -> Result<OpenIndex, ManagementError> {
    validate_index_path(path)?;
    reject_sidecars(path)?;
    let sqlite_path = resolve_parent_without_following_file(path)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(sqlite_path, flags).map_err(|source| {
        ManagementError::Database {
            operation: "open read-only",
            path: path.to_path_buf(),
            source,
        }
    })?;
    let inventory = read_inventory(&connection, path)?;
    Ok(OpenIndex {
        connection,
        inventory,
    })
}

/// Lists every valid Rust index without creating the indexes directory.
pub fn inventory(home: &StoreHome) -> Result<Vec<IndexInventory>, ManagementError> {
    let paths = index_paths(home)?;
    paths
        .into_iter()
        .map(|path| open_index(&path).map(|index| index.inventory().clone()))
        .collect()
}

/// Lists project indexes and preserves paths whose identity is unknown.
pub fn project_inventory(
    home: &StoreHome,
    current: Option<&Path>,
) -> Result<ProjectInventory, ManagementError> {
    let current = current
        .map(fs::canonicalize)
        .transpose()
        .map_err(|source| ManagementError::ReadIndexDirectory {
            path: current.unwrap().to_path_buf(),
            source,
        })?;
    let mut projects = Vec::new();
    let mut unrecorded = Vec::new();
    let mut unreadable = Vec::new();
    for path in index_paths(home)? {
        match open_index(&path) {
            Ok(index) => {
                if let Some(root) = index.root() {
                    let is_current = current.as_deref() == Some(root);
                    projects.push(ProjectRecord {
                        root: root.to_path_buf(),
                        current: is_current,
                        index: index.inventory().clone(),
                    });
                } else {
                    unrecorded.push(path);
                }
            }
            Err(_) => unreadable.push(path),
        }
    }
    projects.sort_by(|left, right| left.root.cmp(&right.root));
    unrecorded.sort();
    unreadable.sort();
    Ok(ProjectInventory {
        projects,
        unrecorded,
        unreadable,
    })
}

/// Returns selected-project counts and vector-cache size without changing the index.
pub fn stats(home: &StoreHome, root: &Path) -> Result<Stats, ManagementError> {
    let lease = VectorMutationLease::acquire_shared(home)?;
    stats_under_lease(home, root, lease)
}

/// Reads one selected-project and vector-cache snapshot under an owned lease.
pub(crate) fn stats_under_lease(
    home: &StoreHome,
    root: &Path,
    lease: VectorMutationLease,
) -> Result<Stats, ManagementError> {
    lease.validate_for(home)?;
    let path = home
        .project_path(root)
        .map_err(|source| ManagementError::Sync(SyncError::Store(source)))?;
    let index = open_index(&path)?;
    let (vectors, vector_bytes) = if home.vectors_path().exists() {
        let connection = open_vectors_read_only(&home.vectors_path())?;
        let count = connection
            .query_row("SELECT count(*) FROM vectors", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|source| ManagementError::VectorDatabase {
                path: home.vectors_path(),
                source,
            })?;
        let bytes = fs::metadata(home.vectors_path())
            .map_err(|source| ManagementError::ReadIndexDirectory {
                path: home.vectors_path(),
                source,
            })?
            .len();
        (usize::try_from(count).unwrap_or(usize::MAX), bytes)
    } else {
        (0, 0)
    };
    let stats = Stats {
        index: index.inventory().clone(),
        vectors,
        vector_bytes,
    };
    drop(lease);
    Ok(stats)
}

/// Rebuilds one selected project through the atomic full-rebuild sync path.
pub fn reindex(
    home: &StoreHome,
    root: &Path,
    scan: &yams_core::ScanReport,
    embedder: &mut dyn yams_embed::Embedder,
) -> Result<SyncReport, ManagementError> {
    synchronize(home, root, scan, embedder, SyncMode::FullRebuild).map_err(ManagementError::from)
}

/// Conservatively removes only vectors absent from both live-key snapshots.
///
/// The lease opens the collection epoch: the initial cache snapshot and both
/// project enumerations are read inside it, so no index a cooperating
/// synchronizer publishes can be invisible to the sweep it precedes.
///
/// Returns an empty report without creating store state when no vector cache
/// exists.  A store with zero readable indexes sweeps every cached vector:
/// after two complete enumerations, an empty live set is authoritative.
pub fn gc(home: &StoreHome) -> Result<crate::SweepReport, ManagementError> {
    // An absent cache has nothing to sweep, so this refusal to create one is
    // free of race consequences in either direction: a cache created after the
    // check is simply left to the next run, and one removed after it reaches
    // cached_keys' own existence re-check, which yields an empty initial
    // snapshot.  No key is ever eligible on the strength of this check alone.
    if !home.vectors_path().exists() {
        return Ok(crate::SweepReport::default());
    }
    let lease = VectorMutationLease::acquire(home)?;
    gc_under_lease(home, lease)
}

/// Runs one complete collection epoch under an already-acquired lease.
pub(crate) fn gc_under_lease(
    home: &StoreHome,
    lease: VectorMutationLease,
) -> Result<crate::SweepReport, ManagementError> {
    lease.validate_for(home)?;
    let initial = match cached_keys(home) {
        Ok(keys) => keys,
        Err(error) => {
            // The cache is derivable from project pages.  Preserve the
            // damaged bytes before creating a replacement, then continue
            // with an empty initial set; no old row is eligible for deletion.
            // A failed preservation is reported as itself: masking it hides
            // that the damaged cache is still in place. A cache that passes
            // quarantine's readability probe needs no preservation, so its
            // original read failure stands.
            match quarantine_vectors(home) {
                // The destination is deliberately not reported: the sweep
                // report has no field for it yet.
                Ok(_quarantined) => BTreeSet::new(),
                Err(ManagementError::VectorCacheNotCorrupt { .. }) => return Err(error),
                Err(quarantine) => return Err(quarantine),
            }
        }
    };
    // The first inventory re-enumerates the index directory after acquiring
    // the lease. This fixes the stale pre-lease path snapshot covered by the
    // lease-boundary regression below.
    let first = live_keys(home)?;
    // The second inventory independently re-enumerates under the continuously
    // held lease. No cooperating synchronizer can publish between the passes;
    // this is a conservative guard against a non-cooperating writer that does.
    // Deterministically killing this pass would require a test-only GC hook
    // between inventories to publish without the lease; GC has no such seam.
    let second = live_keys(home)?;
    // Zero indexes after two complete enumerations means every initially
    // cached key is an orphan; sweeping them is the intended result.
    let live = first.union(&second).copied().collect::<BTreeSet<_>>();
    let mut cache = VectorCache::open(home)?;
    cache
        .sweep_snapshot(&lease, &initial, &live)
        .map_err(ManagementError::from)
}

/// Moves a corrupt existing vector cache aside, preserving all bytes.
pub fn quarantine_vectors(home: &StoreHome) -> Result<PathBuf, ManagementError> {
    let path = home.vectors_path();
    validate_index_path(&path).map_err(|error| match error {
        ManagementError::MissingIndex { .. } => {
            ManagementError::MissingVectorCache { path: path.clone() }
        }
        other => other,
    })?;
    if open_vectors_read_only(&path)
        .and_then(|connection| {
            connection
                .query_row("SELECT count(*) FROM vectors", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(|source| ManagementError::VectorDatabase {
                    path: path.clone(),
                    source,
                })
        })
        .is_ok()
    {
        return Err(ManagementError::VectorCacheNotCorrupt { path });
    }
    for ordinal in 0..MAX_QUARANTINES {
        let suffix = if ordinal == 0 {
            ".corrupt".to_owned()
        } else {
            format!(".corrupt-{}", ordinal + 1)
        };
        let destination = PathBuf::from(appended_name(path.as_os_str(), &suffix));
        match yams_fs::rename_exclusive(&path, &destination) {
            Ok(()) => {}
            Err(yams_fs::RenameError::AlreadyExists { .. }) => continue,
            Err(yams_fs::RenameError::Io { source, .. }) => {
                return Err(ManagementError::Quarantine {
                    path: path.clone(),
                    source,
                });
            }
            Err(error) => {
                return Err(ManagementError::Quarantine {
                    path: path.clone(),
                    source: io::Error::new(io::ErrorKind::InvalidInput, error),
                });
            }
        }
        for suffix in ["-wal", "-shm", "-journal"] {
            let sidecar = PathBuf::from(appended_name(path.as_os_str(), suffix));
            if sidecar.exists() {
                let moved = PathBuf::from(appended_name(destination.as_os_str(), suffix));
                fs::rename(&sidecar, moved).map_err(|source| ManagementError::Quarantine {
                    path: sidecar,
                    source,
                })?;
            }
        }
        return Ok(destination);
    }
    Err(ManagementError::QuarantineLimit { path })
}

fn index_paths(home: &StoreHome) -> Result<Vec<PathBuf>, ManagementError> {
    let directory = home.indexes_dir();
    let entries = match fs::read_dir(&directory) {
        Ok(entries) => entries,
        Err(source) if source.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(source) => {
            return Err(ManagementError::ReadIndexDirectory {
                path: directory,
                source,
            });
        }
    };
    let mut paths = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ManagementError::ReadIndexDirectory {
            path: home.indexes_dir(),
            source,
        })?;
        let path = entry.path();
        if path.extension().is_some_and(|value| value == "sqlite3") {
            paths.push(path);
        }
    }
    paths.sort();
    Ok(paths)
}

fn validate_index_path(path: &Path) -> Result<(), ManagementError> {
    let metadata = fs::symlink_metadata(path).map_err(|source| {
        if source.kind() == io::ErrorKind::NotFound {
            ManagementError::MissingIndex {
                path: path.to_path_buf(),
            }
        } else {
            ManagementError::UnsafeIndexPath {
                path: path.to_path_buf(),
            }
        }
    })?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(ManagementError::UnsafeIndexPath {
            path: path.to_path_buf(),
        });
    }
    let mut header = [0; SQLITE_HEADER.len()];
    let mut file = fs::File::open(path).map_err(|_| ManagementError::UnsafeIndexPath {
        path: path.to_path_buf(),
    })?;
    use std::io::Read;
    let length = file
        .read(&mut header)
        .map_err(|_| ManagementError::UnsafeIndexPath {
            path: path.to_path_buf(),
        })?;
    if length == SQLITE_HEADER.len() && header != SQLITE_HEADER {
        return Err(ManagementError::NotSqlite {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn resolve_parent_without_following_file(path: &Path) -> Result<PathBuf, ManagementError> {
    let parent = path
        .parent()
        .ok_or_else(|| ManagementError::UnsafeIndexPath {
            path: path.to_path_buf(),
        })?;
    let parent = if parent.as_os_str().is_empty() {
        Path::new(".")
    } else {
        parent
    };
    let parent = fs::canonicalize(parent).map_err(|_| ManagementError::UnsafeIndexPath {
        path: path.to_path_buf(),
    })?;
    let name = path
        .file_name()
        .ok_or_else(|| ManagementError::UnsafeIndexPath {
            path: path.to_path_buf(),
        })?;
    Ok(parent.join(name))
}

fn reject_sidecars(path: &Path) -> Result<(), ManagementError> {
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = PathBuf::from(appended_name(path.as_os_str(), suffix));
        if sidecar.exists() {
            return Err(ManagementError::UnsafeSidecar { path: sidecar });
        }
    }
    Ok(())
}

fn read_inventory(connection: &Connection, path: &Path) -> Result<IndexInventory, ManagementError> {
    let root = connection
        .query_row("SELECT root FROM metadata WHERE singleton = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .map_err(|source| ManagementError::Database {
            operation: "read project root metadata",
            path: path.to_path_buf(),
            source,
        })?
        .map(PathBuf::from);
    let generation = connection
        .query_row(
            "SELECT generation FROM metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|source| ManagementError::Database {
            operation: "read project generation metadata",
            path: path.to_path_buf(),
            source,
        })?
        .ok_or_else(|| ManagementError::InvalidMetadata {
            path: path.to_path_buf(),
        })?;
    let pages = count(connection, "docs", path)?;
    let chunks = count(connection, "chunks", path)?;
    let bytes = fs::metadata(path)
        .map_err(|source| ManagementError::ReadIndexDirectory {
            path: path.to_path_buf(),
            source,
        })?
        .len();
    Ok(IndexInventory {
        path: path.to_path_buf(),
        root,
        generation,
        pages,
        chunks,
        bytes,
    })
}

fn count(connection: &Connection, table: &str, path: &Path) -> Result<usize, ManagementError> {
    let sql = format!("SELECT count(*) FROM {table}");
    let count = connection
        .query_row(&sql, [], |row| row.get::<_, i64>(0))
        .map_err(|source| ManagementError::Database {
            operation: "read project row count",
            path: path.to_path_buf(),
            source,
        })?;
    usize::try_from(count).map_err(|_| ManagementError::InvalidMetadata {
        path: path.to_path_buf(),
    })
}

/// Reads every referenced key from a freshly enumerated index directory,
/// refusing to build a sweep decision on a partial inventory.
fn live_keys(home: &StoreHome) -> Result<BTreeSet<VectorKey>, ManagementError> {
    let mut keys = BTreeSet::new();
    let mut unreadable = Vec::new();
    for path in index_paths(home)? {
        match open_index(&path) {
            Ok(index) => keys.extend(index.vector_keys()?),
            Err(ManagementError::NotSqlite { .. }) => {}
            Err(_) => unreadable.push(path),
        }
    }
    if unreadable.is_empty() {
        Ok(keys)
    } else {
        Err(ManagementError::IncompleteInventory {
            operation: "garbage collection",
            paths: unreadable,
        })
    }
}

fn cached_keys(home: &StoreHome) -> Result<BTreeSet<VectorKey>, ManagementError> {
    if !home.vectors_path().exists() {
        return Ok(BTreeSet::new());
    }
    let connection = open_vectors_read_only(&home.vectors_path())?;
    let mut statement = connection
        .prepare("SELECT hash FROM vectors ORDER BY hash")
        .map_err(|source| ManagementError::VectorDatabase {
            path: home.vectors_path(),
            source,
        })?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| ManagementError::VectorDatabase {
            path: home.vectors_path(),
            source,
        })?;
    let mut keys = BTreeSet::new();
    for row in rows {
        let value = row.map_err(|source| ManagementError::VectorDatabase {
            path: home.vectors_path(),
            source,
        })?;
        keys.insert(
            value
                .parse()
                .map_err(|source| ManagementError::InvalidVectorKey {
                    path: home.vectors_path(),
                    value,
                    source,
                })?,
        );
    }
    Ok(keys)
}

fn open_vectors_read_only(path: &Path) -> Result<Connection, ManagementError> {
    validate_index_path(path).map_err(|error| match error {
        ManagementError::MissingIndex { .. } => ManagementError::MissingVectorCache {
            path: path.to_path_buf(),
        },
        other => other,
    })?;
    let sqlite_path = resolve_parent_without_following_file(path)?;
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let connection = Connection::open_with_flags(sqlite_path, flags).map_err(|source| {
        ManagementError::VectorDatabase {
            path: path.to_path_buf(),
            source,
        }
    })?;
    Ok(connection)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::{Path, PathBuf};

    use yams_core::{Corpus, CorpusKind, scan_corpora};
    use yams_embed::FakeEmbedder;

    use super::{gc_under_lease, open_index, open_vectors_read_only, stats, stats_under_lease};
    use crate::{
        ManagementError, StoreError, StoreHome, SyncMode, VectorCache, VectorError, VectorKey,
        VectorMutationLease, synchronize,
    };

    fn write_project(directory: &Path, name: &str, body: &str) -> PathBuf {
        let root = directory.join(name);
        std::fs::create_dir_all(root.join(".agents/memory")).unwrap();
        std::fs::write(
            root.join(".agents/memory/alpha.md"),
            format!("---\ntitle: Alpha\n---\n\n{body}\n"),
        )
        .unwrap();
        root
    }

    fn sync_project(home: &StoreHome, root: &Path) {
        let corpus = Corpus::validated(&root.join(".agents/memory"), CorpusKind::Shared).unwrap();
        let scan = scan_corpora(&[corpus]);
        synchronize(
            home,
            root,
            &scan,
            &mut FakeEmbedder::new(),
            SyncMode::Incremental,
        )
        .unwrap();
    }

    fn index_vector_keys(path: &Path) -> BTreeSet<VectorKey> {
        open_index(path).unwrap().vector_keys().unwrap()
    }

    #[test]
    fn gc_refreshes_index_paths_after_synchronous_lease_contention() {
        let directory = tempfile::tempdir().unwrap();
        let home = StoreHome::new(directory.path().join("state"));
        let keeper = write_project(directory.path(), "keeper", "beta token");
        let latecomer = write_project(directory.path(), "latecomer", "alpha token");
        sync_project(&home, &keeper);
        sync_project(&home, &latecomer);
        let published = home.project_path(&latecomer).unwrap();
        let staged = directory.path().join("staged.sqlite3");
        std::fs::rename(&published, &staged).unwrap();
        let referenced = index_vector_keys(&staged);
        assert_eq!(referenced.len(), 1);
        assert!(
            index_vector_keys(&home.project_path(&keeper).unwrap()).is_disjoint(&referenced),
            "the keeper must not reference the held-back key, or the sweep has nothing to lose"
        );

        let blocker = VectorMutationLease::acquire(&home).unwrap();
        let contention = VectorMutationLease::acquire_without_waiting(&home).unwrap_err();
        assert!(
            matches!(
                contention,
                VectorError::Store(StoreError::Busy {
                    operation: "coordinate vector references and garbage collection",
                    ..
                })
            ),
            "the zero-timeout probe must observe the held GC lease"
        );

        // Publish while the lease is demonstrably unavailable. The old code
        // captured paths before acquisition; gc_under_lease can only run after.
        std::fs::rename(&staged, &published).unwrap();
        drop(blocker);
        let lease = VectorMutationLease::acquire(&home).unwrap();
        let report = gc_under_lease(&home, lease).unwrap();

        let cache = VectorCache::open(&home).unwrap();
        assert_eq!(cache.get_many(&referenced).unwrap().len(), referenced.len());
        assert_eq!(report.removed, 0);
    }

    #[test]
    fn gc_under_lease_rejects_another_home_before_inspecting_its_cache() {
        let directory = tempfile::tempdir().unwrap();
        let lease_home = StoreHome::new(directory.path().join("lease-home"));
        let target_home = StoreHome::new(directory.path().join("target-home"));
        std::fs::create_dir_all(target_home.vectors_path().parent().unwrap()).unwrap();
        std::fs::write(target_home.vectors_path(), b"damaged cache").unwrap();
        let before = std::fs::read(target_home.vectors_path()).unwrap();
        let lease = VectorMutationLease::acquire(&lease_home).unwrap();
        let expected_cache = target_home.vectors_path().canonicalize().unwrap();
        let expected_lease = lease_home
            .vectors_path()
            .parent()
            .unwrap()
            .canonicalize()
            .unwrap()
            .join("vectors.sqlite3");

        let error = gc_under_lease(&target_home, lease).unwrap_err();

        match error {
            ManagementError::Vector(VectorError::WrongMutationLease { cache, lease }) => {
                assert_eq!(cache, expected_cache);
                assert_eq!(lease, expected_lease);
            }
            other => panic!("expected a mismatched mutation lease, got {other:?}"),
        }
        assert_eq!(std::fs::read(target_home.vectors_path()).unwrap(), before);
        assert!(
            !PathBuf::from(crate::secure::appended_name(
                target_home.vectors_path().as_os_str(),
                ".corrupt"
            ))
            .exists()
        );
    }

    #[test]
    fn stats_read_body_requires_an_owned_mutation_lease() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let home = StoreHome::new(directory.join("state"));
        let root = write_project(&directory, "project", "alpha token");
        sync_project(&home, &root);

        let blocker = VectorMutationLease::acquire(&home).unwrap();
        let contention = VectorMutationLease::acquire_without_waiting(&home).unwrap_err();
        assert!(
            matches!(
                contention,
                VectorError::Store(StoreError::Busy {
                    operation: "coordinate vector references and garbage collection",
                    ..
                })
            ),
            "the zero-timeout probe must keep stats outside its lease-bound read body"
        );

        drop(blocker);
        let lease = VectorMutationLease::acquire(&home).unwrap();
        let result = stats_under_lease(&home, &root, lease).unwrap();
        assert_eq!(result.index.page_count(), 1);
        assert_eq!(result.index.chunk_count(), 1);
        assert_eq!(result.vectors, 1);
        assert!(result.vector_bytes > 0);

        let ordinary = stats(&home, &root).unwrap();
        assert_eq!(ordinary.index.page_count(), 1);
        assert_eq!(ordinary.index.chunk_count(), 1);
        assert_eq!(ordinary.vectors, 1);
        assert!(ordinary.vector_bytes > 0);
    }

    #[test]
    fn stats_shared_lease_does_not_block_another_reader() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let home = StoreHome::new(directory.join("state"));
        let root = write_project(&directory, "project", "alpha token");
        sync_project(&home, &root);

        let first = VectorMutationLease::acquire_shared(&home).unwrap();
        let second = VectorMutationLease::acquire_shared_without_waiting(&home).unwrap();
        drop(second);
        drop(first);
        assert_eq!(stats(&home, &root).unwrap().index.page_count(), 1);
    }

    #[test]
    fn stats_shared_lease_blocks_exclusive_publication() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let home = StoreHome::new(directory.join("state"));
        let _root = write_project(&directory, "project", "alpha token");

        let reader = VectorMutationLease::acquire_shared(&home).unwrap();
        let contention = VectorMutationLease::acquire_without_waiting(&home).unwrap_err();
        assert!(
            matches!(
                contention,
                VectorError::Store(StoreError::Busy {
                    operation: "coordinate vector references and garbage collection",
                    ..
                })
            ),
            "{contention:?}"
        );
        drop(reader);
    }

    #[test]
    fn stats_under_lease_validates_home_before_resolving_the_project() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let lease_home = StoreHome::new(directory.join("lease-home"));
        let target_home = StoreHome::new(directory.join("target-home"));
        std::fs::create_dir_all(target_home.vectors_path().parent().unwrap()).unwrap();
        let missing_root = directory.join("missing-project");
        let lease = VectorMutationLease::acquire(&lease_home).unwrap();

        let error = stats_under_lease(&target_home, &missing_root, lease).unwrap_err();

        assert!(matches!(
            error,
            ManagementError::Vector(VectorError::WrongMutationLease { .. })
        ));
    }

    #[test]
    fn vector_read_connection_observes_later_commits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("vectors.sqlite3");
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "CREATE TABLE vectors (hash TEXT PRIMARY KEY);\n\
                 INSERT INTO vectors (hash) VALUES ('first');",
            )
            .unwrap();

        let reader = open_vectors_read_only(&path).unwrap();
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM vectors", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );

        writer
            .execute("INSERT INTO vectors (hash) VALUES ('second')", [])
            .unwrap();

        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM vectors", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            2,
            "an ordinary read-only connection must not retain immutable SQLite pages"
        );
    }

    #[test]
    fn vector_read_preserves_a_live_preexisting_empty_wal_pair() {
        let directory = tempfile::tempdir().unwrap();
        let directory = directory.path().canonicalize().unwrap();
        let home = StoreHome::new(directory.join("state"));
        let _lease = VectorMutationLease::acquire(&home).unwrap();
        let path = home.vectors_path();
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch(
                "PRAGMA journal_mode = WAL;\n\
                 CREATE TABLE vectors (hash TEXT PRIMARY KEY);\n\
                 INSERT INTO vectors (hash) VALUES ('first');\n\
                 PRAGMA wal_checkpoint(TRUNCATE);",
            )
            .unwrap();
        let wal = PathBuf::from(crate::secure::appended_name(path.as_os_str(), "-wal"));
        let shm = PathBuf::from(crate::secure::appended_name(path.as_os_str(), "-shm"));
        assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
        assert!(shm.exists());

        let reader = open_vectors_read_only(&path).unwrap();
        assert_eq!(
            reader
                .query_row("SELECT count(*) FROM vectors", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(reader);

        assert!(wal.exists());
        assert!(shm.exists());
        drop(writer);
    }

    #[test]
    fn vector_read_preserves_sidecars_replaced_during_its_lifetime() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("vectors.sqlite3");
        let writer = rusqlite::Connection::open(&path).unwrap();
        writer
            .execute_batch("CREATE TABLE vectors (hash TEXT PRIMARY KEY);")
            .unwrap();
        drop(writer);

        let reader = open_vectors_read_only(&path).unwrap();
        let wal = PathBuf::from(crate::secure::appended_name(path.as_os_str(), "-wal"));
        let shm = PathBuf::from(crate::secure::appended_name(path.as_os_str(), "-shm"));
        std::fs::write(&wal, b"replacement wal").unwrap();
        std::fs::write(&shm, b"replacement shm").unwrap();
        drop(reader);

        assert_eq!(std::fs::read(wal).unwrap(), b"replacement wal");
        assert_eq!(std::fs::read(shm).unwrap(), b"replacement shm");
    }

    #[test]
    fn in_flight_sidecar_is_transient_contention_and_missing_index_is_not() {
        let sidecar = ManagementError::UnsafeSidecar {
            path: PathBuf::from("/tmp/index.sqlite3-journal"),
        };
        assert!(sidecar.is_transient_contention());
        assert!(
            ManagementError::Vector(VectorError::Store(StoreError::Integrity {
                path: PathBuf::from("/tmp/vectors.sqlite3"),
                detail: "row missing from index".into(),
            }))
            .is_transient_contention()
        );
        assert!(
            !ManagementError::MissingIndex {
                path: PathBuf::from("/tmp/index.sqlite3"),
            }
            .is_transient_contention()
        );
        assert!(
            !ManagementError::InvalidMetadata {
                path: PathBuf::from("/tmp/index.sqlite3"),
            }
            .is_transient_contention()
        );
    }
}
