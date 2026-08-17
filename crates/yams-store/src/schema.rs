use std::path::{Path, PathBuf};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, Transaction, TransactionBehavior, params,
};

use crate::home::StoreHome;
use crate::project::StoreError;
use crate::secure::{
    DatabaseState, NoHooks, OpenHooks, SecureStoreDirectory, appended_name, immutable_uri,
};

pub const SCHEMA_VERSION: u32 = 2;
pub(crate) const MIGRATABLE_SCHEMA_VERSION: u32 = 1;

const METADATA_SCHEMA: &str = r#"
CREATE TABLE metadata (
    singleton      INTEGER PRIMARY KEY CHECK (singleton = 1),
    schema_version INTEGER NOT NULL CHECK (schema_version >= 1),
    root           TEXT NOT NULL CHECK (length(root) > 0),
    generation     INTEGER NOT NULL DEFAULT 0 CHECK (generation >= 0)
) STRICT
"#;

const DOCS_SCHEMA: &str = r#"
CREATE TABLE docs (
    path         TEXT PRIMARY KEY NOT NULL,
    corpus       TEXT NOT NULL CHECK (corpus IN ('shared', 'private', 'override')),
    status       TEXT CHECK (status IS NULL OR status IN ('current', 'historical', 'in-progress')),
    content_hash TEXT NOT NULL CHECK (
        length(content_hash) = 64 AND content_hash NOT GLOB '*[^0-9a-f]*'
    ),
    byte_length  INTEGER NOT NULL CHECK (byte_length >= 0),
    mtime_ns     INTEGER NOT NULL,
    device       INTEGER NOT NULL CHECK (device >= 0),
    inode        INTEGER NOT NULL CHECK (inode >= 0),
    generation   INTEGER NOT NULL CHECK (generation >= 0)
) STRICT
"#;

const CHUNKS_SCHEMA: &str = r#"
CREATE TABLE chunks (
    id          INTEGER PRIMARY KEY,
    path        TEXT NOT NULL,
    ordinal     INTEGER NOT NULL CHECK (ordinal >= 0),
    text        TEXT NOT NULL,
    embed_text  TEXT NOT NULL,
    vector_hash TEXT NOT NULL CHECK (
        length(vector_hash) = 64 AND vector_hash NOT GLOB '*[^0-9a-f]*'
    ),
    UNIQUE (path, ordinal),
    FOREIGN KEY (path) REFERENCES docs(path) ON DELETE CASCADE
) STRICT
"#;

const CHUNKS_PATH_INDEX_SCHEMA: &str = "CREATE INDEX chunks_by_path ON chunks(path)";

const CHUNKS_FTS_SCHEMA: &str = r#"
CREATE VIRTUAL TABLE chunks_fts USING fts5(
    text,
    tokenize = 'porter unicode61'
)
"#;

pub(crate) const EMBEDDING_SCHEME_SCHEMA: &str = r#"
CREATE TABLE embedding_scheme (
    singleton  INTEGER PRIMARY KEY CHECK (singleton = 1),
    signature  TEXT NOT NULL CHECK (
        typeof(signature) = 'text'
        AND length(CAST(signature AS BLOB)) = 64
        AND instr(signature, char(0)) = 0
        AND signature NOT GLOB '*[^0-9a-f]*'
    ),
    dimensions INTEGER NOT NULL CHECK (dimensions > 0)
) STRICT
"#;

const VECTOR_SCHEMA: &str = r#"
CREATE TABLE vectors (
    hash            TEXT PRIMARY KEY NOT NULL,
    model_signature TEXT NOT NULL CHECK (length(model_signature) > 0),
    dimensions      INTEGER NOT NULL CHECK (dimensions > 0),
    bytes           BLOB NOT NULL CHECK (length(bytes) = dimensions * 4)
) STRICT
"#;

#[derive(Debug, Eq, PartialEq)]
struct SchemaObject {
    object_type: String,
    name: String,
    table_name: String,
    sql: Option<String>,
}

pub(crate) fn create_project_schema(
    transaction: &Transaction<'_>,
    root: &str,
) -> Result<(), rusqlite::Error> {
    create_project_objects(transaction)?;
    transaction.execute(
        "INSERT INTO metadata(singleton, schema_version, root) VALUES (1, ?1, ?2)",
        params![SCHEMA_VERSION, root],
    )?;
    Ok(())
}

fn create_project_objects(connection: &Connection) -> Result<(), rusqlite::Error> {
    create_project_v1_objects(connection)?;
    connection.execute_batch(EMBEDDING_SCHEME_SCHEMA)?;
    Ok(())
}

fn create_project_v1_objects(connection: &Connection) -> Result<(), rusqlite::Error> {
    for statement in [
        METADATA_SCHEMA,
        DOCS_SCHEMA,
        CHUNKS_SCHEMA,
        CHUNKS_PATH_INDEX_SCHEMA,
        CHUNKS_FTS_SCHEMA,
    ] {
        connection.execute_batch(statement)?;
    }
    Ok(())
}

pub(crate) fn project_schema_is_current(connection: &Connection) -> Result<bool, rusqlite::Error> {
    project_schema_matches_version(connection, SCHEMA_VERSION)
}

pub(crate) fn project_schema_matches_version(
    connection: &Connection,
    version: u32,
) -> Result<bool, rusqlite::Error> {
    let reference = Connection::open_in_memory()?;
    match version {
        MIGRATABLE_SCHEMA_VERSION => create_project_v1_objects(&reference)?,
        SCHEMA_VERSION => create_project_objects(&reference)?,
        _ => return Ok(false),
    }
    Ok(schema_objects(connection)? == schema_objects(&reference)?)
}

pub(crate) fn migrate_project_schema_v1_to_v2_with_hook(
    transaction: &Transaction<'_>,
    after_table_creation: impl FnOnce() -> Result<(), rusqlite::Error>,
) -> Result<(), rusqlite::Error> {
    transaction.execute_batch(EMBEDDING_SCHEME_SCHEMA)?;
    after_table_creation()?;
    let updated = transaction.execute(
        "UPDATE metadata SET schema_version = ?1 \
         WHERE singleton = 1 AND schema_version = ?2",
        params![SCHEMA_VERSION, MIGRATABLE_SCHEMA_VERSION],
    )?;
    if updated != 1 {
        return Err(rusqlite::Error::QueryReturnedNoRows);
    }
    Ok(())
}

pub(crate) fn has_persistent_schema(connection: &Connection) -> Result<bool, rusqlite::Error> {
    connection.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema)", [], |row| {
        row.get(0)
    })
}

fn vector_schema_is_current(connection: &Connection) -> Result<bool, rusqlite::Error> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(VECTOR_SCHEMA)?;
    Ok(schema_objects(connection)? == schema_objects(&reference)?)
}

fn schema_objects(connection: &Connection) -> Result<Vec<SchemaObject>, rusqlite::Error> {
    let mut statement = connection.prepare(
        "SELECT type, name, tbl_name, sql FROM sqlite_schema \
         ORDER BY type, name, tbl_name, sql",
    )?;
    statement
        .query_map([], |row| {
            Ok(SchemaObject {
                object_type: row.get(0)?,
                name: row.get(1)?,
                table_name: row.get(2)?,
                sql: row.get(3)?,
            })
        })?
        .collect()
}

/// Opens the shared vector cache inside the private rust-v1 boundary.
///
/// Existing rollback journals and nonempty or malformed WAL sidecars are
/// conservatively refused before inspection. The exact private artifact pair
/// left by an ordinary read-only vector connection (an empty WAL and 32-KiB
/// SHM) is pinned and revalidated under the initialization lock, then adopted
/// by SQLite's ordinary read-write opener. This permits management readers and
/// writers to coexist without accepting crash residue. As with `open_project`,
/// processes running as the same effective user remain trusted after the final
/// descriptor/name identity check.
pub fn open_vectors(home: &StoreHome) -> Result<Connection, StoreError> {
    open_vectors_with_hooks(home, &mut NoHooks)
}

/// Opens the vector cache for search without a full `PRAGMA integrity_check`.
pub fn open_vectors_for_search(home: &StoreHome) -> Result<Connection, StoreError> {
    open_vectors_with(home, &mut NoHooks, false)
}

fn open_vectors_with_hooks(
    home: &StoreHome,
    hooks: &mut dyn OpenHooks,
) -> Result<Connection, StoreError> {
    open_vectors_with(home, hooks, true)
}

fn open_vectors_with(
    home: &StoreHome,
    hooks: &mut dyn OpenHooks,
    full_integrity: bool,
) -> Result<Connection, StoreError> {
    let directory = SecureStoreDirectory::for_vectors(home)?;
    let filename = home
        .vectors_path()
        .file_name()
        .expect("vectors path always has a filename")
        .to_owned();
    let database = directory.prepare_database_without_sidecar_check(&filename)?;
    let path = database.path().to_path_buf();
    let sidecars_were_absent = directory.sqlite_sidecars_absent(database.name())?;

    // Preserve the mutation-free rejection path for an existing database with
    // no SQLite sidecars. In particular, malformed externally-created files
    // are refused before we create the initialization-lock file.
    if database.state() == DatabaseState::Existing && sidecars_were_absent {
        let inspection = open_immutable(&path, "open vector store for immutable inspection")?;
        validate_vector_database_with(&inspection, &path, full_integrity)?;
        drop(inspection);
        database.revalidate(&directory)?;
    }

    // Serialize sidecar classification through SQLite open so a cooperating
    // opener cannot replace the exact reader artifacts after they are pinned.
    hooks.before_vector_initialization_lock(&path);
    let initialization_lock_file = directory.prepare_sqlite_initialization_lock(database.name())?;
    let sqlite_initialization_lock = initialization_lock_file.lock()?;
    initialization_lock_file.revalidate(&directory)?;
    database.revalidate(&directory)?;
    let reader_sidecars = directory.pin_empty_vector_reader_sidecars(database.name())?;
    hooks.after_vector_sidecars_pinned(&path);

    // Sidecars may have appeared between the early absence check and taking
    // the lock. Inspect again whenever the database was not already validated
    // in a sidecar-free state, or when a reader pair is now present.
    if database.state() == DatabaseState::Existing
        && (!sidecars_were_absent || reader_sidecars.is_some())
    {
        let inspection = open_immutable(&path, "open vector store for immutable inspection")?;
        validate_vector_database_with(&inspection, &path, full_integrity)?;
        drop(inspection);
        database.revalidate(&directory)?;
        if let Some(sidecars) = &reader_sidecars {
            sidecars.revalidate(&directory)?;
        }
    }

    hooks.before_sqlite_open(&path);

    // Two first openers can both pin the same newly-created empty file before
    // either reaches SQLite. Serialize the whole SQLite-open-to-WAL window so
    // an idle losing connection cannot prevent the winner's journal change.
    // The transaction below still rechecks live schema state; this advisory
    // lock is coordination, not a substitute for validation.
    initialization_lock_file.revalidate(&directory)?;
    database.revalidate(&directory)?;
    if let Some(sidecars) = &reader_sidecars {
        sidecars.revalidate(&directory)?;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    let mut connection =
        Connection::open_with_flags(&path, flags).map_err(|source| StoreError::Database {
            operation: "open vector store read-write without following links",
            path: path.clone(),
            source,
        })?;
    hooks.after_sqlite_open(&path);
    database.revalidate(&directory)?;
    if let Some(sidecars) = &reader_sidecars {
        sidecars.revalidate(&directory)?;
    }

    {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| {
                if reader_sidecars.is_some()
                    && matches!(
                        source.sqlite_error_code(),
                        Some(
                            rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                        )
                    )
                {
                    StoreError::UnsafeSidecar {
                        path: PathBuf::from(appended_name(path.as_os_str(), "-wal")),
                    }
                } else {
                    StoreError::Database {
                        operation: "begin immediate vector-store transaction",
                        path: path.clone(),
                        source,
                    }
                }
            })?;
        let vectors_exist =
            vector_table_exists(&transaction).map_err(|source| StoreError::Database {
                operation: "recheck vector schema state inside immediate transaction",
                path: path.clone(),
                source,
            })?;
        if vectors_exist {
            validate_vector_structure(&transaction, &path)?;
        } else if has_persistent_schema(&transaction).map_err(|source| StoreError::Database {
            operation: "recheck for foreign vector schema inside immediate transaction",
            path: path.clone(),
            source,
        })? {
            return Err(StoreError::IncompatibleVectorSchema { path });
        } else {
            transaction
                .execute_batch(VECTOR_SCHEMA)
                .map_err(|source| StoreError::Database {
                    operation: "create vector schema",
                    path: path.clone(),
                    source,
                })?;
        }
        transaction
            .commit()
            .map_err(|source| StoreError::Database {
                operation: "commit vector-store transaction",
                path: path.clone(),
                source,
            })?;
    }
    if database.state() == DatabaseState::Empty {
        validate_vector_database_with(&connection, &path, full_integrity)?;
    }

    let journal_mode: String = connection
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .map_err(|source| StoreError::Database {
            operation: "enable vector WAL journal mode",
            path: path.clone(),
            source,
        })?;
    if !journal_mode.eq_ignore_ascii_case("wal") {
        return Err(StoreError::UnexpectedJournalMode {
            path,
            expected: "wal",
            found: journal_mode,
        });
    }
    let (write_version, read_version) = database.sqlite_journal_versions()?;
    if (write_version, read_version) != (2, 2) {
        return Err(StoreError::UnexpectedJournalMode {
            path,
            expected: "wal",
            found: format!("header versions {write_version}/{read_version}"),
        });
    }

    // Entering the WAL write-lock once makes SQLite materialize its WAL and
    // shared-memory files even when the cache is still empty. That lets this
    // opener validate and enforce their private permissions before returning
    // a connection that callers can write through.
    {
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| StoreError::Database {
                operation: "materialize private vector WAL sidecars",
                path: path.clone(),
                source,
            })?;
        transaction
            .commit()
            .map_err(|source| StoreError::Database {
                operation: "finish vector WAL sidecar materialization",
                path: path.clone(),
                source,
            })?;
    }

    database.revalidate(&directory)?;
    directory.secure_sqlite_sidecar(database.name(), "-wal")?;
    directory.secure_sqlite_sidecar(database.name(), "-shm")?;
    directory.refuse_sidecar(database.name(), "-journal")?;
    drop(sqlite_initialization_lock);
    Ok(connection)
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

fn validate_vector_database_with(
    connection: &Connection,
    path: &Path,
    full_integrity: bool,
) -> Result<(), StoreError> {
    validate_vector_structure(connection, path)?;
    if full_integrity {
        validate_integrity(connection, path)?;
    }
    validate_foreign_keys(connection, path)
}

fn validate_vector_structure(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    let schema_matches =
        vector_schema_is_current(connection).map_err(|source| StoreError::Database {
            operation: "compare vector schema with the bundled reference",
            path: path.to_path_buf(),
            source,
        })?;
    if !schema_matches {
        return Err(StoreError::IncompatibleVectorSchema {
            path: path.to_path_buf(),
        });
    }
    Ok(())
}

fn vector_table_exists(connection: &Connection) -> Result<bool, rusqlite::Error> {
    connection.query_row(
        "SELECT EXISTS(\
             SELECT 1 FROM sqlite_schema \
             WHERE type = 'table' AND name = 'vectors'\
         )",
        [],
        |row| row.get(0),
    )
}

fn validate_integrity(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    let mut statement = connection
        .prepare("PRAGMA integrity_check")
        .map_err(|source| StoreError::Database {
            operation: "prepare full vector integrity check",
            path: path.to_path_buf(),
            source,
        })?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|source| StoreError::Database {
            operation: "run full vector integrity check",
            path: path.to_path_buf(),
            source,
        })?;
    let mut findings = Vec::new();
    for row in rows {
        findings.push(row.map_err(|source| StoreError::Database {
            operation: "read full vector integrity result",
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
            operation: "run vector foreign-key check",
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier};

    use rusqlite::{Connection, OpenFlags, TransactionBehavior};
    use tempfile::tempdir;

    use super::{
        MIGRATABLE_SCHEMA_VERSION, OpenHooks, StoreError, StoreHome, create_project_v1_objects,
        migrate_project_schema_v1_to_v2_with_hook, open_vectors_with_hooks,
        project_schema_matches_version,
    };

    struct ReplaceAfterSqliteOpen {
        backup: PathBuf,
        victim: PathBuf,
        expected: Vec<u8>,
    }

    struct ReplaceBaseAfterSqliteOpen {
        backup: PathBuf,
    }

    struct WaitBeforeVectorInitializationLock {
        barrier: Arc<Barrier>,
    }

    struct ReplaceWalAfterPin {
        backup: PathBuf,
    }

    impl OpenHooks for WaitBeforeVectorInitializationLock {
        fn before_vector_initialization_lock(&mut self, _path: &Path) {
            self.barrier.wait();
        }
    }

    impl OpenHooks for ReplaceWalAfterPin {
        fn after_vector_sidecars_pinned(&mut self, path: &Path) {
            use std::os::unix::fs::PermissionsExt;

            let mut wal = path.as_os_str().to_owned();
            wal.push("-wal");
            let wal = PathBuf::from(wal);
            std::fs::rename(&wal, &self.backup).unwrap();
            std::fs::write(&wal, b"").unwrap();
            std::fs::set_permissions(&wal, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
    }

    impl OpenHooks for ReplaceBaseAfterSqliteOpen {
        fn after_sqlite_open(&mut self, path: &Path) {
            use std::os::unix::fs::PermissionsExt;

            let base = path.parent().and_then(Path::parent).unwrap();
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
    fn project_v1_migration_rolls_back_table_creation_when_version_update_cannot_start() {
        let mut connection = Connection::open_in_memory().unwrap();
        create_project_v1_objects(&connection).unwrap();
        connection
            .execute(
                "INSERT INTO metadata(singleton, schema_version, root) VALUES (1, ?1, '/project')",
                [MIGRATABLE_SCHEMA_VERSION],
            )
            .unwrap();

        {
            let transaction = connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .unwrap();
            let error = migrate_project_schema_v1_to_v2_with_hook(&transaction, || {
                Err(rusqlite::Error::InvalidQuery)
            })
            .unwrap_err();
            assert!(matches!(error, rusqlite::Error::InvalidQuery));
            transaction.rollback().unwrap();
        }

        let version: u32 = connection
            .query_row(
                "SELECT schema_version FROM metadata WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let embedding_scheme_exists: bool = connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'embedding_scheme')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(version, MIGRATABLE_SCHEMA_VERSION);
        assert!(!embedding_scheme_exists);
        assert!(project_schema_matches_version(&connection, MIGRATABLE_SCHEMA_VERSION).unwrap());
    }

    #[test]
    fn vector_path_replacement_across_sqlite_open_is_typed_and_never_touches_the_victim() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let expected = b"external vector victim".to_vec();
        let victim = tmp.path().join("victim");
        let mut hooks = ReplaceAfterSqliteOpen {
            backup: tmp.path().join("pinned-vectors.sqlite3"),
            victim: victim.clone(),
            expected: expected.clone(),
        };

        let error = open_vectors_with_hooks(&home, &mut hooks).unwrap_err();

        assert!(matches!(error, StoreError::RacedStorePath { .. }));
        assert_eq!(std::fs::read(victim).unwrap(), expected);
        assert_eq!(std::fs::metadata(hooks.backup).unwrap().len(), 0);
    }

    #[test]
    fn vector_base_replacement_across_sqlite_open_is_refused() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let mut hooks = ReplaceBaseAfterSqliteOpen {
            backup: tmp.path().join("pinned-vector-state"),
        };

        let error = open_vectors_with_hooks(&home, &mut hooks).unwrap_err();

        assert!(matches!(error, StoreError::RacedStorePath { .. }));
        assert!(!home.vectors_path().exists());
        let pinned_database = hooks.backup.join("rust-v1/vectors.sqlite3");
        assert_eq!(std::fs::metadata(&pinned_database).unwrap().len(), 0);
        let mut entries = std::fs::read_dir(pinned_database.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect::<Vec<_>>();
        entries.sort();
        assert_eq!(
            entries,
            ["vectors.sqlite3", "vectors.sqlite3-init.lock"]
                .into_iter()
                .map(std::ffi::OsString::from)
                .collect::<Vec<_>>()
        );
        assert_eq!(
            std::fs::metadata(pinned_database.with_file_name("vectors.sqlite3-init.lock"))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn vector_reader_sidecar_replacement_after_pin_is_refused_and_preserved() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        drop(super::open_vectors(&home).unwrap());
        let path = home.vectors_path().canonicalize().unwrap();
        let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        let reader = Connection::open_with_flags(&path, flags).unwrap();
        reader
            .query_row("SELECT count(*) FROM vectors", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap();
        drop(reader);
        let backup = tmp.path().join("pinned-wal");
        let mut hooks = ReplaceWalAfterPin {
            backup: backup.clone(),
        };

        let error = open_vectors_with_hooks(&home, &mut hooks).unwrap_err();

        assert!(matches!(error, StoreError::UnsafeSidecar { .. }));
        assert_eq!(std::fs::metadata(backup).unwrap().len(), 0);
        let mut wal = path.as_os_str().to_owned();
        wal.push("-wal");
        assert_eq!(std::fs::metadata(PathBuf::from(wal)).unwrap().len(), 0);
    }

    #[test]
    fn concurrent_first_vector_openers_adopt_one_transactional_schema() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("state"));
        let barrier = Arc::new(Barrier::new(2));

        let (first, second) = std::thread::scope(|scope| {
            let first_barrier = Arc::clone(&barrier);
            let first = scope.spawn(|| {
                let mut hooks = WaitBeforeVectorInitializationLock {
                    barrier: first_barrier,
                };
                open_vectors_with_hooks(&home, &mut hooks)
            });
            let second_barrier = Arc::clone(&barrier);
            let second = scope.spawn(|| {
                let mut hooks = WaitBeforeVectorInitializationLock {
                    barrier: second_barrier,
                };
                open_vectors_with_hooks(&home, &mut hooks)
            });
            (first.join().unwrap(), second.join().unwrap())
        });
        let first = first.unwrap();
        let second = second.unwrap();

        first
            .execute(
                "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
                 VALUES ('key', 'model-v1', 1, X'00000000')",
                [],
            )
            .unwrap();
        let rows: i64 = second
            .query_row("SELECT count(*) FROM vectors", [], |row| row.get(0))
            .unwrap();
        assert_eq!(rows, 1);
        assert!(matches!(
            super::open_vectors(&home),
            Err(StoreError::UnsafeSidecar { .. })
        ));
    }
}
