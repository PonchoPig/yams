use std::path::{Path, PathBuf};

use rusqlite::Connection;
use tempfile::TempDir;
use yams_core::{Corpus, CorpusKind, MAX_FILE_BYTES, scan_corpora};
use yams_embed::{Embedder, Embedding, EmbeddingError, EmbeddingRole, FakeEmbedder};
use yams_store::{
    EmbeddingScheme, StoreHome, SyncError, SyncMode, VectorError, embedding_scheme_for,
    execute_sync_plan, open_project, plan_synchronization, read_embedding_scheme, synchronize,
    vector_key,
};

struct Fixture {
    _directory: TempDir,
    root: PathBuf,
    corpus: Corpus,
    home: StoreHome,
}

impl Fixture {
    fn new(pages: &[(&str, &str)]) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("clockwork-library");
        let corpus_path = root.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        for (name, body) in pages {
            std::fs::write(corpus_path.join(name), page(body)).unwrap();
        }
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let home = StoreHome::new(directory.path().join("state"));
        Self {
            _directory: directory,
            root,
            corpus,
            home,
        }
    }

    fn scan(&self) -> yams_core::ScanReport {
        scan_corpora(std::slice::from_ref(&self.corpus))
    }

    fn page_path(&self, name: &str) -> PathBuf {
        self.corpus.path().join(name)
    }
}

fn page(body: &str) -> String {
    format!("---\ntitle: Clockwork Notes\nstatus: current\n---\n\n{body}\n")
}

#[derive(Default)]
struct RecordingFake {
    inner: FakeEmbedder,
    batches: Vec<Vec<String>>,
}

struct SignatureFake {
    inner: FakeEmbedder,
    signature: &'static str,
}

struct DimensionalFake {
    signature: &'static str,
    dimensions: usize,
}

impl Embedder for DimensionalFake {
    fn signature(&self) -> &str {
        self.signature
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        texts
            .iter()
            .map(|_| {
                let mut values = vec![0.0; self.dimensions];
                values[0] = 1.0;
                Embedding::new(values)
            })
            .collect()
    }

    fn embed_query(&mut self, _text: &str) -> Result<Embedding, EmbeddingError> {
        let mut values = vec![0.0; self.dimensions];
        values[0] = 1.0;
        Embedding::new(values)
    }
}

impl SignatureFake {
    fn new(signature: &'static str) -> Self {
        Self {
            inner: FakeEmbedder::new(),
            signature,
        }
    }
}

impl Embedder for SignatureFake {
    fn signature(&self) -> &str {
        self.signature
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        self.inner.embed_passages(texts)
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        self.inner.embed_query(text)
    }
}

struct MutatingFake {
    inner: FakeEmbedder,
    path: PathBuf,
    replacement: String,
}

impl Embedder for MutatingFake {
    fn signature(&self) -> &str {
        self.inner.signature()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        let result = self.inner.embed_passages(texts)?;
        std::fs::write(&self.path, &self.replacement).unwrap();
        Ok(result)
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        self.inner.embed_query(text)
    }
}

struct GenerationRacingFake {
    inner: FakeEmbedder,
    project_database: PathBuf,
}

struct VectorCollisionFake {
    inner: FakeEmbedder,
    vector_database: PathBuf,
    key: String,
}

impl Embedder for VectorCollisionFake {
    fn signature(&self) -> &str {
        self.inner.signature()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        let embeddings = self.inner.embed_passages(texts)?;
        let collision = Embedding::new(vec![9.0; self.dimensions()]).unwrap();
        let connection = Connection::open(&self.vector_database).unwrap();
        connection
            .execute(
                "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
                 VALUES (?1, 'fake-token-v1', ?2, ?3)",
                (
                    &self.key,
                    i64::try_from(self.dimensions()).unwrap(),
                    collision.to_le_bytes(),
                ),
            )
            .unwrap();
        Ok(embeddings)
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        self.inner.embed_query(text)
    }
}

struct SchemeRacingFake {
    inner: FakeEmbedder,
    project_database: PathBuf,
}

impl Embedder for SchemeRacingFake {
    fn signature(&self) -> &str {
        self.inner.signature()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        let embeddings = self.inner.embed_passages(texts)?;
        let connection = Connection::open(&self.project_database).unwrap();
        connection
            .execute(
                "INSERT INTO embedding_scheme(singleton, signature, dimensions) \
                 VALUES (1, ?1, ?2) ON CONFLICT(singleton) DO UPDATE SET \
                 signature = excluded.signature, dimensions = excluded.dimensions",
                ("a".repeat(64), i64::try_from(self.dimensions()).unwrap()),
            )
            .unwrap();
        Ok(embeddings)
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        self.inner.embed_query(text)
    }
}

enum BrokenOutput {
    MissingLast,
    WrongDimensions,
}

struct BrokenFake {
    inner: FakeEmbedder,
    output: BrokenOutput,
}

impl Embedder for BrokenFake {
    fn signature(&self) -> &str {
        self.inner.signature()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        match self.output {
            BrokenOutput::MissingLast => {
                let mut embeddings = self.inner.embed_passages(texts)?;
                embeddings.pop();
                Ok(embeddings)
            }
            BrokenOutput::WrongDimensions => texts
                .iter()
                .map(|_| Embedding::new(vec![1.0; 31]))
                .collect(),
        }
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        self.inner.embed_query(text)
    }
}

impl Embedder for GenerationRacingFake {
    fn signature(&self) -> &str {
        self.inner.signature()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        let result = self.inner.embed_passages(texts)?;
        let connection = Connection::open(&self.project_database).unwrap();
        connection
            .execute(
                "UPDATE metadata SET generation = generation + 1 WHERE singleton = 1",
                [],
            )
            .unwrap();
        Ok(result)
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        self.inner.embed_query(text)
    }
}

impl Embedder for RecordingFake {
    fn signature(&self) -> &str {
        self.inner.signature()
    }

    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        self.batches.push(texts.to_vec());
        self.inner.embed_passages(texts)
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        self.inner.embed_query(text)
    }
}

fn indexed_hashes(home: &StoreHome, root: &Path) -> Vec<(String, String)> {
    let connection = open_project(home, root).unwrap();
    let mut statement = connection
        .prepare("SELECT path, content_hash FROM docs ORDER BY path")
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn indexed_names(home: &StoreHome, root: &Path) -> Vec<String> {
    indexed_hashes(home, root)
        .into_iter()
        .map(|(path, _)| {
            Path::new(&path)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[derive(Debug, Eq, PartialEq)]
struct ProjectState {
    generation: i64,
    scheme: Option<EmbeddingScheme>,
    docs: Vec<(String, String, i64)>,
    chunks: Vec<(String, i64, String, String, String)>,
    fts: Vec<(i64, String)>,
}

fn project_state(home: &StoreHome, root: &Path) -> ProjectState {
    let connection = open_project(home, root).unwrap();
    let generation = connection
        .query_row(
            "SELECT generation FROM metadata WHERE singleton = 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let scheme = read_embedding_scheme(&connection).unwrap();
    let docs = {
        let mut statement = connection
            .prepare("SELECT path, content_hash, generation FROM docs ORDER BY path")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let chunks = {
        let mut statement = connection
            .prepare(
                "SELECT path, ordinal, text, embed_text, vector_hash \
                 FROM chunks ORDER BY path, ordinal",
            )
            .unwrap();
        statement
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let fts = {
        let mut statement = connection
            .prepare("SELECT rowid, text FROM chunks_fts ORDER BY rowid")
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    ProjectState {
        generation,
        scheme,
        docs,
        chunks,
        fts,
    }
}

fn only_vector_hash(home: &StoreHome, root: &Path) -> String {
    let connection = open_project(home, root).unwrap();
    connection
        .query_row("SELECT vector_hash FROM chunks", [], |row| row.get(0))
        .unwrap()
}

#[derive(Debug, Eq, PartialEq)]
struct IndexedMetadata {
    corpus: String,
    byte_length: i64,
    modified_ns: i64,
    device: i64,
    inode: i64,
    generation: i64,
}

fn indexed_metadata(home: &StoreHome, root: &Path) -> IndexedMetadata {
    let connection = open_project(home, root).unwrap();
    connection
        .query_row(
            "SELECT corpus, byte_length, mtime_ns, device, inode, generation FROM docs",
            [],
            |row| {
                Ok(IndexedMetadata {
                    corpus: row.get(0)?,
                    byte_length: row.get(1)?,
                    modified_ns: row.get(2)?,
                    device: row.get(3)?,
                    inode: row.get(4)?,
                    generation: row.get(5)?,
                })
            },
        )
        .unwrap()
}

#[test]
fn incremental_sync_embeds_only_changed_exact_snapshots() {
    let fixture = Fixture::new(&[
        ("alpha.md", "alpha escapement"),
        ("beta.md", "beta mainspring"),
    ]);
    let mut embedder = RecordingFake::default();

    let first = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    assert_eq!((first.changed, first.removed, first.embedded), (2, 0, 2));
    assert_eq!(embedder.batches.len(), 1);
    assert_eq!(embedder.batches[0].len(), 2);

    let before = indexed_hashes(&fixture.home, &fixture.root);
    let unchanged = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    assert_eq!(
        (unchanged.changed, unchanged.removed, unchanged.embedded),
        (0, 0, 0)
    );
    assert_eq!(embedder.batches.len(), 1, "no-op sync must not embed");
    assert_eq!(indexed_hashes(&fixture.home, &fixture.root), before);

    std::fs::write(fixture.page_path("beta.md"), page("beta tourbillon")).unwrap();
    let changed = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    assert_eq!(
        (changed.changed, changed.removed, changed.embedded),
        (1, 0, 1)
    );
    assert_eq!(embedder.batches.len(), 2);
    assert_eq!(embedder.batches[1], ["Clockwork Notes\n\nbeta tourbillon"]);
}

#[test]
fn unchanged_sync_refuses_a_corrupt_vector_cache_before_publishing() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = RecordingFake::default();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    std::fs::write(fixture.home.vectors_path(), b"not a sqlite database").unwrap();

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        SyncError::Vector(VectorError::Corrupt { .. } | VectorError::Store(_))
    ));
    assert_eq!(embedder.batches.len(), 1);
}

#[test]
fn public_plan_exposes_deterministic_owned_work_before_execution() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let vanished_path = fixture.root.join("vanished-corpus");
    std::fs::create_dir(&vanished_path).unwrap();
    let vanished = Corpus::validated(&vanished_path, CorpusKind::Override).unwrap();
    std::fs::remove_dir(&vanished_path).unwrap();
    let scan = scan_corpora(&[fixture.corpus.clone(), vanished]);
    let embedder = FakeEmbedder::new();

    let plan = plan_synchronization(
        &fixture.home,
        &fixture.root,
        &scan,
        &embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    assert_eq!(plan.generation, 0);
    assert!(plan.deletions.is_empty());
    assert_eq!(plan.unknown, scan.unknown);
    assert_eq!(plan.upserts.len(), 1);
    assert_eq!(plan.upserts[0].path(), fixture.page_path("alpha.md"));
    assert_eq!(plan.upserts[0].corpus(), CorpusKind::Shared);
    assert_eq!(plan.upserts[0].chunks().len(), 1);
    assert!(indexed_names(&fixture.home, &fixture.root).is_empty());

    let mut embedder = FakeEmbedder::new();
    let report = execute_sync_plan(&fixture.home, &fixture.root, plan, &mut embedder).unwrap();
    assert_eq!((report.changed, report.embedded), (1, 1));
}

#[test]
fn forged_scanned_page_identity_and_snapshot_fields_are_refused_before_embedding() {
    for field in [
        "path", "corpus", "hash", "length", "mtime", "device", "inode",
    ] {
        let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
        let before = project_state(&fixture.home, &fixture.root);
        let mut scan = fixture.scan();
        let page = &mut scan.present[0];
        match field {
            "path" => page.path = fixture.page_path("forged.md"),
            "corpus" => page.corpus = CorpusKind::Private,
            "hash" => page.sha256 = "0".repeat(64),
            "length" => page.byte_len += 1,
            "mtime" => page.modified_ns += 1,
            "device" => page.device += 1,
            "inode" => page.inode += 1,
            _ => unreachable!(),
        }
        let mut embedder = RecordingFake::default();

        let error = synchronize(
            &fixture.home,
            &fixture.root,
            &scan,
            &mut embedder,
            SyncMode::Incremental,
        )
        .unwrap_err();

        assert!(
            matches!(error, SyncError::SourceChanged(_)),
            "field={field}"
        );
        assert!(embedder.batches.is_empty(), "field={field}");
        assert_eq!(project_state(&fixture.home, &fixture.root), before);
    }
}

#[test]
fn altered_public_plan_is_refused_before_embedding_or_mutation() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = RecordingFake::default();
    let mut plan = plan_synchronization(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    plan.deletions.push(fixture.page_path("forged.md"));
    let before = project_state(&fixture.home, &fixture.root);

    let error = execute_sync_plan(&fixture.home, &fixture.root, plan, &mut embedder).unwrap_err();

    assert!(matches!(error, SyncError::AlteredPlan));
    assert!(embedder.batches.is_empty());
    assert_eq!(project_state(&fixture.home, &fixture.root), before);
}

#[test]
fn executable_plan_is_bound_to_the_captured_project_root_inode() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("project");
    let corpus_path = directory.path().join("independent-corpus");
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&corpus_path).unwrap();
    std::fs::write(corpus_path.join("alpha.md"), page("alpha escapement")).unwrap();
    let corpus = Corpus::validated(&corpus_path, CorpusKind::Override).unwrap();
    let scan = scan_corpora(&[corpus]);
    let home = StoreHome::new(directory.path().join("state"));
    let mut embedder = FakeEmbedder::new();
    let plan = plan_synchronization(&home, &root, &scan, &embedder, SyncMode::Incremental).unwrap();
    std::fs::rename(&root, directory.path().join("old-project")).unwrap();
    std::fs::create_dir(&root).unwrap();

    let error = execute_sync_plan(&home, &root, plan, &mut embedder).unwrap_err();

    assert!(matches!(error, SyncError::ProjectRootChanged { .. }));
}

#[test]
fn identical_bytes_with_changed_corpus_update_metadata_without_reembedding() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = RecordingFake::default();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let before_vector = only_vector_hash(&fixture.home, &fixture.root);
    let private_corpus = Corpus::validated(fixture.corpus.path(), CorpusKind::Private).unwrap();
    let moved = scan_corpora(&[private_corpus]);

    let report = synchronize(
        &fixture.home,
        &fixture.root,
        &moved,
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    assert_eq!((report.changed, report.embedded), (1, 0));
    assert_eq!(
        indexed_metadata(&fixture.home, &fixture.root).corpus,
        "private"
    );
    assert_eq!(
        only_vector_hash(&fixture.home, &fixture.root),
        before_vector
    );
    assert_eq!(embedder.batches.len(), 1);
}

#[test]
fn same_byte_file_replacement_updates_provenance_without_reembedding() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = RecordingFake::default();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let before = indexed_metadata(&fixture.home, &fixture.root);
    let replacement = fixture.corpus.path().join("replacement.tmp");
    std::fs::write(&replacement, page("alpha escapement")).unwrap();
    std::fs::rename(&replacement, fixture.page_path("alpha.md")).unwrap();
    let replacement_scan = fixture.scan();
    assert_ne!(
        replacement_scan.present[0].inode,
        u64::try_from(before.inode).unwrap()
    );

    let report = synchronize(
        &fixture.home,
        &fixture.root,
        &replacement_scan,
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    let after = indexed_metadata(&fixture.home, &fixture.root);
    assert_eq!((report.changed, report.embedded), (1, 0));
    assert_ne!(after.inode, before.inode);
    assert_eq!(
        after.inode,
        i64::try_from(replacement_scan.present[0].inode).unwrap()
    );
    assert_eq!(embedder.batches.len(), 1);
}

#[test]
fn unknown_observations_retain_rows_but_positive_absence_and_oversize_remove_them() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("clockwork-library");
    let alpha_root = root.join("alpha-corpus");
    let beta_root = root.join("beta-corpus");
    std::fs::create_dir_all(&alpha_root).unwrap();
    std::fs::create_dir(&beta_root).unwrap();
    let alpha_path = alpha_root.join("alpha.md");
    let beta_path = beta_root.join("beta.md");
    std::fs::write(&alpha_path, page("alpha escapement")).unwrap();
    std::fs::write(&beta_path, page("beta mainspring")).unwrap();
    let alpha = Corpus::validated(&alpha_root, CorpusKind::Shared).unwrap();
    let beta = Corpus::validated(&beta_root, CorpusKind::Private).unwrap();
    let home = StoreHome::new(directory.path().join("state"));
    let mut embedder = RecordingFake::default();
    synchronize(
        &home,
        &root,
        &scan_corpora(&[alpha.clone(), beta.clone()]),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    let vanished_beta = root.join("vanished-beta");
    std::fs::rename(&beta_root, &vanished_beta).unwrap();
    let incomplete = scan_corpora(&[alpha.clone(), beta]);
    assert_eq!(incomplete.present.len(), 1);
    assert!(!incomplete.unknown.is_empty());
    let retained = synchronize(
        &home,
        &root,
        &incomplete,
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    assert_eq!(retained.removed, 0);
    assert_eq!(indexed_names(&home, &root), ["alpha.md", "beta.md"]);

    std::fs::remove_dir_all(vanished_beta).unwrap();
    std::fs::create_dir(&beta_root).unwrap();
    let empty_beta = Corpus::validated(&beta_root, CorpusKind::Private).unwrap();
    let empty_error = synchronize(
        &home,
        &root,
        &scan_corpora(&[alpha.clone(), empty_beta.clone()]),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap_err();
    assert!(matches!(
        empty_error,
        SyncError::IncompleteIncremental { .. }
    ));
    assert_eq!(indexed_names(&home, &root), ["alpha.md", "beta.md"]);

    std::fs::write(&beta_path, page("beta mainspring")).unwrap();
    std::fs::write(
        alpha_path,
        vec![b'x'; usize::try_from(MAX_FILE_BYTES).unwrap() + 1],
    )
    .unwrap();
    let oversized_scan = scan_corpora(&[alpha, empty_beta]);
    assert_eq!(oversized_scan.oversized.len(), 1);
    let oversized = synchronize(
        &home,
        &root,
        &oversized_scan,
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    assert_eq!((oversized.changed, oversized.removed), (1, 1));
    assert_eq!(indexed_names(&home, &root), ["beta.md"]);
}

#[test]
fn an_oversized_page_does_not_hide_an_empty_indexed_remainder_of_its_corpus() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("clockwork-library");
    let alpha_root = root.join("alpha-corpus");
    let beta_root = root.join("beta-corpus");
    std::fs::create_dir_all(&alpha_root).unwrap();
    std::fs::create_dir(&beta_root).unwrap();
    let vanished_path = alpha_root.join("vanished.md");
    let oversized_path = alpha_root.join("oversized.md");
    std::fs::write(&vanished_path, page("vanished escapement")).unwrap();
    std::fs::write(&oversized_path, page("oversized mainspring")).unwrap();
    std::fs::write(beta_root.join("beta.md"), page("beta remontoire")).unwrap();
    let alpha = Corpus::validated(&alpha_root, CorpusKind::Shared).unwrap();
    let beta = Corpus::validated(&beta_root, CorpusKind::Private).unwrap();
    let home = StoreHome::new(directory.path().join("state"));
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &home,
        &root,
        &scan_corpora(&[alpha.clone(), beta.clone()]),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let before = project_state(&home, &root);

    std::fs::remove_file(vanished_path).unwrap();
    std::fs::write(
        oversized_path,
        vec![b'x'; usize::try_from(MAX_FILE_BYTES).unwrap() + 1],
    )
    .unwrap();
    let scan = scan_corpora(&[alpha, beta]);
    assert_eq!(scan.present.len(), 1);
    assert_eq!(scan.oversized.len(), 1);

    let error = synchronize(&home, &root, &scan, &mut embedder, SyncMode::Incremental).unwrap_err();

    assert!(matches!(error, SyncError::IncompleteIncremental { .. }));
    assert_eq!(project_state(&home, &root), before);
}

#[test]
fn scheme_change_refuses_to_mix_with_an_unknown_retained_page() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("clockwork-library");
    let alpha_root = root.join("alpha-corpus");
    let beta_root = root.join("beta-corpus");
    std::fs::create_dir_all(&alpha_root).unwrap();
    std::fs::create_dir(&beta_root).unwrap();
    std::fs::write(alpha_root.join("alpha.md"), page("alpha escapement")).unwrap();
    std::fs::write(beta_root.join("beta.md"), page("beta mainspring")).unwrap();
    let alpha = Corpus::validated(&alpha_root, CorpusKind::Shared).unwrap();
    let beta = Corpus::validated(&beta_root, CorpusKind::Private).unwrap();
    let home = StoreHome::new(directory.path().join("state"));
    let mut first_embedder = FakeEmbedder::new();
    synchronize(
        &home,
        &root,
        &scan_corpora(&[alpha.clone(), beta.clone()]),
        &mut first_embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let before = project_state(&home, &root);

    std::fs::rename(&beta_root, root.join("vanished-beta")).unwrap();
    let incomplete = scan_corpora(&[alpha, beta]);
    let mut replacement = SignatureFake::new("fake-token-v2-fixture");

    let error = synchronize(
        &home,
        &root,
        &incomplete,
        &mut replacement,
        SyncMode::Incremental,
    )
    .unwrap_err();

    assert!(matches!(error, SyncError::IncompleteEmbeddingScheme { .. }));
    assert_eq!(project_state(&home, &root), before);
}

#[test]
fn full_rebuild_replaces_the_complete_index_and_can_clear_a_complete_empty_scope() {
    let fixture = Fixture::new(&[
        ("alpha.md", "alpha escapement"),
        ("beta.md", "beta mainspring"),
    ]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    std::fs::write(fixture.page_path("alpha.md"), page("alpha tourbillon")).unwrap();
    std::fs::remove_file(fixture.page_path("beta.md")).unwrap();
    std::fs::write(fixture.page_path("gamma.md"), page("gamma regulator")).unwrap();
    let replacement = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::FullRebuild,
    )
    .unwrap();
    assert_eq!((replacement.changed, replacement.removed), (2, 1));
    assert_eq!(
        indexed_names(&fixture.home, &fixture.root),
        ["alpha.md", "gamma.md"]
    );

    std::fs::remove_file(fixture.page_path("alpha.md")).unwrap();
    std::fs::remove_file(fixture.page_path("gamma.md")).unwrap();
    let empty_scan = fixture.scan();
    assert!(empty_scan.present.is_empty());
    assert_eq!(empty_scan.scanned_corpora, [fixture.corpus.path()]);
    let cleared = synchronize(
        &fixture.home,
        &fixture.root,
        &empty_scan,
        &mut embedder,
        SyncMode::FullRebuild,
    )
    .unwrap();
    assert_eq!((cleared.changed, cleared.removed), (0, 2));
    assert!(indexed_names(&fixture.home, &fixture.root).is_empty());
    assert_eq!(
        project_state(&fixture.home, &fixture.root).scheme,
        Some(embedding_scheme_for(&embedder).unwrap())
    );
}

#[test]
fn incremental_readable_empty_scan_is_typed_refusal_and_preserves_the_index() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let before = project_state(&fixture.home, &fixture.root);
    std::fs::remove_file(fixture.page_path("alpha.md")).unwrap();
    let empty = fixture.scan();
    assert!(empty.present.is_empty());
    assert!(empty.unknown.is_empty());

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &empty,
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap_err();

    let SyncError::IncompleteIncremental { notes } = error else {
        panic!("expected readable-empty incremental refusal")
    };
    assert!(notes.is_empty());
    assert_eq!(project_state(&fixture.home, &fixture.root), before);
}

#[test]
fn first_incremental_scan_with_a_readable_empty_corpus_is_allowed() {
    let fixture = Fixture::new(&[]);
    let mut embedder = FakeEmbedder::new();
    let report = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    assert_eq!((report.changed, report.removed, report.embedded), (0, 0, 0));
    assert!(indexed_names(&fixture.home, &fixture.root).is_empty());
}

#[test]
fn synchronization_plan_debug_never_exposes_retained_or_chunk_text() {
    const SECRET: &str = "PRIVATE-CLOCKWORK-MARKER-9f42";
    let fixture = Fixture::new(&[("alpha.md", SECRET)]);
    let embedder = FakeEmbedder::new();
    let scheme = embedding_scheme_for(&embedder).unwrap();
    let plan = plan_synchronization(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    let plan_debug = format!("{plan:?}");
    let upsert_debug = format!("{:?}", plan.upserts[0]);
    let sentinels = [
        SECRET,
        plan.upserts[0].path().to_str().unwrap(),
        plan.upserts[0].status().unwrap(),
        plan.upserts[0].content_hash(),
        scheme.signature(),
    ];
    for sentinel in sentinels {
        assert!(!plan_debug.contains(sentinel));
        assert!(!upsert_debug.contains(sentinel));
    }
    assert_eq!(plan_debug, "SyncPlan { .. }");
    assert_eq!(upsert_debug, "PageUpsert { .. }");
}

#[test]
fn source_change_after_embedding_aborts_full_rebuild_without_touching_project_state() {
    let fixture = Fixture::new(&[
        ("alpha.md", "alpha escapement"),
        ("beta.md", "beta mainspring"),
    ]);
    let mut initial = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut initial,
        SyncMode::Incremental,
    )
    .unwrap();
    let before = project_state(&fixture.home, &fixture.root);

    std::fs::write(fixture.page_path("alpha.md"), page("alpha remontoire")).unwrap();
    let scan = fixture.scan();
    let mut mutating = MutatingFake {
        inner: FakeEmbedder::new(),
        path: fixture.page_path("alpha.md"),
        replacement: page("alpha changed after embedding"),
    };
    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &scan,
        &mut mutating,
        SyncMode::FullRebuild,
    )
    .unwrap_err();

    assert!(matches!(error, SyncError::SourceChanged(_)));
    assert_eq!(project_state(&fixture.home, &fixture.root), before);
}

#[test]
fn generation_precondition_refuses_a_concurrent_project_writer() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let project_database = fixture.home.project_path(&fixture.root).unwrap();
    let mut embedder = GenerationRacingFake {
        inner: FakeEmbedder::new(),
        project_database,
    };

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        SyncError::ProjectChanged {
            expected: 0,
            actual: 1
        }
    ));
    let state = project_state(&fixture.home, &fixture.root);
    assert_eq!(state.generation, 1, "only the competing writer advanced it");
    assert!(state.docs.is_empty());
    assert!(state.chunks.is_empty());
    assert!(state.fts.is_empty());
    assert_eq!(state.scheme, None);
}

#[test]
fn unchanged_page_self_heals_an_out_of_band_missing_cached_vector() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = RecordingFake::default();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let vector_hash = only_vector_hash(&fixture.home, &fixture.root);
    let vectors = Connection::open(fixture.home.vectors_path()).unwrap();
    vectors
        .execute("DELETE FROM vectors WHERE hash = ?1", [&vector_hash])
        .unwrap();
    drop(vectors);

    let unchanged = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    assert_eq!(
        (unchanged.changed, unchanged.removed, unchanged.embedded),
        (1, 0, 1)
    );
    assert_eq!(embedder.batches.len(), 2);
    assert_eq!(only_vector_hash(&fixture.home, &fixture.root), vector_hash);
}

#[test]
fn no_op_plan_self_heals_a_missing_vector_without_deserializing_all_vectors() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let plan = plan_synchronization(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let key = only_vector_hash(&fixture.home, &fixture.root);
    let vectors = Connection::open(fixture.home.vectors_path()).unwrap();
    vectors
        .execute("DELETE FROM vectors WHERE hash = ?1", [&key])
        .unwrap();
    drop(vectors);

    let report = execute_sync_plan(&fixture.home, &fixture.root, plan, &mut embedder).unwrap();

    assert_eq!((report.changed, report.removed, report.embedded), (1, 0, 1));
}

#[test]
fn malformed_requested_vector_bytes_are_a_typed_failure() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    std::fs::write(fixture.page_path("alpha.md"), page("alpha remontoire")).unwrap();
    let key = vector_key(
        "fake-token-v1",
        EmbeddingRole::Passage,
        "Clockwork Notes\n\nalpha remontoire",
    )
    .unwrap();
    let old_key = only_vector_hash(&fixture.home, &fixture.root);
    let vectors = Connection::open(fixture.home.vectors_path()).unwrap();
    let mut malformed = vec![0_u8; 32 * size_of::<f32>()];
    malformed[..size_of::<f32>()].copy_from_slice(&f32::NAN.to_le_bytes());
    vectors
        .execute(
            "UPDATE vectors SET hash = ?2, bytes = ?3 WHERE hash = ?1",
            (&old_key, key.to_string(), malformed),
        )
        .unwrap();
    drop(vectors);

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        SyncError::Vector(yams_store::VectorError::InvalidStoredEmbedding { .. })
    ));
}

#[test]
fn vector_collision_after_missing_snapshot_is_typed_and_does_not_publish_project_rows() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let embed_text = "Clockwork Notes\n\nalpha escapement";
    let key = vector_key("fake-token-v1", EmbeddingRole::Passage, embed_text).unwrap();
    let before = project_state(&fixture.home, &fixture.root);
    let mut embedder = VectorCollisionFake {
        inner: FakeEmbedder::new(),
        vector_database: fixture.home.vectors_path(),
        key: key.to_string(),
    };

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        SyncError::Vector(yams_store::VectorError::VectorCollision { key: found })
            if found == key
    ));
    assert_eq!(project_state(&fixture.home, &fixture.root), before);
}

#[test]
fn scheme_race_after_embedding_is_typed_and_publishes_no_project_rows() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = SchemeRacingFake {
        inner: FakeEmbedder::new(),
        project_database: fixture.home.project_path(&fixture.root).unwrap(),
    };

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap_err();

    assert!(matches!(error, SyncError::ProjectSchemeChanged { .. }));
    let state = project_state(&fixture.home, &fixture.root);
    assert_eq!(state.generation, 0);
    assert!(state.docs.is_empty());
    assert!(state.chunks.is_empty());
    assert!(state.fts.is_empty());
    assert_eq!(state.scheme.unwrap().signature(), "a".repeat(64));
}

#[test]
fn cached_vector_with_the_wrong_model_signature_is_a_typed_failure() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let embed_text = "Clockwork Notes\n\nalpha escapement";
    let key = vector_key("fake-token-v1", EmbeddingRole::Passage, embed_text).unwrap();
    let mut fake = FakeEmbedder::new();
    let embedding = fake
        .embed_passages(&[embed_text.to_owned()])
        .unwrap()
        .remove(0);
    drop(yams_store::VectorCache::open(&fixture.home).unwrap());
    let vectors = Connection::open(fixture.home.vectors_path()).unwrap();
    vectors
        .execute(
            "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
             VALUES (?1, 'wrong-fixture-model', ?2, ?3)",
            (
                key.to_string(),
                i64::try_from(embedding.dimensions()).unwrap(),
                embedding.to_le_bytes(),
            ),
        )
        .unwrap();
    drop(vectors);
    let before = project_state(&fixture.home, &fixture.root);

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut fake,
        SyncMode::Incremental,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        SyncError::CachedSignature { key: found, .. } if found == key
    ));
    assert_eq!(project_state(&fixture.home, &fixture.root), before);
}

#[test]
fn cached_vector_with_the_wrong_dimensions_is_a_typed_failure() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let embed_text = "Clockwork Notes\n\nalpha escapement";
    let key = vector_key("fake-token-v1", EmbeddingRole::Passage, embed_text).unwrap();
    let wrong = Embedding::new(vec![1.0; 31]).unwrap();
    drop(yams_store::VectorCache::open(&fixture.home).unwrap());
    let vectors = Connection::open(fixture.home.vectors_path()).unwrap();
    vectors
        .execute(
            "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
             VALUES (?1, 'fake-token-v1', 31, ?2)",
            (key.to_string(), wrong.to_le_bytes()),
        )
        .unwrap();
    drop(vectors);
    let before = project_state(&fixture.home, &fixture.root);
    let mut fake = FakeEmbedder::new();

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut fake,
        SyncMode::Incremental,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        SyncError::CachedDimensions {
            key: found,
            expected: 32,
            actual: 31,
        } if found == key
    ));
    assert_eq!(project_state(&fixture.home, &fixture.root), before);
}

#[test]
fn embedding_scheme_and_batch_bound_are_deterministic() {
    let fixture = Fixture::new(&[]);
    for index in 0..65 {
        std::fs::write(
            fixture.page_path(&format!("fixture-{index:02}.md")),
            page(&format!("fictional mechanism number {index}")),
        )
        .unwrap();
    }
    let mut embedder = RecordingFake::default();

    assert_eq!(
        embedding_scheme_for(&embedder).unwrap().signature(),
        "39a45f36245e7618ca2203616ba7f78b7fa61823bf16b565ea4ecd8c69bed303"
    );
    let report = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    assert_eq!((report.changed, report.embedded), (65, 65));
    assert_eq!(
        embedder.batches.iter().map(Vec::len).collect::<Vec<_>>(),
        [32, 32, 1]
    );
}

#[test]
fn same_embedder_signature_with_new_dimensions_is_a_typed_identity_violation() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut first = DimensionalFake {
        signature: "fictional-stable-instance",
        dimensions: 3,
    };
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut first,
        SyncMode::Incremental,
    )
    .unwrap();
    let before = project_state(&fixture.home, &fixture.root);
    let mut replacement = DimensionalFake {
        signature: "fictional-stable-instance",
        dimensions: 5,
    };

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut replacement,
        SyncMode::FullRebuild,
    )
    .unwrap_err();

    assert!(matches!(
        error,
        SyncError::CachedDimensions {
            expected: 5,
            actual: 3,
            ..
        }
    ));
    assert_eq!(project_state(&fixture.home, &fixture.root), before);
}

#[test]
fn embedder_cardinality_and_dimension_mismatches_are_typed_and_atomic() {
    for (output, expected) in [
        (BrokenOutput::MissingLast, "cardinality"),
        (BrokenOutput::WrongDimensions, "dimension"),
    ] {
        let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
        let before = project_state(&fixture.home, &fixture.root);
        let mut embedder = BrokenFake {
            inner: FakeEmbedder::new(),
            output,
        };

        let error = synchronize(
            &fixture.home,
            &fixture.root,
            &fixture.scan(),
            &mut embedder,
            SyncMode::Incremental,
        )
        .unwrap_err();

        match expected {
            "cardinality" => assert!(matches!(
                error,
                SyncError::Embedding(EmbeddingError::CardinalityMismatch { .. })
            )),
            "dimension" => assert!(matches!(
                error,
                SyncError::Embedding(EmbeddingError::DimensionMismatch { .. })
            )),
            _ => unreachable!(),
        }
        assert_eq!(project_state(&fixture.home, &fixture.root), before);
    }
}

#[test]
fn busy_project_transaction_is_typed_and_preserves_the_committed_index() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let before = project_state(&fixture.home, &fixture.root);

    std::fs::write(fixture.page_path("alpha.md"), page("alpha remontoire")).unwrap();
    let holder = Connection::open(fixture.home.project_path(&fixture.root).unwrap()).unwrap();
    holder.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap_err();
    holder.execute_batch("ROLLBACK").unwrap();

    assert!(matches!(error, SyncError::ProjectBusy { .. }));
    assert_eq!(project_state(&fixture.home, &fixture.root), before);
}

#[test]
fn same_size_edit_under_a_restored_mtime_is_detected_by_the_snapshot_hash() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let before_hash = indexed_hashes(&fixture.home, &fixture.root)[0].1.clone();
    let original_modified = std::fs::metadata(fixture.page_path("alpha.md"))
        .unwrap()
        .modified()
        .unwrap();

    let replacement = page("alpha remontoire");
    assert_eq!(replacement.len(), page("alpha escapement").len());
    std::fs::write(fixture.page_path("alpha.md"), replacement).unwrap();
    std::fs::File::options()
        .write(true)
        .open(fixture.page_path("alpha.md"))
        .unwrap()
        .set_times(std::fs::FileTimes::new().set_modified(original_modified))
        .unwrap();

    let report = synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    assert_eq!(report.changed, 1);
    assert_ne!(
        indexed_hashes(&fixture.home, &fixture.root)[0].1,
        before_hash
    );
}

#[test]
fn full_rebuild_refuses_any_incomplete_scan_before_mutation() {
    let fixture = Fixture::new(&[("alpha.md", "alpha escapement")]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &fixture.scan(),
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let before = project_state(&fixture.home, &fixture.root);
    let first_path = fixture.root.join("vanished-one");
    let second_path = fixture.root.join("vanished-two");
    std::fs::create_dir(&first_path).unwrap();
    std::fs::create_dir(&second_path).unwrap();
    let first = Corpus::validated(&first_path, CorpusKind::Private).unwrap();
    let second = Corpus::validated(&second_path, CorpusKind::Override).unwrap();
    std::fs::remove_dir(&first_path).unwrap();
    std::fs::remove_dir(&second_path).unwrap();
    let incomplete = scan_corpora(&[fixture.corpus.clone(), second, first]);

    let error = synchronize(
        &fixture.home,
        &fixture.root,
        &incomplete,
        &mut embedder,
        SyncMode::FullRebuild,
    )
    .unwrap_err();

    let SyncError::IncompleteFullRebuild { notes, .. } = error else {
        panic!("expected an incomplete full rebuild refusal");
    };
    assert_eq!(notes.len(), 2, "refusal notes are deterministic");
    assert!(notes[0].path < notes[1].path);
    assert_eq!(project_state(&fixture.home, &fixture.root), before);
}
