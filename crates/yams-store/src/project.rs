use std::fmt;
use std::fs;
use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};
use rustix::fs::{self as rfs, FileType, Mode, OFlags};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::home::StoreHome;
use crate::schema::{
    MIGRATABLE_SCHEMA_VERSION, SCHEMA_VERSION, create_project_schema, has_persistent_schema,
    migrate_project_schema_v1_to_v2_with_hook, project_schema_is_current,
    project_schema_matches_version,
};
use crate::secure::{DatabaseState, NoHooks, OpenHooks, SecureStoreDirectory, immutable_uri};

const PROJECT_DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PathKind {
    Project,
    Corpus,
}

/// The embedding identity stamped into one project index.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct EmbeddingScheme {
    signature: String,
    dimensions: usize,
}

impl EmbeddingScheme {
    /// Constructs a scheme from its lowercase 64-hex signature and dimension.
    pub fn new(
        signature: impl Into<String>,
        dimensions: usize,
    ) -> Result<Self, EmbeddingSchemeError> {
        let signature = signature.into();
        if signature.len() != 64
            || !signature
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(EmbeddingSchemeError::InvalidSignature);
        }
        if dimensions == 0 || i64::try_from(dimensions).is_err() {
            return Err(EmbeddingSchemeError::InvalidDimensions);
        }
        Ok(Self {
            signature,
            dimensions,
        })
    }

    pub fn signature(&self) -> &str {
        &self.signature
    }

    pub const fn dimensions(&self) -> usize {
        self.dimensions
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum EmbeddingSchemeError {
    #[error("embedding scheme signature must be exactly 64 lowercase hexadecimal characters")]
    InvalidSignature,

    #[error("embedding scheme dimensions must be positive")]
    InvalidDimensions,

    #[error("embedding scheme storage must be empty or contain only singleton row 1")]
    InvalidStorageShape,
}

/// Reads the optional embedding scheme stamped into an open project index.
pub fn read_embedding_scheme(
    connection: &Connection,
) -> Result<Option<EmbeddingScheme>, rusqlite::Error> {
    let row = connection
        .query_row(
            "SELECT singleton, signature, dimensions, \
                    (SELECT count(*) FROM embedding_scheme) \
             FROM embedding_scheme LIMIT 1",
            [],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            },
        )
        .optional()?;
    row.map(|(singleton, signature, dimensions, rows)| {
        if singleton != 1 || rows != 1 {
            return Err(rusqlite::Error::FromSqlConversionFailure(
                0,
                rusqlite::types::Type::Integer,
                Box::new(EmbeddingSchemeError::InvalidStorageShape),
            ));
        }
        let dimensions = usize::try_from(dimensions).map_err(|source| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Integer,
                Box::new(source),
            )
        })?;
        EmbeddingScheme::new(signature, dimensions).map_err(|source| {
            let (column, data_type) = match source {
                EmbeddingSchemeError::InvalidSignature => (1, rusqlite::types::Type::Text),
                EmbeddingSchemeError::InvalidDimensions => (2, rusqlite::types::Type::Integer),
                EmbeddingSchemeError::InvalidStorageShape => (0, rusqlite::types::Type::Integer),
            };
            rusqlite::Error::FromSqlConversionFailure(column, data_type, Box::new(source))
        })
    })
    .transpose()
}

/// Sets or clears the project embedding stamp inside the caller's transaction.
pub fn write_embedding_scheme(
    transaction: &Transaction<'_>,
    scheme: Option<&EmbeddingScheme>,
) -> Result<(), rusqlite::Error> {
    match scheme {
        Some(scheme) => {
            let dimensions = i64::try_from(scheme.dimensions())
                .expect("EmbeddingScheme construction checked SQLite integer range");
            transaction.execute(
                "INSERT INTO embedding_scheme(singleton, signature, dimensions) \
                 VALUES (1, ?1, ?2) \
                 ON CONFLICT(singleton) DO UPDATE SET \
                 signature = excluded.signature, dimensions = excluded.dimensions",
                params![scheme.signature(), dimensions],
            )?;
        }
        None => {
            transaction.execute("DELETE FROM embedding_scheme WHERE singleton = 1", [])?;
        }
    }
    Ok(())
}

impl fmt::Display for PathKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Project => formatter.write_str("project"),
            Self::Corpus => formatter.write_str("corpus"),
        }
    }
}

#[derive(Debug, Error)]
pub enum StoreError {
    #[error("could not resolve {kind} path {path}: {source}")]
    Canonicalize {
        kind: PathKind,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("{kind} path is not valid UTF-8: {path:?}")]
    NonUtf8Path { kind: PathKind, path: PathBuf },

    #[error("{kind} path is not a directory: {path}")]
    NotDirectory { kind: PathKind, path: PathBuf },

    #[error("could not create store directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("could not {operation} at {path}: {source}")]
    InspectPath {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },

    #[error("store path is not a regular file: {path}")]
    NotRegular { path: PathBuf },

    #[error("unsafe private store directory {path}: {reason}")]
    UnsafeStoreDirectory { path: PathBuf, reason: String },

    #[error("unsafe private store file {path}: {reason}")]
    UnsafeStoreFile { path: PathBuf, reason: String },

    #[error(
        "SQLite sidecar {path} is present; a live or crashed store must be closed/recovered before this conservative opener can inspect it"
    )]
    UnsafeSidecar { path: PathBuf },

    #[error("store path changed while it was being opened: {path}")]
    RacedStorePath { path: PathBuf },

    #[error("project root changed while it was being used: {path}")]
    RacedProjectRoot { path: PathBuf },

    #[error("store is busy while trying to {operation}: {path}")]
    Busy {
        operation: &'static str,
        path: PathBuf,
    },

    #[error("could not {operation} SQLite database {path}: {source}")]
    Database {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("project store belongs to {found}, not {expected}")]
    WrongRoot { expected: PathBuf, found: PathBuf },

    #[error("project store schema {found} is newer than supported schema {supported}")]
    FutureSchema { found: u64, supported: u32 },

    #[error("project store schema {found} is not supported by schema {supported}")]
    UnsupportedSchema { found: i64, supported: u32 },

    #[error("project store at {path} has no valid Rust metadata")]
    IncompatibleSchema { path: PathBuf },

    #[error("vector store at {path} does not have the rust-v1 schema")]
    IncompatibleVectorSchema { path: PathBuf },

    #[error("SQLite integrity validation failed for {path}: {detail}")]
    Integrity { path: PathBuf, detail: String },

    #[error("SQLite store at {path} requires journal mode {expected}, found {found}")]
    UnexpectedJournalMode {
        path: PathBuf,
        expected: &'static str,
        found: String,
    },

    #[error(transparent)]
    Sql(#[from] rusqlite::Error),
}

impl StoreError {
    /// True when a conservative reader refused a store a concurrent writer may
    /// still be mutating. Retrying the same open can succeed; a refusal that
    /// persists is still a hard failure.
    pub fn is_transient_contention(&self) -> bool {
        matches!(
            self,
            Self::UnsafeSidecar { .. }
                | Self::Integrity { .. }
                | Self::Busy { .. }
                | Self::RacedStorePath { .. }
                | Self::RacedProjectRoot { .. }
        )
    }
}

#[derive(Debug)]
struct ProjectIdentity {
    canonical_root: PathBuf,
    root_text: String,
    filename: String,
}

impl ProjectIdentity {
    fn resolve(root: &Path) -> Result<Self, StoreError> {
        // Check the caller spelling before touching the filesystem. Some
        // supported filesystems refuse non-UTF-8 names during lookup, but the
        // store contract still owes the caller the stable path-kind error.
        path_as_utf8(root, PathKind::Project)?;
        let canonical_root = fs::canonicalize(root).map_err(|source| StoreError::Canonicalize {
            kind: PathKind::Project,
            path: root.to_path_buf(),
            source,
        })?;
        let root_metadata =
            fs::metadata(&canonical_root).map_err(|source| StoreError::Canonicalize {
                kind: PathKind::Project,
                path: canonical_root.clone(),
                source,
            })?;
        if !root_metadata.is_dir() {
            return Err(StoreError::NotDirectory {
                kind: PathKind::Project,
                path: canonical_root,
            });
        }
        let root_text = path_as_utf8(&canonical_root, PathKind::Project)?.to_owned();
        let readable = canonical_root
            .file_name()
            .and_then(|name| name.to_str())
            .map(sanitize_basename)
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| "project".to_owned());
        let hash = format!("{:x}", Sha256::digest(root_text.as_bytes()));
        let filename = format!("{readable}-{}.sqlite3", &hash[..16]);
        Ok(Self {
            canonical_root,
            root_text,
            filename,
        })
    }
}

pub(crate) struct ProjectRootBinding {
    canonical_root: PathBuf,
    fd: OwnedFd,
    device: u64,
    inode: u64,
}

impl ProjectRootBinding {
    fn capture(canonical_root: PathBuf) -> Result<Self, StoreError> {
        let fd = rfs::open(&canonical_root, PROJECT_DIRECTORY_FLAGS, Mode::empty()).map_err(
            |error| StoreError::InspectPath {
                operation: "open canonical project root without following links",
                path: canonical_root.clone(),
                source: io::Error::from_raw_os_error(error.raw_os_error()),
            },
        )?;
        let stat = rfs::fstat(&fd).map_err(|error| StoreError::InspectPath {
            operation: "inspect canonical project root descriptor",
            path: canonical_root.clone(),
            source: io::Error::from_raw_os_error(error.raw_os_error()),
        })?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Err(StoreError::NotDirectory {
                kind: PathKind::Project,
                path: canonical_root,
            });
        }
        #[allow(clippy::unnecessary_cast)]
        let binding = Self {
            canonical_root,
            fd,
            device: stat.st_dev as u64,
            inode: stat.st_ino as u64,
        };
        if !binding.revalidate() {
            return Err(StoreError::RacedProjectRoot {
                path: binding.canonical_root,
            });
        }
        Ok(binding)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.canonical_root
    }

    pub(crate) fn revalidate(&self) -> bool {
        let Ok(held) = rfs::fstat(&self.fd) else {
            return false;
        };
        let Ok(named) = rfs::open(&self.canonical_root, PROJECT_DIRECTORY_FLAGS, Mode::empty())
        else {
            return false;
        };
        let Ok(named) = rfs::fstat(&named) else {
            return false;
        };
        #[allow(clippy::unnecessary_cast)]
        {
            held.st_dev as u64 == self.device
                && held.st_ino as u64 == self.inode
                && named.st_dev as u64 == self.device
                && named.st_ino as u64 == self.inode
                && FileType::from_raw_mode(held.st_mode) == FileType::Directory
                && FileType::from_raw_mode(named.st_mode) == FileType::Directory
        }
    }
}

pub(crate) fn bind_project_root(
    home: &StoreHome,
    root: &Path,
) -> Result<(PathBuf, ProjectRootBinding), StoreError> {
    let identity = ProjectIdentity::resolve(root)?;
    let path = home.indexes_dir().join(&identity.filename);
    let binding = ProjectRootBinding::capture(identity.canonical_root)?;
    Ok((path, binding))
}

impl StoreHome {
    pub fn project_path(&self, root: &Path) -> Result<PathBuf, StoreError> {
        let identity = ProjectIdentity::resolve(root)?;
        Ok(self.indexes_dir().join(identity.filename))
    }
}

pub fn path_as_utf8(path: &Path, kind: PathKind) -> Result<&str, StoreError> {
    path.to_str().ok_or_else(|| StoreError::NonUtf8Path {
        kind,
        path: path.to_path_buf(),
    })
}

/// Opens the project index inside an owner-private, descriptor-pinned store.
///
/// Symlinks, foreign ownership, hard links, public modes, and existing SQLite
/// sidecars are refused before SQLite inspects the database. The 0700 store
/// directories exclude other OS users when the canonical StoreHome base is in
/// a hierarchy they cannot rename. A malicious process with the same effective
/// user ID remains trusted; same-UID callers must serialize external path or
/// database mutation during validation. A peer can race after the final
/// identity check, so this API does not claim a same-user adversary guarantee.
pub fn open_project(home: &StoreHome, root: &Path) -> Result<Connection, StoreError> {
    open_project_with_hooks(home, root, &mut NoHooks)
}

fn open_project_with_hooks(
    home: &StoreHome,
    root: &Path,
    hooks: &mut dyn OpenHooks,
) -> Result<Connection, StoreError> {
    let identity = ProjectIdentity::resolve(root)?;
    let directory = SecureStoreDirectory::for_project(home)?;
    let database = directory.prepare_database_without_sidecar_check(identity.filename.as_ref())?;
    let path = database.path().to_path_buf();

    hooks.before_sqlite_open(&path);
    let project_lock = directory.lock_project_indexes(&path)?;
    directory.revalidate()?;
    database.revalidate(&directory)?;
    directory.refuse_sidecars(database.name())?;
    if database.state() == DatabaseState::Existing {
        let inspection = open_immutable(&path, "open project store for immutable inspection")?;
        validate_project_database(&inspection, &path, &identity)?;
        drop(inspection);
        validate_project_journal_header(&database)?;
        database.revalidate(&directory)?;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut connection =
        Connection::open_with_flags(&path, flags).map_err(|source| StoreError::Database {
            operation: "open project store read-write without following links",
            path: path.clone(),
            source,
        })?;
    hooks.after_sqlite_open(&path);
    database.revalidate(&directory)?;
    connection
        .pragma_update(None, "foreign_keys", "ON")
        .map_err(|source| StoreError::Database {
            operation: "enable project foreign keys",
            path: path.clone(),
            source,
        })?;

    let mut needs_post_commit_validation = database.state() == DatabaseState::Empty;
    {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "begin immediate project-store transaction",
                path: path.clone(),
                source,
            })?;
        let metadata_exists =
            metadata_table_exists(&transaction).map_err(|source| StoreError::Database {
                operation: "recheck project schema state inside immediate transaction",
                path: path.clone(),
                source,
            })?;
        if metadata_exists {
            let version = validate_project_structure(&transaction, &path, &identity)?;
            if version == MIGRATABLE_SCHEMA_VERSION {
                validate_project_database(&transaction, &path, &identity)?;
                migrate_project_schema_v1_to_v2_with_hook(&transaction, || {
                    hooks.after_project_migration_table_creation(&path);
                    Ok(())
                })
                .map_err(|source| StoreError::Database {
                    operation: "migrate project schema from v1 to v2",
                    path: path.clone(),
                    source,
                })?;
                let migrated_version = validate_project_structure(&transaction, &path, &identity)?;
                if migrated_version != SCHEMA_VERSION {
                    return Err(StoreError::IncompatibleSchema { path });
                }
                needs_post_commit_validation = true;
            }
        } else if has_persistent_schema(&transaction).map_err(|source| StoreError::Database {
            operation: "recheck for foreign project schema inside immediate transaction",
            path: path.clone(),
            source,
        })? {
            return Err(StoreError::IncompatibleSchema { path });
        } else {
            create_project_schema(&transaction, &identity.root_text).map_err(|source| {
                StoreError::Database {
                    operation: "create project schema",
                    path: path.clone(),
                    source,
                }
            })?;
        }
        transaction
            .commit()
            .map_err(|source| StoreError::Database {
                operation: "commit project-store transaction",
                path: path.clone(),
                source,
            })?;
    }

    if needs_post_commit_validation {
        validate_project_database(&connection, &path, &identity)?;
    }
    if database.state() == DatabaseState::Empty {
        validate_project_journal_header(&database)?;
    }
    database.revalidate(&directory)?;
    directory.refuse_sidecars(database.name())?;
    directory.revalidate()?;
    drop(project_lock);
    Ok(connection)
}

fn validate_project_journal_header(
    database: &crate::secure::PinnedDatabase,
) -> Result<(), StoreError> {
    let (write_version, read_version) = database.sqlite_journal_versions()?;
    if (write_version, read_version) != (1, 1) {
        let found = if (write_version, read_version) == (2, 2) {
            "wal".to_owned()
        } else {
            format!("header versions {write_version}/{read_version}")
        };
        return Err(StoreError::UnexpectedJournalMode {
            path: database.path().to_path_buf(),
            expected: "delete",
            found,
        });
    }
    Ok(())
}

fn sanitize_basename(value: &str) -> String {
    const MAX_READABLE_BYTES: usize = 64;

    let mut sanitized = String::with_capacity(value.len().min(MAX_READABLE_BYTES));
    let mut pending_separator = false;
    for character in value.chars() {
        if character.is_ascii_alphanumeric() {
            let needed = usize::from(pending_separator && !sanitized.is_empty()) + 1;
            if sanitized.len() + needed > MAX_READABLE_BYTES {
                break;
            }
            if pending_separator && !sanitized.is_empty() {
                sanitized.push('-');
            }
            sanitized.push(character);
            pending_separator = false;
        } else {
            pending_separator = true;
        }
    }
    sanitized
}

fn open_immutable(path: &Path, operation: &'static str) -> Result<Connection, StoreError> {
    // SQLite's immutable URI deliberately skips locking and change detection.
    // Callers reach this only with a pinned, owner-private, single-link file,
    // no recovery sidecars, and the documented same-UID serialization trust.
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_URI
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    Connection::open_with_flags(immutable_uri(path), flags).map_err(|source| StoreError::Database {
        operation,
        path: path.to_path_buf(),
        source,
    })
}

fn validate_project_database(
    connection: &Connection,
    path: &Path,
    identity: &ProjectIdentity,
) -> Result<u32, StoreError> {
    let version = validate_project_structure(connection, path, identity)?;
    validate_integrity(connection, path)?;
    validate_foreign_keys(connection, path)?;
    validate_fts_consistency(connection, path)?;
    Ok(version)
}

fn validate_project_structure(
    connection: &Connection,
    path: &Path,
    identity: &ProjectIdentity,
) -> Result<u32, StoreError> {
    let version = validate_metadata_stamp(connection, path, identity)?;
    let schema_matches = if version == SCHEMA_VERSION {
        project_schema_is_current(connection)
    } else {
        project_schema_matches_version(connection, version)
    }
    .map_err(|source| StoreError::Database {
        operation: "compare project schema with the bundled reference",
        path: path.to_path_buf(),
        source,
    })?;
    if !schema_matches {
        return Err(StoreError::IncompatibleSchema {
            path: path.to_path_buf(),
        });
    }
    if version == SCHEMA_VERSION {
        validate_embedding_scheme_stamp(connection, path)?;
    }
    Ok(version)
}

fn validate_embedding_scheme_stamp(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    match read_embedding_scheme(connection) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::FromSqlConversionFailure(..)) => Err(StoreError::IncompatibleSchema {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(StoreError::Database {
            operation: "validate project embedding scheme stamp",
            path: path.to_path_buf(),
            source,
        }),
    }
}

fn validate_metadata_stamp(
    connection: &Connection,
    path: &Path,
    identity: &ProjectIdentity,
) -> Result<u32, StoreError> {
    let metadata_exists =
        metadata_table_exists(connection).map_err(|source| StoreError::Database {
            operation: "inspect project metadata table",
            path: path.to_path_buf(),
            source,
        })?;
    if !metadata_exists {
        return Err(StoreError::IncompatibleSchema {
            path: path.to_path_buf(),
        });
    }
    let version = connection
        .query_row(
            "SELECT schema_version FROM metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|_| StoreError::IncompatibleSchema {
            path: path.to_path_buf(),
        })?;
    let Some(version) = version else {
        return Err(StoreError::IncompatibleSchema {
            path: path.to_path_buf(),
        });
    };

    if version > i64::from(SCHEMA_VERSION) {
        return Err(StoreError::FutureSchema {
            found: u64::try_from(version).unwrap_or(u64::MAX),
            supported: SCHEMA_VERSION,
        });
    }
    if version != i64::from(MIGRATABLE_SCHEMA_VERSION) && version != i64::from(SCHEMA_VERSION) {
        return Err(StoreError::UnsupportedSchema {
            found: version,
            supported: SCHEMA_VERSION,
        });
    }
    let found_root = connection
        .query_row("SELECT root FROM metadata WHERE singleton = 1", [], |row| {
            row.get::<_, String>(0)
        })
        .optional()
        .map_err(|_| StoreError::IncompatibleSchema {
            path: path.to_path_buf(),
        })?
        .ok_or_else(|| StoreError::IncompatibleSchema {
            path: path.to_path_buf(),
        })?;
    if found_root != identity.root_text {
        return Err(StoreError::WrongRoot {
            expected: identity.canonical_root.clone(),
            found: PathBuf::from(found_root),
        });
    }
    Ok(u32::try_from(version).expect("accepted project schema versions fit u32"))
}

fn validate_integrity(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    let mut statement = connection
        .prepare("PRAGMA integrity_check")
        .map_err(|source| StoreError::Database {
            operation: "prepare full project integrity check",
            path: path.to_path_buf(),
            source,
        })?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| StoreError::Database {
            operation: "run full project integrity check",
            path: path.to_path_buf(),
            source,
        })?;
    let mut findings = Vec::new();
    for row in rows {
        findings.push(row.map_err(|source| StoreError::Database {
            operation: "read full project integrity result",
            path: path.to_path_buf(),
            source,
        })?);
    }
    if findings.len() != 1 || findings[0] != "ok" {
        return Err(StoreError::Integrity {
            path: path.to_path_buf(),
            detail: findings.join("; "),
        });
    }
    Ok(())
}

fn validate_foreign_keys(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    let finding = connection
        .query_row(
            "SELECT \"table\", rowid, parent, fkid FROM pragma_foreign_key_check LIMIT 1",
            [],
            |row| {
                Ok(format!(
                    "table={} rowid={} parent={} foreign-key={}",
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?
                ))
            },
        )
        .optional()
        .map_err(|source| StoreError::Database {
            operation: "run project foreign-key check",
            path: path.to_path_buf(),
            source,
        })?;
    if let Some(detail) = finding {
        return Err(StoreError::Integrity {
            path: path.to_path_buf(),
            detail: format!("foreign_key_check: {detail}"),
        });
    }
    Ok(())
}

fn validate_fts_consistency(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    let missing_or_mismatched = connection
        .query_row(
            "SELECT c.id FROM chunks AS c \
             WHERE NOT EXISTS (\
                 SELECT 1 FROM chunks_fts AS f \
                 WHERE f.rowid = c.id AND f.text = c.text\
             ) LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|source| StoreError::Database {
            operation: "compare chunks with project FTS rows",
            path: path.to_path_buf(),
            source,
        })?;
    if let Some(rowid) = missing_or_mismatched {
        return Err(StoreError::Integrity {
            path: path.to_path_buf(),
            detail: format!("chunk {rowid} has no exact FTS row/text match"),
        });
    }

    let phantom = connection
        .query_row(
            "SELECT f.rowid FROM chunks_fts AS f \
             WHERE NOT EXISTS (SELECT 1 FROM chunks AS c WHERE c.id = f.rowid) \
             LIMIT 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(|source| StoreError::Database {
            operation: "compare project FTS rows with chunks",
            path: path.to_path_buf(),
            source,
        })?;
    if let Some(rowid) = phantom {
        return Err(StoreError::Integrity {
            path: path.to_path_buf(),
            detail: format!("FTS row {rowid} has no chunk"),
        });
    }
    Ok(())
}

fn metadata_table_exists(connection: &Connection) -> Result<bool, rusqlite::Error> {
    connection.query_row(
        "SELECT EXISTS(\
             SELECT 1 FROM sqlite_schema \
             WHERE type = 'table' AND name = 'metadata'\
         )",
        [],
        |row| row.get(0),
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    use tempfile::tempdir;

    use super::{
        OpenHooks, SCHEMA_VERSION, StoreError, StoreHome, open_project, open_project_with_hooks,
    };

    struct ReplaceAfterSqliteOpen {
        backup: PathBuf,
        victim: PathBuf,
        expected: Vec<u8>,
    }

    struct ReplaceBaseAfterSqliteOpen {
        backup: PathBuf,
    }

    struct WaitBeforeSqliteOpen {
        barrier: Arc<Barrier>,
    }

    struct PauseAfterMigrationTableCreation {
        reached: mpsc::SyncSender<()>,
        release: mpsc::Receiver<()>,
    }

    struct SignalBeforeSqliteOpen {
        reached: mpsc::SyncSender<()>,
    }

    impl OpenHooks for PauseAfterMigrationTableCreation {
        fn after_project_migration_table_creation(&mut self, _path: &Path) {
            self.reached.send(()).unwrap();
            self.release.recv().unwrap();
        }
    }

    impl OpenHooks for SignalBeforeSqliteOpen {
        fn before_sqlite_open(&mut self, _path: &Path) {
            self.reached.send(()).unwrap();
        }
    }

    impl OpenHooks for WaitBeforeSqliteOpen {
        fn before_sqlite_open(&mut self, _path: &Path) {
            self.barrier.wait();
        }
    }

    impl OpenHooks for ReplaceBaseAfterSqliteOpen {
        fn after_sqlite_open(&mut self, path: &Path) {
            use std::os::unix::fs::PermissionsExt;

            let base = path
                .parent()
                .and_then(Path::parent)
                .and_then(Path::parent)
                .unwrap();
            std::fs::rename(base, &self.backup).unwrap();
            std::fs::create_dir(base).unwrap();
            std::fs::set_permissions(base, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
    }

    impl OpenHooks for ReplaceAfterSqliteOpen {
        fn after_sqlite_open(&mut self, path: &Path) {
            use std::os::unix::fs::{PermissionsExt, symlink};

            std::fs::rename(path, &self.backup).unwrap();
            std::fs::write(&self.victim, &self.expected).unwrap();
            std::fs::set_permissions(&self.victim, std::fs::Permissions::from_mode(0o600)).unwrap();
            symlink(&self.victim, path).unwrap();
        }
    }

    #[test]
    fn project_path_replacement_across_sqlite_open_is_typed_and_never_touches_the_victim() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let root = tmp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let expected = b"external project victim".to_vec();
        let victim = tmp.path().join("victim");
        let mut hooks = ReplaceAfterSqliteOpen {
            backup: tmp.path().join("pinned-project.sqlite3"),
            victim: victim.clone(),
            expected: expected.clone(),
        };

        let error = open_project_with_hooks(&home, &root, &mut hooks).unwrap_err();

        assert!(matches!(error, StoreError::RacedStorePath { .. }));
        assert_eq!(std::fs::read(victim).unwrap(), expected);
        assert_eq!(std::fs::metadata(hooks.backup).unwrap().len(), 0);
    }

    #[test]
    fn project_base_replacement_across_sqlite_open_is_refused() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let root = tmp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let mut hooks = ReplaceBaseAfterSqliteOpen {
            backup: tmp.path().join("pinned-project-state"),
        };

        let error = open_project_with_hooks(&home, &root, &mut hooks).unwrap_err();

        assert!(matches!(error, StoreError::RacedStorePath { .. }));
        assert!(!home.project_path(&root).unwrap().exists());
        let pinned_database = hooks
            .backup
            .join("rust-v1/indexes")
            .join(home.project_path(&root).unwrap().file_name().unwrap());
        assert_eq!(std::fs::metadata(&pinned_database).unwrap().len(), 0);
        let database_name = pinned_database.file_name().unwrap().to_owned();
        let entries = std::fs::read_dir(pinned_database.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        assert_eq!(
            entries,
            vec![database_name],
            "the pinned project directory should contain only the database"
        );
    }

    #[test]
    fn concurrent_first_project_openers_adopt_one_transactional_schema() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let root = tmp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let barrier = Arc::new(Barrier::new(2));

        let (first, second) = std::thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let first = scope.spawn(|| {
                let mut hooks = WaitBeforeSqliteOpen {
                    barrier: first_barrier,
                };
                open_project_with_hooks(&home, &root, &mut hooks)
            });
            let second_barrier = Arc::clone(&barrier);
            let second = scope.spawn(|| {
                let mut hooks = WaitBeforeSqliteOpen {
                    barrier: second_barrier,
                };
                open_project_with_hooks(&home, &root, &mut hooks)
            });
            (first.join().unwrap(), second.join().unwrap())
        });
        let first = first.unwrap();
        let second = second.unwrap();

        first
            .execute("UPDATE metadata SET generation = 1 WHERE singleton = 1", [])
            .unwrap();
        let generation: i64 = second
            .query_row(
                "SELECT generation FROM metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(generation, 1);
    }

    #[test]
    fn concurrent_v1_migrators_pin_v1_before_serialized_preflight_and_converge() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let root = tmp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let initial = open_project(&home, &root).unwrap();
        initial
            .execute_batch(
                "DROP TABLE embedding_scheme; \
                 UPDATE metadata SET schema_version = 1 WHERE singleton = 1;",
            )
            .unwrap();
        drop(initial);
        let barrier = Arc::new(Barrier::new(2));

        let (first, second) = std::thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let first = scope.spawn(|| {
                let mut hooks = WaitBeforeSqliteOpen {
                    barrier: first_barrier,
                };
                open_project_with_hooks(&home, &root, &mut hooks)
            });
            let second_barrier = Arc::clone(&barrier);
            let second = scope.spawn(|| {
                let mut hooks = WaitBeforeSqliteOpen {
                    barrier: second_barrier,
                };
                open_project_with_hooks(&home, &root, &mut hooks)
            });
            (first.join().unwrap(), second.join().unwrap())
        });

        for connection in [first.unwrap(), second.unwrap()] {
            let version: u32 = connection
                .query_row(
                    "SELECT schema_version FROM metadata WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let schemes: i64 = connection
                .query_row("SELECT count(*) FROM embedding_scheme", [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
            assert_eq!(schemes, 0);
        }
    }

    #[test]
    fn opener_waits_for_a_cooperating_v1_migration_journal_then_converges() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let root = tmp.path().join("project");
        std::fs::create_dir(&root).unwrap();
        let initial = open_project(&home, &root).unwrap();
        initial
            .execute_batch(
                "DROP TABLE embedding_scheme; \
                 UPDATE metadata SET schema_version = 1 WHERE singleton = 1;",
            )
            .unwrap();
        drop(initial);
        let database_path = home.project_path(&root).unwrap();
        let mut journal_name = database_path.as_os_str().to_owned();
        journal_name.push("-journal");
        let journal_path = PathBuf::from(journal_name);
        let (ddl_reached_sender, ddl_reached_receiver) = mpsc::sync_channel(0);
        let (release_sender, release_receiver) = mpsc::sync_channel(0);
        let (opener_started_sender, opener_started_receiver) = mpsc::sync_channel(0);
        let (opener_sender, opener_receiver) = mpsc::sync_channel(1);

        let (migrator, opener) = std::thread::scope(|scope| {
            let migrator = scope.spawn(|| {
                let mut hooks = PauseAfterMigrationTableCreation {
                    reached: ddl_reached_sender,
                    release: release_receiver,
                };
                open_project_with_hooks(&home, &root, &mut hooks)
            });
            ddl_reached_receiver.recv().unwrap();
            assert!(journal_path.exists());
            let opener = scope.spawn(|| {
                let mut hooks = SignalBeforeSqliteOpen {
                    reached: opener_started_sender,
                };
                opener_sender
                    .send(open_project_with_hooks(&home, &root, &mut hooks))
                    .unwrap();
            });
            opener_started_receiver.recv().unwrap();
            assert!(matches!(
                opener_receiver.recv_timeout(Duration::from_millis(250)),
                Err(mpsc::RecvTimeoutError::Timeout)
            ));
            release_sender.send(()).unwrap();
            (migrator.join().unwrap(), {
                opener.join().unwrap();
                opener_receiver.recv().unwrap()
            })
        });

        for connection in [migrator.unwrap(), opener.unwrap()] {
            let version: u32 = connection
                .query_row(
                    "SELECT schema_version FROM metadata WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(version, SCHEMA_VERSION);
        }
    }

    #[test]
    fn sidecar_integrity_busy_and_races_are_transient_contention() {
        let path = PathBuf::from("/tmp/index.sqlite3");
        assert!(StoreError::UnsafeSidecar { path: path.clone() }.is_transient_contention());
        assert!(
            StoreError::Integrity {
                path: path.clone(),
                detail: "wrong # of entries in index".into(),
            }
            .is_transient_contention()
        );
        assert!(
            StoreError::Busy {
                operation: "open",
                path: path.clone(),
            }
            .is_transient_contention()
        );
        assert!(StoreError::RacedStorePath { path: path.clone() }.is_transient_contention());
        assert!(StoreError::RacedProjectRoot { path }.is_transient_contention());
        assert!(
            !StoreError::FutureSchema {
                found: 9,
                supported: SCHEMA_VERSION,
            }
            .is_transient_contention()
        );
        assert!(
            !StoreError::IncompatibleSchema {
                path: PathBuf::from("/tmp/index.sqlite3"),
            }
            .is_transient_contention()
        );
    }
}
