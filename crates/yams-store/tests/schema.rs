use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::os::unix::fs::MetadataExt;

use rusqlite::{Connection, ErrorCode, OpenFlags, params};
use sha2::{Digest, Sha256};
use tempfile::tempdir;
use yams_store::{
    EmbeddingScheme, EmbeddingSchemeError, PathKind, SCHEMA_VERSION, StoreError, StoreHome,
    open_project, open_vectors, path_as_utf8, read_embedding_scheme, write_embedding_scheme,
};

fn database_path(connection: &Connection) -> PathBuf {
    connection
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name = 'main'",
            [],
            |row| row.get::<_, String>(0),
        )
        .map(PathBuf::from)
        .unwrap()
}

fn integrity(connection: &Connection) -> String {
    connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap()
}

#[cfg(unix)]
#[derive(Debug, Eq, PartialEq)]
struct PersistentFileSnapshot {
    bytes: Vec<u8>,
    device: u64,
    inode: u64,
    length: u64,
    modified: std::time::SystemTime,
    parent_modified: std::time::SystemTime,
    siblings: Vec<std::ffi::OsString>,
}

#[cfg(unix)]
fn persistent_file_snapshot(path: &Path) -> PersistentFileSnapshot {
    let metadata = std::fs::metadata(path).unwrap();
    let parent = path.parent().unwrap();
    let parent_metadata = std::fs::metadata(parent).unwrap();
    let mut siblings = std::fs::read_dir(parent)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect::<Vec<_>>();
    siblings.sort();
    PersistentFileSnapshot {
        bytes: std::fs::read(path).unwrap(),
        device: metadata.dev(),
        inode: metadata.ino(),
        length: metadata.len(),
        modified: metadata.modified().unwrap(),
        parent_modified: parent_metadata.modified().unwrap(),
        siblings,
    }
}

fn constraint_failed(error: rusqlite::Error) -> bool {
    matches!(
        error,
        rusqlite::Error::SqliteFailure(ref failure, _)
            if failure.code == ErrorCode::ConstraintViolation
    )
}

fn create_project(base: &Path, name: &str) -> PathBuf {
    let root = base.join(name);
    std::fs::create_dir(&root).unwrap();
    root
}

#[cfg(unix)]
fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;

    std::fs::symlink_metadata(path)
        .unwrap()
        .permissions()
        .mode()
        & 0o777
}

fn sidecar_path(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_owned();
    value.push(suffix);
    value.into()
}

fn open_vector_reader(path: &Path) -> Connection {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY
        | OpenFlags::SQLITE_OPEN_NO_MUTEX
        | OpenFlags::SQLITE_OPEN_NOFOLLOW;
    Connection::open_with_flags(path, flags).unwrap()
}

fn materialize_empty_vector_reader_sidecars(home: &StoreHome) -> PathBuf {
    drop(open_vectors(home).unwrap());
    let path = home.vectors_path().canonicalize().unwrap();
    let reader = open_vector_reader(&path);
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM vectors", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    drop(reader);
    path
}

fn version_dir(home: &StoreHome) -> PathBuf {
    home.vectors_path().parent().unwrap().to_owned()
}

#[cfg(unix)]
fn create_private_project_store_dirs(home: &StoreHome) {
    std::fs::create_dir_all(home.indexes_dir()).unwrap();
    set_mode(&version_dir(home), 0o700);
    set_mode(&home.indexes_dir(), 0o700);
}

#[cfg(unix)]
fn create_private_vector_store_dir(home: &StoreHome) {
    std::fs::create_dir_all(version_dir(home)).unwrap();
    set_mode(&version_dir(home), 0o700);
}

fn insert_doc_and_chunk(connection: &Connection, fts_text: Option<&str>) {
    connection
        .execute(
            "INSERT INTO docs \
             (path, corpus, status, content_hash, byte_length, mtime_ns, device, inode, generation) \
             VALUES ('/corpus/alpha.md', 'shared', 'current', ?1, 5, 6, 7, 8, 1)",
            ["a".repeat(64)],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO chunks \
             (path, ordinal, text, embed_text, vector_hash) \
             VALUES ('/corpus/alpha.md', 0, 'alpha', 'title: alpha', ?1)",
            ["b".repeat(64)],
        )
        .unwrap();
    if let Some(text) = fts_text {
        connection
            .execute(
                "INSERT INTO chunks_fts(rowid, text) \
                 SELECT id, ?1 FROM chunks WHERE path = '/corpus/alpha.md'",
                [text],
            )
            .unwrap();
    }
}

fn downgrade_project_to_exact_v1(connection: &Connection) {
    connection
        .execute_batch(
            "DROP TABLE embedding_scheme; \
             UPDATE metadata SET schema_version = 1 WHERE singleton = 1;",
        )
        .unwrap();
}

#[test]
fn store_home_keeps_every_generated_path_under_rust_v1() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));

    assert_eq!(home.indexes_dir(), tmp.path().join("state/rust-v1/indexes"));
    assert_eq!(
        home.vectors_path(),
        tmp.path().join("state/rust-v1/vectors.sqlite3")
    );
    assert_eq!(home.models_dir(), tmp.path().join("state/rust-v1/models"));
}

#[test]
fn project_filename_is_sanitized_readable_and_bound_to_the_canonical_root() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let first_parent = tmp.path().join("one");
    let second_parent = tmp.path().join("two");
    std::fs::create_dir_all(&first_parent).unwrap();
    std::fs::create_dir_all(&second_parent).unwrap();
    let first = create_project(&first_parent, "project #name?%");
    let second = create_project(&second_parent, "project #name?%");

    let canonical = first.canonicalize().unwrap();
    let digest = format!(
        "{:x}",
        Sha256::digest(canonical.to_str().unwrap().as_bytes())
    );
    let expected = format!("project-name-{}.sqlite3", &digest[..16]);
    let first_path = home.project_path(&first).unwrap();
    let second_path = home.project_path(&second).unwrap();

    assert_eq!(first_path.file_name().unwrap().to_str().unwrap(), expected);
    assert_eq!(first_path.parent(), Some(home.indexes_dir().as_path()));
    assert_ne!(first_path, second_path);
    assert!(
        !first_path
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .contains(['?', '#', '%', ' '])
    );
}

#[test]
fn sanitized_project_name_is_bounded_before_the_hash_suffix() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let long_name = format!("{} {}", "a".repeat(63), "b".repeat(120));
    let root = create_project(tmp.path(), &long_name);

    let path = home.project_path(&root).unwrap();
    let filename = path.file_name().unwrap().to_str().unwrap();

    assert!(filename.len() <= 64 + 1 + 16 + ".sqlite3".len());
}

#[test]
fn a_new_project_store_has_versioned_constrained_tables_and_fts5() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();

    let version: u32 = connection
        .query_row(
            "SELECT schema_version FROM metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let stored_root: String = connection
        .query_row("SELECT root FROM metadata WHERE singleton = 1", [], |row| {
            row.get(0)
        })
        .unwrap();
    let foreign_keys: i64 = connection
        .query_row("PRAGMA foreign_keys", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(stored_root, root.canonicalize().unwrap().to_str().unwrap());
    assert_eq!(foreign_keys, 1);
    assert!(constraint_failed(
        connection
            .execute(
                "INSERT INTO metadata(singleton, schema_version, root) \
                 VALUES (2, ?1, '/another/root')",
                [SCHEMA_VERSION],
            )
            .unwrap_err()
    ));

    connection
        .execute(
            "INSERT INTO docs \
             (path, corpus, status, content_hash, byte_length, mtime_ns, device, inode, generation) \
             VALUES (?1, 'shared', 'current', ?2, 5, 6, 7, 8, 1)",
            params!["/corpus/alpha.md", "a".repeat(64)],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO chunks \
             (path, ordinal, text, embed_text, vector_hash) \
             VALUES ('/corpus/alpha.md', 0, 'alpha', 'title: alpha', ?1)",
            ["b".repeat(64)],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO chunks_fts(rowid, text) \
             SELECT id, text FROM chunks WHERE path = '/corpus/alpha.md'",
            [],
        )
        .unwrap();
    let hits: i64 = connection
        .query_row(
            "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH 'alpha'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(hits, 1);

    let duplicate = connection
        .execute(
            "INSERT INTO chunks \
             (path, ordinal, text, embed_text, vector_hash) \
             VALUES ('/corpus/alpha.md', 0, 'again', 'again', ?1)",
            ["c".repeat(64)],
        )
        .unwrap_err();
    assert!(constraint_failed(duplicate));

    for sql in [
        "INSERT INTO chunks(path, ordinal, text, embed_text, vector_hash) \
         VALUES ('/corpus/alpha.md', -1, 'bad', 'bad', \
                 'dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd')",
        "INSERT INTO docs(path, corpus, status, content_hash, byte_length, mtime_ns, device, inode, generation) \
         VALUES ('/corpus/bad-corpus.md', 'foreign', NULL, \
                 'eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee', \
                 0, 0, 0, 0, 0)",
        "INSERT INTO docs(path, corpus, status, content_hash, byte_length, mtime_ns, device, inode, generation) \
         VALUES ('/corpus/bad-status.md', 'private', 'deleted', \
                 'ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff', \
                 0, 0, 0, 0, 0)",
    ] {
        assert!(constraint_failed(connection.execute(sql, []).unwrap_err()));
    }

    connection
        .execute("DELETE FROM docs WHERE path = '/corpus/alpha.md'", [])
        .unwrap();
    let remaining: i64 = connection
        .query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(remaining, 0);
    assert_eq!(integrity(&connection), "ok");
}

#[test]
fn a_new_project_store_has_the_constrained_v2_embedding_scheme_stamp() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();

    assert_eq!(SCHEMA_VERSION, 2);
    let rows: i64 = connection
        .query_row("SELECT count(*) FROM embedding_scheme", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(rows, 0);

    let signature = "a".repeat(64);
    connection
        .execute(
            "INSERT INTO embedding_scheme(singleton, signature, dimensions) \
             VALUES (1, ?1, 384)",
            [&signature],
        )
        .unwrap();
    for sql in [
        "INSERT INTO embedding_scheme(singleton, signature, dimensions) \
         VALUES (2, 'bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb', 384)",
        "UPDATE embedding_scheme SET signature = \
         'AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA'",
        "UPDATE embedding_scheme SET signature = \
         'ccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc'",
        "UPDATE embedding_scheme SET dimensions = 0",
    ] {
        assert!(constraint_failed(connection.execute(sql, []).unwrap_err()));
    }
    assert_eq!(integrity(&connection), "ok");
}

#[test]
fn embedding_scheme_constraint_rejects_a_nul_hidden_suffix() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    for signature in [
        format!("{}\0", "a".repeat(63)),
        format!("{}\0hidden", "a".repeat(64)),
    ] {
        let error = connection
            .execute(
                "INSERT INTO embedding_scheme(singleton, signature, dimensions) \
                 VALUES (1, ?1, 384)",
                [&signature],
            )
            .unwrap_err();

        assert!(constraint_failed(error));
    }
}

#[test]
fn open_project_refuses_an_adversarial_noncanonical_embedding_stamp() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    let signature = format!("{}\0hidden", "b".repeat(64));
    connection
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    connection
        .execute(
            "INSERT INTO embedding_scheme(singleton, signature, dimensions) \
             VALUES (1, ?1, 768)",
            [&signature],
        )
        .unwrap();
    connection
        .pragma_update(None, "ignore_check_constraints", "OFF")
        .unwrap();
    assert!(read_embedding_scheme(&connection).is_err());
    drop(connection);

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(error, StoreError::IncompatibleSchema { .. }));
}

#[test]
fn embedding_scheme_reads_and_project_open_refuse_a_noncanonical_singleton_shape() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    connection
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    connection
        .execute(
            "INSERT INTO embedding_scheme(singleton, signature, dimensions) \
             VALUES (2, ?1, 768)",
            ["c".repeat(64)],
        )
        .unwrap();
    connection
        .pragma_update(None, "ignore_check_constraints", "OFF")
        .unwrap();

    assert!(read_embedding_scheme(&connection).is_err());
    drop(connection);
    assert!(matches!(
        open_project(&home, &root),
        Err(StoreError::IncompatibleSchema { .. })
    ));
}

#[test]
fn embedding_scheme_read_attributes_invalid_dimensions_to_the_integer_column() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    connection
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    connection
        .execute(
            "INSERT INTO embedding_scheme(singleton, signature, dimensions) \
             VALUES (1, ?1, 0)",
            ["d".repeat(64)],
        )
        .unwrap();
    connection
        .pragma_update(None, "ignore_check_constraints", "OFF")
        .unwrap();

    let error = read_embedding_scheme(&connection).unwrap_err();

    assert!(matches!(
        error,
        rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Integer, _)
    ));
}

#[test]
fn embedding_scheme_values_validate_the_persisted_contract() {
    let scheme = EmbeddingScheme::new("a".repeat(64), 384).unwrap();

    assert_eq!(scheme.signature(), "a".repeat(64));
    assert_eq!(scheme.dimensions(), 384);
    assert_eq!(
        EmbeddingScheme::new("A".repeat(64), 384),
        Err(EmbeddingSchemeError::InvalidSignature)
    );
    assert_eq!(
        EmbeddingScheme::new("a".repeat(63), 384),
        Err(EmbeddingSchemeError::InvalidSignature)
    );
    assert_eq!(
        EmbeddingScheme::new("a".repeat(64), 0),
        Err(EmbeddingSchemeError::InvalidDimensions)
    );
    assert_eq!(
        EmbeddingScheme::new("a".repeat(64), usize::MAX),
        Err(EmbeddingSchemeError::InvalidDimensions)
    );
}

#[test]
fn embedding_scheme_api_sets_clears_and_rolls_back_with_the_caller_transaction() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let mut connection = open_project(&home, &root).unwrap();
    let scheme = EmbeddingScheme::new("b".repeat(64), 768).unwrap();

    assert_eq!(read_embedding_scheme(&connection).unwrap(), None);
    {
        let transaction = connection.transaction().unwrap();
        write_embedding_scheme(&transaction, Some(&scheme)).unwrap();
        transaction.rollback().unwrap();
    }
    assert_eq!(read_embedding_scheme(&connection).unwrap(), None);
    {
        let transaction = connection.transaction().unwrap();
        write_embedding_scheme(&transaction, Some(&scheme)).unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(read_embedding_scheme(&connection).unwrap(), Some(scheme));
    {
        let transaction = connection.transaction().unwrap();
        write_embedding_scheme(&transaction, None).unwrap();
        transaction.commit().unwrap();
    }
    assert_eq!(read_embedding_scheme(&connection).unwrap(), None);
}

#[test]
fn an_exact_v1_project_migrates_atomically_to_unstamped_v2_and_reopens() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    insert_doc_and_chunk(&connection, Some("alpha"));
    downgrade_project_to_exact_v1(&connection);
    drop(connection);

    let migrated = open_project(&home, &root).unwrap();

    let version: u32 = migrated
        .query_row(
            "SELECT schema_version FROM metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let chunks: i64 = migrated
        .query_row("SELECT count(*) FROM chunks", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 2);
    assert_eq!(chunks, 1);
    assert_eq!(read_embedding_scheme(&migrated).unwrap(), None);
    assert_eq!(integrity(&migrated), "ok");
    drop(migrated);

    let reopened = open_project(&home, &root).unwrap();
    assert_eq!(read_embedding_scheme(&reopened).unwrap(), None);
    assert_eq!(integrity(&reopened), "ok");
}

#[cfg(unix)]
#[test]
fn malformed_v1_schema_is_refused_without_migration_or_mutation() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    let path = database_path(&connection);
    downgrade_project_to_exact_v1(&connection);
    connection
        .execute("CREATE TABLE unexpected(value TEXT)", [])
        .unwrap();
    drop(connection);
    let before = persistent_file_snapshot(&path);

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(error, StoreError::IncompatibleSchema { .. }));
    assert_eq!(persistent_file_snapshot(&path), before);
}

#[cfg(unix)]
#[test]
fn wrong_root_v1_schema_is_refused_without_migration_or_mutation() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    let path = database_path(&connection);
    downgrade_project_to_exact_v1(&connection);
    connection
        .execute(
            "UPDATE metadata SET root = '/another/project' WHERE singleton = 1",
            [],
        )
        .unwrap();
    drop(connection);
    let before = persistent_file_snapshot(&path);

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(error, StoreError::WrongRoot { .. }));
    assert_eq!(persistent_file_snapshot(&path), before);
}

#[cfg(unix)]
#[test]
fn foreign_key_invalid_v1_schema_is_refused_without_migration_or_mutation() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    let path = database_path(&connection);
    downgrade_project_to_exact_v1(&connection);
    drop(connection);
    let raw = Connection::open(&path).unwrap();
    raw.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    raw.execute(
        "INSERT INTO chunks(path, ordinal, text, embed_text, vector_hash) \
         VALUES ('/missing.md', 0, 'orphan', 'orphan', ?1)",
        ["c".repeat(64)],
    )
    .unwrap();
    drop(raw);
    let before = persistent_file_snapshot(&path);

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(error, StoreError::Integrity { .. }));
    assert_eq!(persistent_file_snapshot(&path), before);
}

#[test]
fn opening_through_a_symlink_stamps_the_canonical_project_root() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let alias = tmp.path().join("alias");
    std::os::unix::fs::symlink(&root, &alias).unwrap();

    let connection = open_project(&home, &alias).unwrap();
    let stored: String = connection
        .query_row("SELECT root FROM metadata WHERE singleton = 1", [], |row| {
            row.get(0)
        })
        .unwrap();

    assert_eq!(stored, root.canonicalize().unwrap().to_str().unwrap());
    assert_eq!(
        database_path(&connection).canonicalize().unwrap(),
        home.project_path(&root).unwrap().canonicalize().unwrap()
    );
}

#[test]
fn a_regular_file_is_not_a_project_root_and_creates_no_store() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = tmp.path().join("project.txt");
    std::fs::write(&root, "not a project directory").unwrap();

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(
        error,
        StoreError::NotDirectory {
            kind: PathKind::Project,
            ref path,
        } if path == &root.canonicalize().unwrap()
    ));
    assert!(!home.indexes_dir().exists());
}

#[test]
fn shared_vectors_are_separate_constrained_and_use_wal() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let connection = open_vectors(&home).unwrap();

    assert_eq!(
        database_path(&connection).canonicalize().unwrap(),
        home.vectors_path().canonicalize().unwrap()
    );
    let journal: String = connection
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");
    connection
        .execute(
            "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
             VALUES ('key', 'model-v1', 2, X'0000000000000000')",
            [],
        )
        .unwrap();
    assert!(constraint_failed(
        connection
            .execute(
                "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
                 VALUES ('key', 'model-v1', 2, X'0000000000000000')",
                [],
            )
            .unwrap_err()
    ));
    assert!(constraint_failed(
        connection
            .execute(
                "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
                 VALUES ('negative', 'model-v1', -1, X'')",
                [],
            )
            .unwrap_err()
    ));
    assert_eq!(integrity(&connection), "ok");
    drop(connection);

    let reopened = open_vectors(&home).unwrap();
    let rows: i64 = reopened
        .query_row("SELECT count(*) FROM vectors", [], |row| row.get(0))
        .unwrap();
    assert_eq!(rows, 1);
    assert_eq!(integrity(&reopened), "ok");
}

#[test]
fn malformed_vector_schema_is_typed_and_refused_before_enabling_wal() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    create_private_vector_store_dir(&home);
    let malformed = Connection::open(home.vectors_path()).unwrap();
    malformed
        .execute("CREATE TABLE vectors(hash TEXT PRIMARY KEY)", [])
        .unwrap();
    drop(malformed);
    set_mode(&home.vectors_path(), 0o600);
    let before = persistent_file_snapshot(&home.vectors_path());

    let error = open_vectors(&home).unwrap_err();

    assert!(matches!(error, StoreError::IncompatibleVectorSchema { .. }));
    assert_eq!(persistent_file_snapshot(&home.vectors_path()), before);
}

#[test]
fn wrong_root_is_typed_and_refused_without_modifying_the_database() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    let path = database_path(&connection);
    connection
        .execute(
            "UPDATE metadata SET root = '/another/project' WHERE singleton = 1",
            [],
        )
        .unwrap();
    drop(connection);
    let before = persistent_file_snapshot(&path);

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(
        error,
        StoreError::WrongRoot { ref found, .. } if found == Path::new("/another/project")
    ));
    assert_eq!(persistent_file_snapshot(&path), before);
}

#[test]
fn future_schema_is_typed_and_refused_without_modifying_the_database() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    let path = database_path(&connection);
    let future = SCHEMA_VERSION + 1;
    connection
        .execute(
            "UPDATE metadata SET schema_version = ?1 WHERE singleton = 1",
            [future],
        )
        .unwrap();
    drop(connection);
    let before = persistent_file_snapshot(&path);

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(
        error,
        StoreError::FutureSchema { found, supported }
            if found == u64::from(future) && supported == SCHEMA_VERSION
    ));
    assert_eq!(persistent_file_snapshot(&path), before);
}

#[test]
fn future_version_is_refused_before_reading_current_version_columns() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let path = home.project_path(&root).unwrap();
    create_private_project_store_dirs(&home);
    let future = Connection::open(&path).unwrap();
    future
        .execute_batch(
            "CREATE TABLE metadata(\
                 singleton INTEGER PRIMARY KEY, \
                 schema_version INTEGER NOT NULL\
             );",
        )
        .unwrap();
    future
        .execute(
            "INSERT INTO metadata(singleton, schema_version) VALUES (1, ?1)",
            [SCHEMA_VERSION + 1],
        )
        .unwrap();
    drop(future);
    set_mode(&path, 0o600);
    let before = persistent_file_snapshot(&path);

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(
        error,
        StoreError::FutureSchema { found, supported }
            if found == u64::from(SCHEMA_VERSION + 1) && supported == SCHEMA_VERSION
    ));
    assert_eq!(persistent_file_snapshot(&path), before);

    let repeated = open_project(&home, &root).unwrap_err();
    assert!(matches!(repeated, StoreError::FutureSchema { .. }));
    assert_eq!(persistent_file_snapshot(&path), before);
}

#[test]
fn an_existing_empty_project_file_is_initialized_atomically() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let path = home.project_path(&root).unwrap();
    create_private_project_store_dirs(&home);
    std::fs::File::create(&path).unwrap();
    set_mode(&path, 0o600);

    let connection = open_project(&home, &root).unwrap();

    let version: u32 = connection
        .query_row(
            "SELECT schema_version FROM metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    assert_eq!(integrity(&connection), "ok");
}

#[test]
fn an_exact_version_store_for_the_same_root_reopens() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let first = open_project(&home, &root).unwrap();
    let path = database_path(&first);
    drop(first);

    let reopened = open_project(&home, &root).unwrap();

    assert_eq!(database_path(&reopened), path);
    assert_eq!(integrity(&reopened), "ok");
}

#[test]
fn malformed_existing_schema_is_typed_and_never_reinitialized() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let path = home.project_path(&root).unwrap();
    create_private_project_store_dirs(&home);
    let foreign = Connection::open(&path).unwrap();
    foreign
        .execute("CREATE TABLE unrelated(value TEXT)", [])
        .unwrap();
    drop(foreign);
    set_mode(&path, 0o600);
    let before = std::fs::read(&path).unwrap();

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(error, StoreError::IncompatibleSchema { .. }));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn a_matching_metadata_stamp_without_the_versioned_schema_is_malformed() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let path = home.project_path(&root).unwrap();
    create_private_project_store_dirs(&home);
    let malformed = Connection::open(&path).unwrap();
    malformed
        .execute_batch(
            "CREATE TABLE metadata(\
                 singleton INTEGER PRIMARY KEY, \
                 schema_version INTEGER NOT NULL, \
                 root TEXT NOT NULL\
             );",
        )
        .unwrap();
    malformed
        .execute(
            "INSERT INTO metadata(singleton, schema_version, root) VALUES (1, ?1, ?2)",
            params![
                SCHEMA_VERSION,
                root.canonicalize().unwrap().to_str().unwrap()
            ],
        )
        .unwrap();
    drop(malformed);
    set_mode(&path, 0o600);
    let before = std::fs::read(&path).unwrap();

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(error, StoreError::IncompatibleSchema { .. }));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn matching_names_without_versioned_schema_semantics_are_malformed() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let path = home.project_path(&root).unwrap();
    create_private_project_store_dirs(&home);
    let malformed = Connection::open(&path).unwrap();
    malformed
        .execute_batch(
            "CREATE TABLE metadata(\
                 singleton INTEGER PRIMARY KEY, schema_version INTEGER, \
                 root TEXT, generation INTEGER\
             );\
             CREATE TABLE docs(\
                 path TEXT PRIMARY KEY, corpus TEXT, status TEXT, content_hash TEXT, \
                 byte_length INTEGER, mtime_ns INTEGER, device INTEGER, inode INTEGER, \
                 generation INTEGER\
             );\
             CREATE TABLE chunks(\
                 id INTEGER PRIMARY KEY, path TEXT, ordinal INTEGER, text TEXT, \
                 embed_text TEXT, vector_hash TEXT\
             );\
             CREATE INDEX chunks_by_path ON chunks(path);\
             CREATE VIRTUAL TABLE chunks_fts USING fts5(\
                 text, tokenize = 'porter unicode61'\
             );",
        )
        .unwrap();
    malformed
        .execute(
            "INSERT INTO metadata(singleton, schema_version, root, generation) \
             VALUES (1, ?1, ?2, 0)",
            params![
                SCHEMA_VERSION,
                root.canonicalize().unwrap().to_str().unwrap()
            ],
        )
        .unwrap();
    drop(malformed);
    set_mode(&path, 0o600);
    let before = std::fs::read(&path).unwrap();

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(error, StoreError::IncompatibleSchema { .. }));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[test]
fn schema_fingerprint_preserves_case_inside_constraint_literals() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    let path = database_path(&connection);
    connection
        .execute_batch(
            "PRAGMA writable_schema = ON;\
             UPDATE sqlite_schema \
             SET sql = replace(sql, '''shared''', '''SHARED''') \
             WHERE type = 'table' AND name = 'docs';\
             PRAGMA writable_schema = OFF;",
        )
        .unwrap();
    drop(connection);
    let changed_sql: String = Connection::open(&path)
        .unwrap()
        .query_row(
            "SELECT sql FROM sqlite_schema WHERE name = 'docs'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(changed_sql.contains("'SHARED'"));
    let before = persistent_file_snapshot(&path);

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(error, StoreError::IncompatibleSchema { .. }));
    assert_eq!(persistent_file_snapshot(&path), before);
}

#[test]
fn older_existing_schema_is_typed_and_never_migrated_implicitly() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let path = home.project_path(&root).unwrap();
    create_private_project_store_dirs(&home);
    let older = Connection::open(&path).unwrap();
    older
        .execute_batch(
            "CREATE TABLE metadata(\
                 singleton INTEGER PRIMARY KEY, \
                 schema_version INTEGER NOT NULL, \
                 root TEXT NOT NULL\
             );",
        )
        .unwrap();
    older
        .execute(
            "INSERT INTO metadata(singleton, schema_version, root) VALUES (1, 0, ?1)",
            [root.canonicalize().unwrap().to_str().unwrap()],
        )
        .unwrap();
    drop(older);
    set_mode(&path, 0o600);
    let before = std::fs::read(&path).unwrap();

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(
        error,
        StoreError::UnsupportedSchema { found: 0, supported }
            if supported == SCHEMA_VERSION
    ));
    assert_eq!(std::fs::read(path).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn non_utf8_project_and_corpus_paths_return_typed_errors() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = tmp
        .path()
        .join(OsString::from_vec(b"project-\xff".to_vec()));

    assert!(matches!(
        open_project(&home, &root),
        Err(StoreError::NonUtf8Path {
            kind: PathKind::Project,
            ..
        })
    ));
    assert!(matches!(
        path_as_utf8(&root, PathKind::Corpus),
        Err(StoreError::NonUtf8Path {
            kind: PathKind::Corpus,
            ..
        })
    ));
}

#[cfg(unix)]
#[test]
fn store_directories_do_not_follow_symlinks_outside_the_home() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();

    let vector_home = StoreHome::new(tmp.path().join("vector-state"));
    let vector_outside = tmp.path().join("vector-outside");
    std::fs::create_dir_all(version_dir(&vector_home).parent().unwrap()).unwrap();
    std::fs::create_dir(&vector_outside).unwrap();
    symlink(&vector_outside, version_dir(&vector_home)).unwrap();

    let vector_result = open_vectors(&vector_home);

    assert!(matches!(
        vector_result,
        Err(StoreError::UnsafeStoreDirectory { .. })
    ));
    assert!(!vector_outside.join("vectors.sqlite3").exists());

    let project_home = StoreHome::new(tmp.path().join("project-state"));
    let project_root = create_project(tmp.path(), "project");
    let project_outside = tmp.path().join("project-outside");
    std::fs::create_dir_all(version_dir(&project_home)).unwrap();
    set_mode(&version_dir(&project_home), 0o700);
    std::fs::create_dir(&project_outside).unwrap();
    symlink(&project_outside, project_home.indexes_dir()).unwrap();

    let project_result = open_project(&project_home, &project_root);

    assert!(matches!(
        project_result,
        Err(StoreError::UnsafeStoreDirectory { .. })
    ));
    assert!(
        std::fs::read_dir(&project_outside)
            .unwrap()
            .next()
            .is_none()
    );
}

#[cfg(unix)]
#[test]
fn database_hardlinks_are_refused_without_modifying_the_other_link() {
    let tmp = tempdir().unwrap();

    let project_home = StoreHome::new(tmp.path().join("project-state"));
    let project_root = create_project(tmp.path(), "project");
    std::fs::create_dir_all(project_home.indexes_dir()).unwrap();
    set_mode(&version_dir(&project_home), 0o700);
    set_mode(&project_home.indexes_dir(), 0o700);
    let project_path = project_home.project_path(&project_root).unwrap();
    let project_victim = tmp.path().join("project-victim");
    std::fs::write(&project_victim, b"").unwrap();
    set_mode(&project_victim, 0o600);
    std::fs::hard_link(&project_victim, &project_path).unwrap();

    let project_result = open_project(&project_home, &project_root);

    assert!(matches!(
        project_result,
        Err(StoreError::UnsafeStoreFile { .. })
    ));
    assert_eq!(std::fs::read(&project_victim).unwrap(), b"");

    let vector_home = StoreHome::new(tmp.path().join("vector-state"));
    std::fs::create_dir_all(version_dir(&vector_home)).unwrap();
    set_mode(&version_dir(&vector_home), 0o700);
    let vector_victim = tmp.path().join("vector-victim");
    std::fs::write(&vector_victim, b"").unwrap();
    set_mode(&vector_victim, 0o600);
    std::fs::hard_link(&vector_victim, vector_home.vectors_path()).unwrap();

    let vector_result = open_vectors(&vector_home);

    assert!(matches!(
        vector_result,
        Err(StoreError::UnsafeStoreFile { .. })
    ));
    assert_eq!(std::fs::read(&vector_victim).unwrap(), b"");
}

#[cfg(unix)]
#[test]
fn database_symlinks_are_refused_without_opening_their_targets() {
    use std::os::unix::fs::symlink;

    let tmp = tempdir().unwrap();

    let project_home = StoreHome::new(tmp.path().join("project-state"));
    let project_root = create_project(tmp.path(), "project");
    create_private_project_store_dirs(&project_home);
    let project_path = project_home.project_path(&project_root).unwrap();
    let project_victim = tmp.path().join("project-victim");
    std::fs::write(&project_victim, b"project victim").unwrap();
    set_mode(&project_victim, 0o600);
    symlink(&project_victim, &project_path).unwrap();

    let project_result = open_project(&project_home, &project_root);

    assert!(matches!(project_result, Err(StoreError::NotRegular { .. })));
    assert_eq!(std::fs::read(&project_victim).unwrap(), b"project victim");

    let vector_home = StoreHome::new(tmp.path().join("vector-state"));
    create_private_vector_store_dir(&vector_home);
    let vector_victim = tmp.path().join("vector-victim");
    std::fs::write(&vector_victim, b"vector victim").unwrap();
    set_mode(&vector_victim, 0o600);
    symlink(&vector_victim, vector_home.vectors_path()).unwrap();

    let vector_result = open_vectors(&vector_home);

    assert!(matches!(vector_result, Err(StoreError::NotRegular { .. })));
    assert_eq!(std::fs::read(&vector_victim).unwrap(), b"vector victim");
}

#[cfg(unix)]
#[test]
fn public_database_modes_are_refused_without_repair_or_initialization() {
    let tmp = tempdir().unwrap();

    let project_home = StoreHome::new(tmp.path().join("project-state"));
    let project_root = create_project(tmp.path(), "project");
    create_private_project_store_dirs(&project_home);
    let project_path = project_home.project_path(&project_root).unwrap();
    std::fs::write(&project_path, b"").unwrap();
    set_mode(&project_path, 0o644);
    let project_before = persistent_file_snapshot(&project_path);

    let project_result = open_project(&project_home, &project_root);

    assert!(matches!(
        project_result,
        Err(StoreError::UnsafeStoreFile { .. })
    ));
    assert_eq!(persistent_file_snapshot(&project_path), project_before);

    let vector_home = StoreHome::new(tmp.path().join("vector-state"));
    create_private_vector_store_dir(&vector_home);
    std::fs::write(vector_home.vectors_path(), b"").unwrap();
    set_mode(&vector_home.vectors_path(), 0o644);
    let vector_before = persistent_file_snapshot(&vector_home.vectors_path());

    let vector_result = open_vectors(&vector_home);

    assert!(matches!(
        vector_result,
        Err(StoreError::UnsafeStoreFile { .. })
    ));
    assert_eq!(
        persistent_file_snapshot(&vector_home.vectors_path()),
        vector_before
    );
}

#[cfg(unix)]
#[test]
fn crash_sidecars_are_refused_without_any_persistent_mutation() {
    for suffix in ["-wal", "-shm", "-journal"] {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("project-state"));
        let root = create_project(tmp.path(), "project");
        let connection = open_project(&home, &root).unwrap();
        let path = database_path(&connection);
        drop(connection);
        let sidecar = sidecar_path(&path, suffix);
        std::fs::write(&sidecar, format!("crash residue {suffix}")).unwrap();
        let main_before = persistent_file_snapshot(&path);
        let sidecar_before = persistent_file_snapshot(&sidecar);
        let expected_sidecar = sidecar.canonicalize().unwrap();

        let result = open_project(&home, &root);

        assert!(
            matches!(result, Err(StoreError::UnsafeSidecar { ref path }) if path == &expected_sidecar),
            "project store did not type {suffix} as an unsafe sidecar"
        );
        assert_eq!(persistent_file_snapshot(&path), main_before);
        assert_eq!(persistent_file_snapshot(&sidecar), sidecar_before);
    }

    for suffix in ["-wal", "-shm", "-journal"] {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join("vector-state"));
        drop(open_vectors(&home).unwrap());
        let path = home.vectors_path();
        let sidecar = sidecar_path(&path, suffix);
        std::fs::write(&sidecar, format!("crash residue {suffix}")).unwrap();
        let main_before = persistent_file_snapshot(&path);
        let sidecar_before = persistent_file_snapshot(&sidecar);
        let expected_sidecar = sidecar.canonicalize().unwrap();

        let result = open_vectors(&home);

        assert!(
            matches!(result, Err(StoreError::UnsafeSidecar { ref path }) if path == &expected_sidecar),
            "vector store did not type {suffix} as an unsafe sidecar"
        );
        assert_eq!(persistent_file_snapshot(&path), main_before);
        assert_eq!(persistent_file_snapshot(&sidecar), sidecar_before);
    }
}

#[cfg(unix)]
#[test]
fn vector_opener_adopts_exact_empty_reader_sidecars() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let path = materialize_empty_vector_reader_sidecars(&home);
    let wal = sidecar_path(&path, "-wal");
    let shm = sidecar_path(&path, "-shm");
    assert_eq!(std::fs::metadata(&wal).unwrap().len(), 0);
    assert_eq!(std::fs::metadata(&shm).unwrap().len(), 32_768);
    assert_eq!(mode(&wal), 0o600);
    assert_eq!(mode(&shm), 0o600);

    let reopened = open_vectors(&home).unwrap();

    assert_eq!(
        reopened
            .query_row("SELECT count(*) FROM vectors", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[cfg(unix)]
#[test]
fn vector_opener_coordinates_with_a_live_ordinary_reader() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    drop(open_vectors(&home).unwrap());
    let path = home.vectors_path().canonicalize().unwrap();
    let reader = open_vector_reader(&path);
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM vectors", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );

    let writer = open_vectors(&home).unwrap();

    assert_eq!(
        writer
            .query_row("SELECT count(*) FROM vectors", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
    assert_eq!(
        reader
            .query_row("SELECT count(*) FROM vectors", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[cfg(unix)]
#[test]
fn vector_opener_refuses_every_reader_sidecar_deviation_without_mutation() {
    for case in [
        "nonempty-wal",
        "odd-shm-size",
        "wal-mode",
        "shm-mode",
        "journal",
        "missing-wal",
        "missing-shm",
        "wal-hardlink",
        "shm-symlink",
    ] {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join(case));
        let path = materialize_empty_vector_reader_sidecars(&home);
        let wal = sidecar_path(&path, "-wal");
        let shm = sidecar_path(&path, "-shm");
        let journal = sidecar_path(&path, "-journal");
        match case {
            "nonempty-wal" => std::fs::write(&wal, b"not empty").unwrap(),
            "odd-shm-size" => std::fs::OpenOptions::new()
                .write(true)
                .open(&shm)
                .unwrap()
                .set_len(32_767)
                .unwrap(),
            "wal-mode" => set_mode(&wal, 0o644),
            "shm-mode" => set_mode(&shm, 0o644),
            "journal" => std::fs::write(&journal, b"rollback residue").unwrap(),
            "missing-wal" => std::fs::remove_file(&wal).unwrap(),
            "missing-shm" => std::fs::remove_file(&shm).unwrap(),
            "wal-hardlink" => std::fs::hard_link(&wal, tmp.path().join("extra-wal-link")).unwrap(),
            "shm-symlink" => {
                std::fs::remove_file(&shm).unwrap();
                std::os::unix::fs::symlink(tmp.path().join("victim"), &shm).unwrap();
            }
            _ => unreachable!(),
        }
        let main_before = persistent_file_snapshot(&path);
        let wal_before = std::fs::symlink_metadata(&wal).ok();
        let shm_before = std::fs::symlink_metadata(&shm).ok();
        let wal_bytes = std::fs::read(&wal).ok();
        let shm_bytes = std::fs::read(&shm).ok();

        let result = open_vectors(&home);

        assert!(
            matches!(result, Err(StoreError::UnsafeSidecar { .. })),
            "vector store accepted hostile reader sidecars for {case}"
        );
        assert_eq!(persistent_file_snapshot(&path), main_before);
        let wal_after = std::fs::symlink_metadata(&wal).ok();
        let shm_after = std::fs::symlink_metadata(&shm).ok();
        assert_eq!(
            wal_after
                .as_ref()
                .map(|metadata| (metadata.dev(), metadata.ino())),
            wal_before
                .as_ref()
                .map(|metadata| (metadata.dev(), metadata.ino()))
        );
        assert_eq!(
            shm_after
                .as_ref()
                .map(|metadata| (metadata.dev(), metadata.ino())),
            shm_before
                .as_ref()
                .map(|metadata| (metadata.dev(), metadata.ino()))
        );
        assert_eq!(std::fs::read(&wal).ok(), wal_bytes);
        assert_eq!(std::fs::read(&shm).ok(), shm_bytes);
    }
}

#[cfg(unix)]
#[test]
fn a_clean_project_database_in_wal_mode_is_refused_before_writable_open() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    let path = database_path(&connection);
    drop(connection);
    let raw = Connection::open(&path).unwrap();
    let mode: String = raw
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    drop(raw);
    assert!(!sidecar_path(&path, "-wal").exists());
    assert!(!sidecar_path(&path, "-shm").exists());
    let before = persistent_file_snapshot(&path);

    let error = open_project(&home, &root).unwrap_err();

    assert!(matches!(
        error,
        StoreError::UnexpectedJournalMode { ref found, .. } if found == "wal"
    ));
    assert_eq!(persistent_file_snapshot(&path), before);
}

#[test]
fn project_schema_rejects_every_kind_of_unexpected_persistent_object() {
    let cases = [
        (
            "table",
            "CREATE TABLE unexpected_table(value TEXT)".to_owned(),
        ),
        (
            "index",
            "CREATE INDEX unexpected_index ON docs(byte_length)".to_owned(),
        ),
        (
            "view",
            "CREATE VIEW unexpected_view AS SELECT path FROM docs".to_owned(),
        ),
        (
            "trigger",
            "CREATE TRIGGER unexpected_trigger AFTER INSERT ON docs \
             BEGIN SELECT 1; END"
                .to_owned(),
        ),
    ];
    let mut accepted = Vec::new();

    for (ordinal, (kind, sql)) in cases.into_iter().enumerate() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join(format!("state-{ordinal}")));
        let root = create_project(tmp.path(), &format!("project-{ordinal}"));
        let connection = open_project(&home, &root).unwrap();
        connection.execute_batch(&sql).unwrap();
        drop(connection);
        if !matches!(
            open_project(&home, &root),
            Err(StoreError::IncompatibleSchema { .. })
        ) {
            accepted.push(kind);
        }
    }

    assert!(accepted.is_empty(), "accepted unexpected {accepted:?}");
}

#[test]
fn vector_schema_rejects_every_kind_of_unexpected_persistent_object() {
    let cases = [
        (
            "table",
            "CREATE TABLE unexpected_table(value TEXT)".to_owned(),
        ),
        (
            "index",
            "CREATE INDEX unexpected_index ON vectors(model_signature)".to_owned(),
        ),
        (
            "view",
            "CREATE VIEW unexpected_view AS SELECT hash FROM vectors".to_owned(),
        ),
        (
            "trigger",
            "CREATE TRIGGER unexpected_trigger AFTER INSERT ON vectors \
             BEGIN SELECT 1; END"
                .to_owned(),
        ),
    ];
    let mut accepted = Vec::new();

    for (ordinal, (kind, sql)) in cases.into_iter().enumerate() {
        let tmp = tempdir().unwrap();
        let home = StoreHome::new(tmp.path().join(format!("state-{ordinal}")));
        let connection = open_vectors(&home).unwrap();
        connection.execute_batch(&sql).unwrap();
        drop(connection);
        if !matches!(
            open_vectors(&home),
            Err(StoreError::IncompatibleVectorSchema { .. })
        ) {
            accepted.push(kind);
        }
    }

    assert!(accepted.is_empty(), "accepted unexpected {accepted:?}");
}

#[test]
fn project_integrity_rejects_orphan_chunks() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    let path = database_path(&connection);
    drop(connection);
    let raw = Connection::open(path).unwrap();
    raw.execute_batch("PRAGMA foreign_keys = OFF;").unwrap();
    raw.execute(
        "INSERT INTO chunks(path, ordinal, text, embed_text, vector_hash) \
         VALUES ('/missing.md', 0, 'orphan', 'orphan', ?1)",
        ["c".repeat(64)],
    )
    .unwrap();
    drop(raw);

    assert!(matches!(
        open_project(&home, &root),
        Err(StoreError::Integrity { .. })
    ));
}

#[test]
fn project_integrity_rejects_missing_fts_rows() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    insert_doc_and_chunk(&connection, None);
    drop(connection);

    assert!(matches!(
        open_project(&home, &root),
        Err(StoreError::Integrity { .. })
    ));
}

#[test]
fn project_integrity_rejects_phantom_fts_rows() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    connection
        .execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (999, 'phantom')",
            [],
        )
        .unwrap();
    drop(connection);

    assert!(matches!(
        open_project(&home, &root),
        Err(StoreError::Integrity { .. })
    ));
}

#[test]
fn project_integrity_rejects_mismatched_fts_text() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    let root = create_project(tmp.path(), "project");
    let connection = open_project(&home, &root).unwrap();
    insert_doc_and_chunk(&connection, Some("different"));
    drop(connection);

    assert!(matches!(
        open_project(&home, &root),
        Err(StoreError::Integrity { .. })
    ));
}

#[cfg(unix)]
#[test]
fn created_store_directories_and_sqlite_files_are_private() {
    let tmp = tempdir().unwrap();
    let project_home = StoreHome::new(tmp.path().join("project-state"));
    let root = create_project(tmp.path(), "project");
    let project = open_project(&project_home, &root).unwrap();
    let project_path = database_path(&project);

    assert_eq!(mode(&version_dir(&project_home)), 0o700);
    assert_eq!(mode(&project_home.indexes_dir()), 0o700);
    assert_eq!(mode(&project_path), 0o600);
    drop(project);

    let vector_home = StoreHome::new(tmp.path().join("vector-state"));
    let vectors = open_vectors(&vector_home).unwrap();
    let vector_path = database_path(&vectors);

    assert_eq!(mode(&version_dir(&vector_home)), 0o700);
    assert_eq!(mode(&vector_path), 0o600);
    assert_eq!(mode(&sidecar_path(&vector_path, "-init.lock")), 0o600);
    assert_eq!(mode(&sidecar_path(&vector_path, "-wal")), 0o600);
    assert_eq!(mode(&sidecar_path(&vector_path, "-shm")), 0o600);
}

#[cfg(unix)]
#[test]
fn missing_store_home_is_created_private_under_permissive_umask() {
    const CHILD_ENV: &str = "YAMS_STORE_UMASK_CHILD";
    const STATE_ENV: &str = "YAMS_STORE_UMASK_STATE";

    if std::env::var_os(CHILD_ENV).is_some() {
        rustix::process::umask(rustix::fs::Mode::from(0o002));
        let state = PathBuf::from(std::env::var_os(STATE_ENV).unwrap());
        let home = StoreHome::new(&state);

        let connection = open_vectors(&home).unwrap();
        drop(connection);

        assert_eq!(mode(state.parent().unwrap()), 0o700);
        assert_eq!(mode(&state), 0o700);
        assert_eq!(mode(&version_dir(&home)), 0o700);
        return;
    }

    let tmp = tempdir().unwrap();
    let state = tmp.path().join("nested/state");
    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "missing_store_home_is_created_private_under_permissive_umask",
            "--nocapture",
        ])
        .env(CHILD_ENV, "1")
        .env(STATE_ENV, &state)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "child failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[cfg(unix)]
#[test]
fn existing_public_store_directory_is_refused() {
    let tmp = tempdir().unwrap();
    let home = StoreHome::new(tmp.path().join("state"));
    std::fs::create_dir_all(version_dir(&home)).unwrap();
    set_mode(&version_dir(&home), 0o755);

    let result = open_vectors(&home);

    assert!(matches!(
        result,
        Err(StoreError::UnsafeStoreDirectory { .. })
    ));
    assert!(!home.vectors_path().exists());
}
