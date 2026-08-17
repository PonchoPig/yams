use std::collections::BTreeSet;
use std::ptr;
use std::sync::mpsc;
use std::thread;

use rusqlite::{Connection, TransactionBehavior, params};
use tempfile::{TempDir, tempdir};
use yams_embed::{Embedding, EmbeddingRole};
use yams_store::{
    DenseChunk, EmbeddingScheme, LEXICAL_OVERFETCH_CAP, LexicalChunk, RetrievalError,
    RetrievalSnapshot, StoreHome, VectorCache, VectorError, VectorInsert, VectorKey, fts_query,
    open_project, vector_key, write_embedding_scheme,
};

const MODEL: &str = "fictional-model-v1";

fn lexical_candidates(
    project: &Connection,
    query: &str,
) -> Result<Vec<LexicalChunk>, RetrievalError> {
    RetrievalSnapshot::begin(project)?.lexical_candidates(query)
}

fn dense_candidates(
    project: &Connection,
    cache: &VectorCache,
    query: &Embedding,
    expected_scheme: &EmbeddingScheme,
    expected_model_signature: &str,
) -> Result<Vec<DenseChunk>, RetrievalError> {
    RetrievalSnapshot::begin(project)?.dense_candidates(
        cache,
        query,
        expected_scheme,
        expected_model_signature,
    )
}

fn dense_ranking(
    project: &Connection,
    cache: &VectorCache,
    query: &Embedding,
    expected_scheme: &EmbeddingScheme,
    expected_model_signature: &str,
) -> Result<Vec<yams_core::DenseRankedPage>, RetrievalError> {
    RetrievalSnapshot::begin(project)?.dense_ranking(
        cache,
        query,
        expected_scheme,
        expected_model_signature,
    )
}

#[test]
fn fts_query_quotes_the_shared_core_tokenizer() {
    assert_eq!(
        fts_query("How is Alpha-beta NEAR(foo) x _ok and C++?"),
        Some("\"alpha\" OR \"beta\" OR \"near\" OR \"foo\" OR \"_ok\"".to_owned())
    );
    assert_eq!(
        fts_query("naïve café Δelta"),
        Some("\"naïve\" OR \"café\" OR \"δelta\"".to_owned())
    );
    assert_eq!(
        fts_query("what is this"),
        Some("\"what\" OR \"is\" OR \"this\"".to_owned())
    );
    assert_eq!(fts_query("' -- \u{1b}[;m Δ"), None);
}

fn lexical_fixture() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "CREATE TABLE metadata(singleton INTEGER, generation INTEGER); \
             INSERT INTO metadata(singleton, generation) VALUES (1, 0); \
             CREATE TABLE embedding_scheme( \
                 singleton INTEGER, signature TEXT, dimensions INTEGER \
             ); \
             CREATE TABLE docs(path TEXT); \
             CREATE TABLE chunks(id INTEGER, path TEXT, ordinal INTEGER); \
             CREATE VIRTUAL TABLE chunks_fts USING fts5( \
                 text, tokenize = 'porter unicode61' \
             );",
        )
        .unwrap();
    connection
}

fn insert_lexical_chunk(connection: &Connection, rowid: i64, path: &str, ordinal: i64, text: &str) {
    connection
        .execute("INSERT INTO docs(path) VALUES (?1)", [path])
        .unwrap();
    connection
        .execute(
            "INSERT INTO chunks(id, path, ordinal) VALUES (?1, ?2, ?3)",
            params![rowid, path, ordinal],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
            params![rowid, text],
        )
        .unwrap();
}

#[test]
fn lexical_candidates_keep_chunk_order_and_cap_the_unfiltered_stream_at_200() {
    let connection = lexical_fixture();
    for rowid in (1_i64..=205).rev() {
        insert_lexical_chunk(
            &connection,
            rowid,
            &format!("/fiction/page-{rowid:03}.md"),
            0,
            "sharedtoken",
        );
    }

    let candidates = lexical_candidates(&connection, "sharedtoken").unwrap();

    assert_eq!(candidates.len(), LEXICAL_OVERFETCH_CAP);
    assert_eq!(candidates[0].rank(), 1);
    assert_eq!(candidates[0].rowid(), 1);
    assert_eq!(
        candidates[0].chunk().page().as_str(),
        "/fiction/page-001.md"
    );
    assert_eq!(candidates[LEXICAL_OVERFETCH_CAP - 1].rank(), 200);
    assert_eq!(candidates[LEXICAL_OVERFETCH_CAP - 1].rowid(), 200);
    assert!(
        candidates
            .windows(2)
            .all(|pair| pair[0].bm25() <= pair[1].bm25())
    );
    assert_eq!(candidates[0].as_candidate().chunk(), candidates[0].chunk());
}

#[test]
fn lexical_candidates_return_a_typed_error_for_a_missing_chunk_ordinal() {
    let connection = lexical_fixture();
    connection
        .execute(
            "INSERT INTO docs(path) VALUES ('/fiction/malformed.md')",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO chunks(id, path, ordinal) \
             VALUES (1, '/fiction/malformed.md', NULL)",
            [],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (1, 'malformedtoken')",
            [],
        )
        .unwrap();

    let error = lexical_candidates(&connection, "malformedtoken").unwrap_err();

    assert!(matches!(
        error,
        RetrievalError::MissingChunkOrdinal { ref path }
            if path == std::path::Path::new("/fiction/malformed.md")
    ));
}

#[test]
fn lexical_candidates_preserve_the_best_bm25_chunk_for_downstream_page_collapse() {
    let connection = lexical_fixture();
    connection
        .execute("INSERT INTO docs(path) VALUES ('/fiction/multi.md')", [])
        .unwrap();
    connection
        .execute("INSERT INTO docs(path) VALUES ('/fiction/other.md')", [])
        .unwrap();
    for (rowid, path, ordinal, text) in [
        (7, "/fiction/multi.md", 0, "rareone raretwo"),
        (8, "/fiction/multi.md", 1, "rareone filler filler filler"),
        (9, "/fiction/other.md", 0, "rareone elsewhere"),
    ] {
        connection
            .execute(
                "INSERT INTO chunks(id, path, ordinal) VALUES (?1, ?2, ?3)",
                params![rowid, path, ordinal],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
                params![rowid, text],
            )
            .unwrap();
    }

    let candidates = lexical_candidates(&connection, "rareone raretwo").unwrap();

    assert_eq!(candidates[0].rowid(), 7);
    assert_eq!(candidates[0].chunk().ordinal(), 0);
    assert!(
        candidates
            .iter()
            .any(|candidate| candidate.chunk().ordinal() == 1)
    );
    assert_eq!(
        candidates
            .iter()
            .filter(|candidate| candidate.chunk().page().as_str() == "/fiction/multi.md")
            .count(),
        2
    );
}

#[test]
fn missing_fts_is_empty_but_an_orphan_matching_fts_row_is_corruption() {
    let no_fts = lexical_fixture();
    no_fts.execute("DROP TABLE chunks_fts", []).unwrap();
    assert!(lexical_candidates(&no_fts, "anything").unwrap().is_empty());

    let orphan = lexical_fixture();
    orphan
        .execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (41, 'orphanmatch')",
            [],
        )
        .unwrap();
    let error = lexical_candidates(&orphan, "orphanmatch").unwrap_err();
    assert!(matches!(error, RetrievalError::OrphanFtsRow { rowid: 41 }));
}

#[test]
fn lexical_candidates_reject_noncanonical_paths_out_of_range_ordinals_and_duplicates() {
    let relative = lexical_fixture();
    insert_lexical_chunk(&relative, 1, "relative.md", 0, "relativehit");
    assert!(matches!(
        lexical_candidates(&relative, "relativehit").unwrap_err(),
        RetrievalError::InvalidPagePath { .. }
    ));

    let invalid_ordinal = lexical_fixture();
    insert_lexical_chunk(&invalid_ordinal, 1, "/fiction/ordinal.md", -1, "ordinalhit");
    assert!(matches!(
        lexical_candidates(&invalid_ordinal, "ordinalhit").unwrap_err(),
        RetrievalError::InvalidChunkOrdinal { ordinal: -1, .. }
    ));

    let duplicate = lexical_fixture();
    insert_lexical_chunk(&duplicate, 1, "/fiction/duplicate.md", 0, "duplicatehit");
    duplicate
        .execute(
            "INSERT INTO docs(path) VALUES ('/fiction/duplicate.md')",
            [],
        )
        .unwrap();
    assert!(matches!(
        lexical_candidates(&duplicate, "duplicatehit").unwrap_err(),
        RetrievalError::DuplicateChunk { .. }
    ));

    let non_text = lexical_fixture();
    non_text
        .execute_batch(
            "INSERT INTO docs(path) VALUES (CAST(X'ff' AS BLOB)); \
             INSERT INTO chunks(id, path, ordinal) \
             VALUES (1, CAST(X'ff' AS BLOB), 0); \
             INSERT INTO chunks_fts(rowid, text) VALUES (1, 'blobpathhit');",
        )
        .unwrap();
    assert!(matches!(
        lexical_candidates(&non_text, "blobpathhit").unwrap_err(),
        RetrievalError::InvalidStoredPathType { rowid: 1, .. }
    ));
}

struct DenseFixture {
    _directory: TempDir,
    home: StoreHome,
    project: Connection,
    cache: VectorCache,
    scheme: EmbeddingScheme,
    query: Embedding,
    first_key: VectorKey,
}

fn insert_dense_chunk(
    project: &Connection,
    path: &str,
    ordinal: i64,
    embed_text: &str,
    key: VectorKey,
) {
    project
        .execute(
            "INSERT INTO docs \
             (path, corpus, status, content_hash, byte_length, mtime_ns, device, inode, generation) \
             VALUES (?1, 'shared', NULL, ?2, 1, 1, 1, 1, 1)",
            params![path, "c".repeat(64)],
        )
        .unwrap();
    project
        .execute(
            "INSERT INTO chunks(path, ordinal, text, embed_text, vector_hash) \
             VALUES (?1, ?2, 'fictional body', ?3, ?4)",
            params![path, ordinal, embed_text, key.to_string()],
        )
        .unwrap();
}

fn dense_fixture() -> DenseFixture {
    let directory = tempdir().unwrap();
    let root = directory.path().join("fictional-project");
    std::fs::create_dir(&root).unwrap();
    let home = StoreHome::new(directory.path().join("state"));
    let mut project = open_project(&home, &root).unwrap();
    let scheme = EmbeddingScheme::new("a".repeat(64), 2).unwrap();
    let transaction = project.transaction().unwrap();
    write_embedding_scheme(&transaction, Some(&scheme)).unwrap();
    transaction.commit().unwrap();

    let first_text = "title: red fictional orchard";
    let second_text = "title: blue fictional harbor";
    let first_key = vector_key(MODEL, EmbeddingRole::Passage, first_text).unwrap();
    let second_key = vector_key(MODEL, EmbeddingRole::Passage, second_text).unwrap();
    for (path, ordinal, text, key) in [
        ("/fiction/orchard.md", 0, first_text, first_key),
        ("/fiction/harbor.md", 0, second_text, second_key),
    ] {
        insert_dense_chunk(&project, path, ordinal, text, key);
    }

    let first = Embedding::new(vec![1.0, 0.0]).unwrap();
    let second = Embedding::new(vec![0.0, 1.0]).unwrap();
    let mut cache = VectorCache::open(&home).unwrap();
    cache
        .insert_batch(&[
            VectorInsert::new(first_key, MODEL, EmbeddingRole::Passage, first_text, &first),
            VectorInsert::new(
                second_key,
                MODEL,
                EmbeddingRole::Passage,
                second_text,
                &second,
            ),
        ])
        .unwrap();
    DenseFixture {
        _directory: directory,
        home,
        project,
        cache,
        scheme,
        query: Embedding::new(vec![4.0, 1.0]).unwrap(),
        first_key,
    }
}

#[test]
fn public_snapshot_handle_composes_lexical_dense_and_ranking_reads() {
    let fixture = dense_fixture();
    fixture
        .project
        .execute(
            "INSERT INTO chunks_fts(rowid, text) \
             SELECT id, 'compositiontoken' FROM chunks",
            [],
        )
        .unwrap();

    let snapshot = RetrievalSnapshot::begin(&fixture.project).unwrap();
    let lexical: Vec<LexicalChunk> = snapshot.lexical_candidates("compositiontoken").unwrap();
    let dense: Vec<DenseChunk> = snapshot
        .dense_candidates(&fixture.cache, &fixture.query, &fixture.scheme, MODEL)
        .unwrap();
    let ranked: Vec<yams_core::DenseRankedPage> = snapshot
        .dense_ranking(&fixture.cache, &fixture.query, &fixture.scheme, MODEL)
        .unwrap();

    assert_eq!(snapshot.generation(), 0);
    assert_eq!(lexical.len(), 2);
    assert_eq!(dense.len(), 2);
    assert_eq!(ranked.len(), 2);
}

#[test]
fn snippet_statistics_zero_term_frequencies_when_fts_table_is_missing() {
    let fixture = dense_fixture();
    fixture
        .project
        .execute("DROP TABLE chunks_fts", [])
        .unwrap();

    let statistics = RetrievalSnapshot::begin(&fixture.project)
        .unwrap()
        .snippet_statistics("orchard harbor")
        .unwrap();

    assert_eq!(statistics.total_chunks, 2);
    assert_eq!(statistics.frequencies.len(), 2);
    assert_eq!(statistics.frequencies[0].term, "orchard");
    assert_eq!(statistics.frequencies[0].matching_chunks, 0);
    assert_eq!(statistics.frequencies[1].term, "harbor");
    assert_eq!(statistics.frequencies[1].matching_chunks, 0);
}

#[test]
fn snippet_statistics_reports_a_database_error_when_chunks_table_is_missing() {
    let fixture = dense_fixture();
    fixture.project.execute("DROP TABLE chunks", []).unwrap();

    let error = RetrievalSnapshot::begin(&fixture.project)
        .unwrap()
        .snippet_statistics("orchard harbor")
        .unwrap_err();

    assert!(matches!(
        error,
        RetrievalError::Database {
            operation: "read snippet chunk count",
            ..
        }
    ));
}

#[test]
fn snippet_statistics_reports_non_missing_fts_term_query_errors() {
    let fixture = dense_fixture();
    fixture
        .project
        .execute_batch("DROP TABLE chunks_fts; CREATE TABLE chunks_fts(text TEXT);")
        .unwrap();

    let error = RetrievalSnapshot::begin(&fixture.project)
        .unwrap()
        .snippet_statistics("orchard harbor")
        .unwrap_err();

    match error {
        RetrievalError::Database { operation, source } => {
            assert_eq!(operation, "read snippet term frequency");
            assert!(source.to_string().contains("no such column: chunks_fts"));
        }
        other => panic!("expected term-frequency database error, got {other:?}"),
    }
}

#[test]
fn dense_ranking_loads_every_chunk_and_uses_the_core_cosine_order() {
    let fixture = dense_fixture();

    let candidates = dense_candidates(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &fixture.scheme,
        MODEL,
    )
    .unwrap();
    assert_eq!(candidates.len(), 2);
    assert_eq!(candidates[0].chunk().page().as_str(), "/fiction/harbor.md");
    assert_eq!(candidates[1].chunk().page().as_str(), "/fiction/orchard.md");
    assert_eq!(candidates[1].vector_key(), fixture.first_key);
    assert_eq!(candidates[1].embedding().dimensions(), 2);
    assert_eq!(candidates[0].as_candidate().id(), candidates[0].chunk());

    let ranked = dense_ranking(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &fixture.scheme,
        MODEL,
    )
    .unwrap();

    assert_eq!(ranked.len(), 2);
    assert_eq!(ranked[0].page().as_str(), "/fiction/orchard.md");
    assert_eq!(ranked[0].chunk().ordinal(), 0);
    assert_eq!(ranked[1].page().as_str(), "/fiction/harbor.md");
    assert_eq!(ranked[0].score().get(), 0.9701);
    assert_eq!(ranked[1].score().get(), 0.2425);
}

#[test]
fn one_snapshot_never_mixes_an_old_scheme_with_an_atomic_new_generation() {
    let mut fixture = dense_fixture();
    fixture
        .project
        .execute(
            "INSERT INTO chunks_fts(rowid, text) \
             SELECT id, 'oldgenerationtoken' FROM chunks",
            [],
        )
        .unwrap();
    let journal: String = fixture
        .project
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(journal, "wal");

    let new_scheme = EmbeddingScheme::new("b".repeat(64), 2).unwrap();
    let new_text = "title: atomic new generation";
    let new_key = vector_key(MODEL, EmbeddingRole::Passage, new_text).unwrap();
    let new_vector = Embedding::new(vec![1.0, 1.0]).unwrap();
    fixture
        .cache
        .insert_batch(&[VectorInsert::new(
            new_key,
            MODEL,
            EmbeddingRole::Passage,
            new_text,
            &new_vector,
        )])
        .unwrap();

    let project_path = fixture
        .project
        .query_row(
            "SELECT file FROM pragma_database_list WHERE name = 'main'",
            [],
            |row| row.get::<_, String>(0),
        )
        .unwrap();
    let (snapshot_reached_tx, snapshot_reached_rx) = mpsc::sync_channel(0);
    let (writer_done_tx, writer_done_rx) = mpsc::sync_channel(0);
    let new_scheme_for_writer = new_scheme.clone();
    let writer = thread::spawn(move || {
        let mut connection = Connection::open(project_path).unwrap();
        connection
            .pragma_update(None, "foreign_keys", "ON")
            .unwrap();
        snapshot_reached_rx.recv().unwrap();
        let transaction = connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .unwrap();
        transaction.execute("DELETE FROM chunks_fts", []).unwrap();
        transaction.execute("DELETE FROM chunks", []).unwrap();
        transaction.execute("DELETE FROM docs", []).unwrap();
        transaction
            .execute(
                "INSERT INTO docs \
                 (path, corpus, status, content_hash, byte_length, mtime_ns, device, inode, generation) \
                 VALUES ('/fiction/new-generation.md', 'shared', NULL, ?1, 1, 2, 1, 2, 1)",
                ["d".repeat(64)],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO chunks(path, ordinal, text, embed_text, vector_hash) \
                 VALUES ('/fiction/new-generation.md', 0, 'newgenerationtoken', ?1, ?2)",
                params![new_text, new_key.to_string()],
            )
            .unwrap();
        transaction
            .execute(
                "INSERT INTO chunks_fts(rowid, text) \
                 SELECT id, 'newgenerationtoken' FROM chunks",
                [],
            )
            .unwrap();
        write_embedding_scheme(&transaction, Some(&new_scheme_for_writer)).unwrap();
        transaction
            .execute("UPDATE metadata SET generation = 1 WHERE singleton = 1", [])
            .unwrap();
        transaction.commit().unwrap();
        writer_done_tx.send(()).unwrap();
    });

    let snapshot = RetrievalSnapshot::begin(&fixture.project).unwrap();
    assert_eq!(snapshot.generation(), 0);
    assert_eq!(snapshot.scheme(), Some(&fixture.scheme));
    let old_lexical = snapshot.lexical_candidates("oldgenerationtoken").unwrap();
    snapshot_reached_tx.send(()).unwrap();
    writer_done_rx.recv().unwrap();
    let old_dense = snapshot
        .dense_candidates(&fixture.cache, &fixture.query, &fixture.scheme, MODEL)
        .unwrap();
    assert_eq!(old_dense.len(), 2);
    assert!(
        old_dense
            .iter()
            .all(|candidate| candidate.chunk().page().as_str() != "/fiction/new-generation.md")
    );
    assert_eq!(old_lexical.len(), 2);
    assert!(
        snapshot
            .lexical_candidates("newgenerationtoken")
            .unwrap()
            .is_empty()
    );
    drop(snapshot);
    writer.join().unwrap();

    let snapshot = RetrievalSnapshot::begin(&fixture.project).unwrap();
    assert_eq!(snapshot.generation(), 1);
    assert_eq!(snapshot.scheme(), Some(&new_scheme));
    let new_dense = snapshot
        .dense_candidates(&fixture.cache, &fixture.query, &new_scheme, MODEL)
        .unwrap();
    assert_eq!(new_dense.len(), 1);
    assert_eq!(
        new_dense[0].chunk().page().as_str(),
        "/fiction/new-generation.md"
    );
}

#[test]
fn dense_ranking_keeps_scheme_model_role_and_dimension_identity_separate() {
    let fixture = dense_fixture();
    let different_scheme = EmbeddingScheme::new("b".repeat(64), 2).unwrap();
    let error = dense_ranking(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &different_scheme,
        MODEL,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RetrievalError::EmbeddingSchemeMismatch { .. }
    ));

    let wrong_dimension = Embedding::new(vec![1.0]).unwrap();
    let error = dense_ranking(
        &fixture.project,
        &fixture.cache,
        &wrong_dimension,
        &fixture.scheme,
        MODEL,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RetrievalError::QueryDimensionMismatch {
            expected: 2,
            actual: 1
        }
    ));

    let error = dense_ranking(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &fixture.scheme,
        "different-model-v1",
    )
    .unwrap_err();
    assert!(matches!(error, RetrievalError::VectorKeyMismatch { .. }));

    let query_role_key =
        vector_key(MODEL, EmbeddingRole::Query, "title: red fictional orchard").unwrap();
    fixture
        .project
        .execute(
            "UPDATE chunks SET vector_hash = ?1 WHERE path = '/fiction/orchard.md'",
            [query_role_key.to_string()],
        )
        .unwrap();
    let error = dense_ranking(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &fixture.scheme,
        MODEL,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RetrievalError::VectorKeyMismatch {
            stored,
            expected,
            ..
        } if stored == query_role_key && expected == fixture.first_key
    ));
}

#[test]
fn dense_ranking_refuses_missing_and_mismatched_cached_vectors() {
    let fixture = dense_fixture();
    let raw = Connection::open(fixture.home.vectors_path()).unwrap();
    raw.execute(
        "DELETE FROM vectors WHERE hash = ?1",
        [fixture.first_key.to_string()],
    )
    .unwrap();
    let error = dense_ranking(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &fixture.scheme,
        MODEL,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RetrievalError::MissingVector { key } if key == fixture.first_key
    ));

    raw.execute(
        "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
         VALUES (?1, 'wrong-model-v1', 2, ?2)",
        params![
            fixture.first_key.to_string(),
            Embedding::new(vec![1.0, 0.0]).unwrap().to_le_bytes()
        ],
    )
    .unwrap();
    let error = dense_ranking(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &fixture.scheme,
        MODEL,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RetrievalError::CachedModelSignatureMismatch { key, .. }
            if key == fixture.first_key
    ));

    raw.execute(
        "UPDATE vectors SET model_signature = ?1, dimensions = 1, bytes = ?2 \
         WHERE hash = ?3",
        params![
            MODEL,
            Embedding::new(vec![1.0]).unwrap().to_le_bytes(),
            fixture.first_key.to_string()
        ],
    )
    .unwrap();
    let error = dense_ranking(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &fixture.scheme,
        MODEL,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RetrievalError::CachedDimensionMismatch {
            key,
            expected: 2,
            actual: 1
        } if key == fixture.first_key
    ));
}

#[test]
fn content_addressed_duplicate_keys_are_loaded_once_but_rank_every_chunk() {
    let fixture = dense_fixture();
    insert_dense_chunk(
        &fixture.project,
        "/fiction/also-orchard.md",
        0,
        "title: red fictional orchard",
        fixture.first_key,
    );

    let ranked = dense_ranking(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &fixture.scheme,
        MODEL,
    )
    .unwrap();

    assert_eq!(ranked.len(), 3);
    assert_eq!(ranked[0].page().as_str(), "/fiction/also-orchard.md");
    assert_eq!(ranked[1].page().as_str(), "/fiction/orchard.md");
}

#[test]
fn a_large_repeated_content_baseline_keeps_one_shared_vector_per_unique_key() {
    const REPEATED_CHUNKS: usize = 512;

    let fixture = dense_fixture();
    for index in 0..REPEATED_CHUNKS {
        insert_dense_chunk(
            &fixture.project,
            &format!("/fiction/repeated-{index:03}.md"),
            0,
            "title: red fictional orchard",
            fixture.first_key,
        );
    }

    let candidates = dense_candidates(
        &fixture.project,
        &fixture.cache,
        &fixture.query,
        &fixture.scheme,
        MODEL,
    )
    .unwrap();
    let repeated = candidates
        .iter()
        .filter(|candidate| candidate.vector_key() == fixture.first_key)
        .collect::<Vec<_>>();

    assert_eq!(repeated.len(), REPEATED_CHUNKS + 1);
    assert!(
        repeated
            .iter()
            .all(|candidate| { ptr::eq(candidate.cached_vector(), repeated[0].cached_vector(),) })
    );
    let unique_allocations = candidates
        .iter()
        .map(|candidate| candidate.cached_vector() as *const _ as usize)
        .collect::<BTreeSet<_>>();
    assert_eq!(unique_allocations.len(), 2);
}

#[test]
fn dense_ranking_refuses_unstamped_or_referentially_broken_projects() {
    let mut unstamped = dense_fixture();
    let transaction = unstamped.project.transaction().unwrap();
    write_embedding_scheme(&transaction, None).unwrap();
    transaction.commit().unwrap();
    let error = dense_ranking(
        &unstamped.project,
        &unstamped.cache,
        &unstamped.query,
        &unstamped.scheme,
        MODEL,
    )
    .unwrap_err();
    assert!(matches!(error, RetrievalError::MissingEmbeddingScheme));

    let orphan = dense_fixture();
    orphan
        .project
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    orphan
        .project
        .execute("DELETE FROM docs WHERE path = '/fiction/orchard.md'", [])
        .unwrap();
    let error = dense_ranking(
        &orphan.project,
        &orphan.cache,
        &orphan.query,
        &orphan.scheme,
        MODEL,
    )
    .unwrap_err();
    assert!(matches!(
        error,
        RetrievalError::OrphanChunk { ref path, .. }
            if path == std::path::Path::new("/fiction/orchard.md")
    ));
}

#[test]
fn dense_ranking_refuses_malformed_paths_ordinals_keys_and_vector_bytes() {
    let relative = dense_fixture();
    relative
        .project
        .pragma_update(None, "foreign_keys", "OFF")
        .unwrap();
    relative
        .project
        .execute_batch(
            "UPDATE docs SET path = 'relative.md' \
             WHERE path = '/fiction/orchard.md'; \
             UPDATE chunks SET path = 'relative.md' \
             WHERE path = '/fiction/orchard.md';",
        )
        .unwrap();
    assert!(matches!(
        dense_ranking(
            &relative.project,
            &relative.cache,
            &relative.query,
            &relative.scheme,
            MODEL,
        )
        .unwrap_err(),
        RetrievalError::InvalidPagePath { .. }
    ));

    let invalid_ordinal = dense_fixture();
    invalid_ordinal
        .project
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    invalid_ordinal
        .project
        .execute(
            "UPDATE chunks SET ordinal = -1 WHERE path = '/fiction/orchard.md'",
            [],
        )
        .unwrap();
    assert!(matches!(
        dense_ranking(
            &invalid_ordinal.project,
            &invalid_ordinal.cache,
            &invalid_ordinal.query,
            &invalid_ordinal.scheme,
            MODEL,
        )
        .unwrap_err(),
        RetrievalError::InvalidChunkOrdinal { ordinal: -1, .. }
    ));

    let invalid_key = dense_fixture();
    invalid_key
        .project
        .pragma_update(None, "ignore_check_constraints", "ON")
        .unwrap();
    invalid_key
        .project
        .execute(
            "UPDATE chunks SET vector_hash = 'not-a-vector-key' \
             WHERE path = '/fiction/orchard.md'",
            [],
        )
        .unwrap();
    assert!(matches!(
        dense_ranking(
            &invalid_key.project,
            &invalid_key.cache,
            &invalid_key.query,
            &invalid_key.scheme,
            MODEL,
        )
        .unwrap_err(),
        RetrievalError::InvalidVectorKey { .. }
    ));

    let corrupt_vector = dense_fixture();
    let raw = Connection::open(corrupt_vector.home.vectors_path()).unwrap();
    raw.execute(
        "UPDATE vectors SET bytes = zeroblob(8) WHERE hash = ?1",
        [corrupt_vector.first_key.to_string()],
    )
    .unwrap();
    assert!(matches!(
        dense_ranking(
            &corrupt_vector.project,
            &corrupt_vector.cache,
            &corrupt_vector.query,
            &corrupt_vector.scheme,
            MODEL,
        )
        .unwrap_err(),
        RetrievalError::VectorCache(VectorError::InvalidStoredEmbedding { .. })
    ));
}
