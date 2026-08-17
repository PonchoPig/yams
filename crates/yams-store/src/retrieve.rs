use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use rusqlite::types::Value;
use rusqlite::{OptionalExtension, Transaction};
use thiserror::Error;
use yams_core::{
    ChunkId, ChunkMetadata, DenseCandidate, DenseRankedPage, LexicalCandidate, LexicalScore,
    PageId, PageLabels, PageMetadata, RankError, SnippetStatistics, TermFrequency, dense_rank,
};
use yams_embed::{Embedding, EmbeddingRole};

use crate::{
    CachedVector, EmbeddingScheme, VectorCache, VectorError, VectorKey, VectorKeyParseError,
    read_embedding_scheme, vector_key,
};

/// The Python implementation scores at most eight chunks for each of its 25
/// candidate pages. Keeping the same absolute bound preserves that unfiltered
/// evidence for page collapse and explanations.
pub const LEXICAL_OVERFETCH_CAP: usize = 200;

/// A project read transaction whose first reads pin one embedding scheme and
/// generation for every lexical and dense operation composed through it.
pub struct RetrievalSnapshot<'project> {
    transaction: Transaction<'project>,
    scheme: Option<EmbeddingScheme>,
    generation: i64,
}

impl<'project> RetrievalSnapshot<'project> {
    pub fn begin(project: &'project Connection) -> Result<Self, RetrievalError> {
        let transaction =
            project
                .unchecked_transaction()
                .map_err(|source| RetrievalError::Database {
                    operation: "begin project retrieval snapshot",
                    source,
                })?;
        let scheme =
            read_embedding_scheme(&transaction).map_err(|source| RetrievalError::Database {
                operation: "read project embedding scheme",
                source,
            })?;
        let generation = read_generation(&transaction)?;
        Ok(Self {
            transaction,
            scheme,
            generation,
        })
    }

    pub const fn scheme(&self) -> Option<&EmbeddingScheme> {
        self.scheme.as_ref()
    }

    pub const fn generation(&self) -> i64 {
        self.generation
    }

    /// Retrieves the bounded BM25 chunk stream without collapsing page
    /// identity.
    ///
    /// A missing FTS table is the one survivable pre-index case and produces
    /// an empty stream. Every other SQLite fault and every broken
    /// FTS/chunk/doc reference is reported rather than silently degrading to
    /// dense-only search.
    pub fn lexical_candidates(&self, query: &str) -> Result<Vec<LexicalChunk>, RetrievalError> {
        lexical_candidates_in(&self.transaction, query)
    }

    /// Loads the renderer-independent page metadata for this snapshot.
    pub fn page_metadata(&self) -> Result<Vec<PageMetadata>, RetrievalError> {
        let mut statement = self
            .transaction
            .prepare(
                "SELECT path, corpus, status, \
                 (SELECT embed_text FROM chunks \
                  WHERE chunks.path = docs.path \
                  ORDER BY ordinal, id LIMIT 1) \
                 FROM docs ORDER BY path",
            )
            .map_err(|source| RetrievalError::Database {
                operation: "prepare page metadata",
                source,
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .map_err(|source| RetrievalError::Database {
                operation: "query page metadata",
                source,
            })?;
        let mut pages = Vec::new();
        for row in rows {
            let (path, corpus, status, embed_text) =
                row.map_err(|source| RetrievalError::Database {
                    operation: "read page metadata",
                    source,
                })?;
            let id = PageId::from_canonical_path(&path).map_err(|source| {
                RetrievalError::InvalidPagePath {
                    path: PathBuf::from(path.clone()),
                    source,
                }
            })?;
            let name = display_title(Path::new(&path), embed_text.as_deref());
            pages.push(PageMetadata::new(
                id,
                name,
                PageLabels::new(Some(&corpus), status.as_deref(), None),
            ));
        }
        Ok(pages)
    }

    /// Loads every stored chunk body in stable page/ordinal order.
    pub fn chunk_metadata(&self) -> Result<Vec<ChunkMetadata>, RetrievalError> {
        let mut statement = self
            .transaction
            .prepare("SELECT path, ordinal, text FROM chunks ORDER BY path, ordinal, id")
            .map_err(|source| RetrievalError::Database {
                operation: "prepare chunk metadata",
                source,
            })?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(|source| RetrievalError::Database {
                operation: "query chunk metadata",
                source,
            })?;
        let mut chunks = Vec::new();
        for row in rows {
            let (path, ordinal, text) = row.map_err(|source| RetrievalError::Database {
                operation: "read chunk metadata",
                source,
            })?;
            let page = page_id(path.clone())?;
            let ordinal = checked_ordinal(page.as_path(), ordinal)?;
            chunks.push(ChunkMetadata::new(ChunkId::new(page, ordinal), text));
        }
        Ok(chunks)
    }

    /// Converts the bounded FTS stream into the core's presentation-neutral
    /// lexical score records.
    pub fn lexical_scores(&self, query: &str) -> Result<Vec<LexicalScore>, RetrievalError> {
        Ok(self
            .lexical_candidates(query)?
            .into_iter()
            .map(|candidate| LexicalScore::new(candidate.chunk, candidate.bm25, candidate.rank))
            .collect())
    }

    /// Computes the term frequencies needed by the pure snippet weighting
    /// kernel while retaining the same transaction snapshot as retrieval.
    /// A missing `chunks_fts` table retains the real chunk total and returns
    /// ordered zero frequencies. Every other database fault is propagated.
    pub fn snippet_statistics(&self, query: &str) -> Result<SnippetStatistics, RetrievalError> {
        let total_chunks = self
            .transaction
            .query_row("SELECT count(*) FROM chunks", [], |row| {
                row.get::<_, i64>(0)
            })
            .map_err(|source| RetrievalError::Database {
                operation: "read snippet chunk count",
                source,
            })?;
        let total_chunks =
            u64::try_from(total_chunks).map_err(|_| RetrievalError::InvalidChunkRowId {
                rowid: total_chunks,
            })?;
        let mut frequencies = Vec::new();
        for term in yams_core::query_terms(query) {
            let expression = format!("\"{term}\"");
            let matching_chunks = match self.transaction.query_row(
                "SELECT count(*) FROM chunks_fts WHERE chunks_fts MATCH ?1",
                [&expression],
                |row| row.get::<_, i64>(0),
            ) {
                Ok(matching_chunks) => matching_chunks,
                Err(source) if missing_fts_table(&source) => 0,
                Err(source) => {
                    return Err(RetrievalError::Database {
                        operation: "read snippet term frequency",
                        source,
                    });
                }
            };
            let matching_chunks =
                u64::try_from(matching_chunks).map_err(|_| RetrievalError::InvalidChunkRowId {
                    rowid: matching_chunks,
                })?;
            frequencies.push(TermFrequency {
                term,
                matching_chunks,
            });
        }
        Ok(SnippetStatistics {
            total_chunks,
            frequencies,
        })
    }

    /// Loads and validates the complete dense chunk baseline.
    ///
    /// The project scheme fingerprint and the embedder model signature are
    /// deliberately separate inputs: the former identifies the complete
    /// indexing layout while the latter authenticates each content-addressed
    /// passage key and cached vector row.
    pub fn dense_candidates(
        &self,
        cache: &VectorCache,
        query: &Embedding,
        expected_scheme: &EmbeddingScheme,
        expected_model_signature: &str,
    ) -> Result<Vec<DenseChunk>, RetrievalError> {
        dense_candidates_in(
            &self.transaction,
            cache,
            query,
            expected_scheme,
            expected_model_signature,
            self.scheme.as_ref(),
        )
    }

    /// Loads the validated baseline and ranks it through the pure core f64
    /// cosine kernel, collapsing to the best dense chunk per page.
    pub fn dense_ranking(
        &self,
        cache: &VectorCache,
        query: &Embedding,
        expected_scheme: &EmbeddingScheme,
        expected_model_signature: &str,
    ) -> Result<Vec<DenseRankedPage>, RetrievalError> {
        let chunks =
            self.dense_candidates(cache, query, expected_scheme, expected_model_signature)?;
        let candidates = chunks
            .iter()
            .map(DenseChunk::as_candidate)
            .collect::<Vec<_>>();
        dense_rank(query.values(), &candidates).map_err(RetrievalError::Rank)
    }
}

fn read_generation(project: &Connection) -> Result<i64, RetrievalError> {
    let row = project
        .query_row(
            "SELECT generation, (SELECT count(*) FROM metadata) \
             FROM metadata WHERE singleton = 1",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()
        .map_err(|source| RetrievalError::Database {
            operation: "read project generation",
            source,
        })?;
    let Some((generation, rows)) = row else {
        return Err(RetrievalError::MissingProjectMetadata);
    };
    if rows != 1 || generation < 0 {
        return Err(RetrievalError::InvalidProjectMetadata { generation, rows });
    }
    Ok(generation)
}

fn display_title(path: &Path, embed_text: Option<&str>) -> String {
    embed_text
        .and_then(|text| text.split("\n\n").next())
        .map(str::trim)
        .filter(|title| !title.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|value| value.to_str())
                .unwrap_or("page")
                .to_owned()
        })
}

/// Converts a query into the FTS5 expression built from [`yams_core::query_terms`].
///
/// Each retained term is quoted and joined by `OR`, so user text cannot
/// supply FTS5 syntax.
pub fn fts_query(query: &str) -> Option<String> {
    let terms = yams_core::query_terms(query);
    (!terms.is_empty()).then(|| {
        terms
            .iter()
            .map(|term| format!("\"{term}\""))
            .collect::<Vec<_>>()
            .join(" OR ")
    })
}

/// One directly scored FTS chunk in the unfiltered, deterministic BM25 order.
#[derive(Clone, Debug)]
pub struct LexicalChunk {
    chunk: ChunkId,
    rowid: i64,
    bm25: f64,
    rank: usize,
}

impl LexicalChunk {
    pub const fn chunk(&self) -> &ChunkId {
        &self.chunk
    }

    pub const fn rowid(&self) -> i64 {
        self.rowid
    }

    /// Lower scores are better, as defined by SQLite FTS5.
    pub const fn bm25(&self) -> f64 {
        self.bm25
    }

    /// One-based position in the unfiltered chunk stream.
    pub const fn rank(&self) -> usize {
        self.rank
    }

    pub fn as_candidate(&self) -> LexicalCandidate {
        LexicalCandidate::new(self.chunk.clone())
    }
}

fn lexical_candidates_in(
    project: &Connection,
    query: &str,
) -> Result<Vec<LexicalChunk>, RetrievalError> {
    let Some(expression) = fts_query(query) else {
        return Ok(Vec::new());
    };

    let mut statement = match project.prepare(
        "SELECT matched.rowid, matched.score, chunk.id, chunk.path, chunk.ordinal, doc.rowid \
         FROM ( \
             SELECT rowid, bm25(chunks_fts) AS score \
             FROM chunks_fts \
             WHERE chunks_fts MATCH ?1 \
             ORDER BY score, rowid \
             LIMIT ?2 \
         ) AS matched \
         LEFT JOIN chunks AS chunk ON chunk.id = matched.rowid \
         LEFT JOIN docs AS doc ON doc.path = chunk.path \
         ORDER BY matched.score, matched.rowid",
    ) {
        Ok(statement) => statement,
        Err(source) if missing_fts_table(&source) => return Ok(Vec::new()),
        Err(source) => {
            return Err(RetrievalError::Database {
                operation: "prepare lexical candidate query",
                source,
            });
        }
    };
    let rows = statement
        .query_map(
            rusqlite::params![expression, LEXICAL_OVERFETCH_CAP as i64],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, f64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, Value>(3)?,
                    row.get::<_, Option<i64>>(4)?,
                    row.get::<_, Option<i64>>(5)?,
                ))
            },
        )
        .map_err(|source| RetrievalError::Database {
            operation: "query lexical candidates",
            source,
        })?;

    let mut candidates = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        let (rowid, bm25, chunk_rowid, path, ordinal, doc_rowid) =
            row.map_err(|source| RetrievalError::Database {
                operation: "read lexical candidate",
                source,
            })?;
        if rowid <= 0 {
            return Err(RetrievalError::InvalidChunkRowId { rowid });
        }
        if chunk_rowid.is_none() {
            return Err(RetrievalError::OrphanFtsRow { rowid });
        }
        let path = stored_path(rowid, path)?;
        if doc_rowid.is_none() {
            return Err(RetrievalError::OrphanChunk {
                rowid,
                path: PathBuf::from(path),
            });
        }
        if !bm25.is_finite() {
            return Err(RetrievalError::NonFiniteBm25 { rowid });
        }
        let page = page_id(path)?;
        let Some(ordinal) = ordinal else {
            return Err(RetrievalError::MissingChunkOrdinal {
                path: page.as_path().to_path_buf(),
            });
        };
        let ordinal = checked_ordinal(page.as_path(), ordinal)?;
        let chunk = ChunkId::new(page, ordinal);
        if !seen.insert(chunk.clone()) {
            return Err(RetrievalError::DuplicateChunk { chunk });
        }
        candidates.push(LexicalChunk {
            chunk,
            rowid,
            bm25,
            rank: candidates.len() + 1,
        });
    }
    Ok(candidates)
}

#[derive(Debug)]
struct StoredChunk {
    id: ChunkId,
    key: VectorKey,
}

/// One fully validated, presentation-neutral dense candidate.
#[derive(Clone, Debug)]
pub struct DenseChunk {
    chunk: ChunkId,
    vector: Arc<CachedVector>,
}

impl DenseChunk {
    pub const fn chunk(&self) -> &ChunkId {
        &self.chunk
    }

    pub fn vector_key(&self) -> VectorKey {
        self.vector.key()
    }

    pub fn embedding(&self) -> &Embedding {
        self.vector.embedding()
    }

    /// The shared validated cache record backing this candidate.
    pub fn cached_vector(&self) -> &CachedVector {
        &self.vector
    }

    pub fn as_candidate(&self) -> DenseCandidate<'_> {
        DenseCandidate::new(self.chunk.clone(), self.vector.embedding().values())
    }
}

fn dense_candidates_in(
    project: &Connection,
    cache: &VectorCache,
    query: &Embedding,
    expected_scheme: &EmbeddingScheme,
    expected_model_signature: &str,
    actual_scheme: Option<&EmbeddingScheme>,
) -> Result<Vec<DenseChunk>, RetrievalError> {
    let Some(actual_scheme) = actual_scheme else {
        return Err(RetrievalError::MissingEmbeddingScheme);
    };
    if actual_scheme != expected_scheme {
        return Err(RetrievalError::EmbeddingSchemeMismatch {
            expected_signature: expected_scheme.signature().to_owned(),
            expected_dimensions: expected_scheme.dimensions(),
            actual_signature: actual_scheme.signature().to_owned(),
            actual_dimensions: actual_scheme.dimensions(),
        });
    }
    if query.dimensions() != expected_scheme.dimensions() {
        return Err(RetrievalError::QueryDimensionMismatch {
            expected: expected_scheme.dimensions(),
            actual: query.dimensions(),
        });
    }
    if expected_model_signature.is_empty() {
        return Err(RetrievalError::EmptyExpectedModelSignature);
    }

    let chunks = load_stored_chunks(project, expected_model_signature)?;
    let requested = chunks
        .iter()
        .map(|chunk| chunk.key)
        .collect::<BTreeSet<_>>();
    let cached = cache.get_many(&requested)?;
    for key in &requested {
        let Some(vector) = cached.get(key) else {
            return Err(RetrievalError::MissingVector { key: *key });
        };
        if vector.model_signature() != expected_model_signature {
            return Err(RetrievalError::CachedModelSignatureMismatch {
                key: *key,
                expected: expected_model_signature.to_owned(),
                actual: vector.model_signature().to_owned(),
            });
        }
        if vector.dimensions() != expected_scheme.dimensions() {
            return Err(RetrievalError::CachedDimensionMismatch {
                key: *key,
                expected: expected_scheme.dimensions(),
                actual: vector.dimensions(),
            });
        }
    }
    let shared = cached
        .into_iter()
        .map(|(key, vector)| (key, Arc::new(vector)))
        .collect::<BTreeMap<_, _>>();

    Ok(chunks
        .into_iter()
        .map(|chunk| DenseChunk {
            chunk: chunk.id,
            vector: Arc::clone(&shared[&chunk.key]),
        })
        .collect())
}

fn load_stored_chunks(
    project: &Connection,
    expected_model_signature: &str,
) -> Result<Vec<StoredChunk>, RetrievalError> {
    let mut statement = project
        .prepare(
            "SELECT chunk.id, chunk.path, chunk.ordinal, chunk.embed_text, \
                    chunk.vector_hash, doc.rowid \
             FROM chunks AS chunk \
             LEFT JOIN docs AS doc ON doc.path = chunk.path \
             ORDER BY chunk.path, chunk.ordinal, chunk.id",
        )
        .map_err(|source| RetrievalError::Database {
            operation: "prepare dense chunk inventory",
            source,
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Value>(1)?,
                row.get::<_, Option<i64>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<i64>>(5)?,
            ))
        })
        .map_err(|source| RetrievalError::Database {
            operation: "query dense chunk inventory",
            source,
        })?;

    let mut chunks = Vec::new();
    let mut seen = BTreeSet::new();
    for row in rows {
        let (rowid, path, ordinal, embed_text, encoded_key, doc_rowid) =
            row.map_err(|source| RetrievalError::Database {
                operation: "read dense chunk inventory",
                source,
            })?;
        if rowid <= 0 {
            return Err(RetrievalError::InvalidChunkRowId { rowid });
        }
        let path = stored_path(rowid, path)?;
        if doc_rowid.is_none() {
            return Err(RetrievalError::OrphanChunk {
                rowid,
                path: PathBuf::from(path),
            });
        }
        let page = page_id(path)?;
        let Some(ordinal) = ordinal else {
            return Err(RetrievalError::MissingChunkOrdinal {
                path: page.as_path().to_path_buf(),
            });
        };
        let ordinal = checked_ordinal(page.as_path(), ordinal)?;
        let id = ChunkId::new(page, ordinal);
        if !seen.insert(id.clone()) {
            return Err(RetrievalError::DuplicateChunk { chunk: id });
        }
        let Some(embed_text) = embed_text else {
            return Err(RetrievalError::MissingEmbedText { chunk: id });
        };
        let Some(encoded_key) = encoded_key else {
            return Err(RetrievalError::MissingVectorKey { chunk: id });
        };
        let key = encoded_key.parse::<VectorKey>().map_err(|source| {
            RetrievalError::InvalidVectorKey {
                chunk: id.clone(),
                value: encoded_key,
                source,
            }
        })?;
        let expected_key = vector_key(
            expected_model_signature,
            EmbeddingRole::Passage,
            &embed_text,
        )?;
        if key != expected_key {
            return Err(RetrievalError::VectorKeyMismatch {
                chunk: id,
                stored: key,
                expected: expected_key,
            });
        }
        chunks.push(StoredChunk { id, key });
    }
    Ok(chunks)
}

fn page_id(path: String) -> Result<PageId, RetrievalError> {
    PageId::from_canonical_path(&path).map_err(|source| RetrievalError::InvalidPagePath {
        path: PathBuf::from(path),
        source,
    })
}

fn stored_path(rowid: i64, value: Value) -> Result<String, RetrievalError> {
    match value {
        Value::Text(path) => Ok(path),
        Value::Null => Err(RetrievalError::MissingChunkPath { rowid }),
        Value::Integer(_) => Err(RetrievalError::InvalidStoredPathType {
            rowid,
            found: "integer",
        }),
        Value::Real(_) => Err(RetrievalError::InvalidStoredPathType {
            rowid,
            found: "real",
        }),
        Value::Blob(_) => Err(RetrievalError::InvalidStoredPathType {
            rowid,
            found: "blob",
        }),
    }
}

fn checked_ordinal(path: &std::path::Path, ordinal: i64) -> Result<u32, RetrievalError> {
    u32::try_from(ordinal).map_err(|_| RetrievalError::InvalidChunkOrdinal {
        path: path.to_path_buf(),
        ordinal,
    })
}

fn missing_fts_table(error: &rusqlite::Error) -> bool {
    error.to_string().contains("no such table: chunks_fts")
}

#[derive(Debug, Error)]
pub enum RetrievalError {
    #[error("project retrieval failed while trying to {operation}: {source}")]
    Database {
        operation: &'static str,
        #[source]
        source: rusqlite::Error,
    },

    #[error("project index has no singleton metadata row")]
    MissingProjectMetadata,

    #[error(
        "project metadata must contain one nonnegative generation, got rows={rows} generation={generation}"
    )]
    InvalidProjectMetadata { generation: i64, rows: i64 },

    #[error("FTS row {rowid} has no indexed chunk")]
    OrphanFtsRow { rowid: i64 },

    #[error("chunk row {rowid} for {path:?} has no indexed document")]
    OrphanChunk { rowid: i64, path: PathBuf },

    #[error("chunk row ID must be positive, got {rowid}")]
    InvalidChunkRowId { rowid: i64 },

    #[error("indexed page path is invalid: {path:?}: {source}")]
    InvalidPagePath {
        path: PathBuf,
        #[source]
        source: RankError,
    },

    #[error("chunk ordinal for {path:?} is outside the u32 range: {ordinal}")]
    InvalidChunkOrdinal { path: PathBuf, ordinal: i64 },

    #[error("duplicate indexed chunk identity: {chunk:?}")]
    DuplicateChunk { chunk: ChunkId },

    #[error("FTS row {rowid} has a non-finite BM25 score")]
    NonFiniteBm25 { rowid: i64 },

    #[error("project index has no embedding scheme stamp")]
    MissingEmbeddingScheme,

    #[error(
        "project embedding scheme differs: expected {expected_signature}/{expected_dimensions}, got {actual_signature}/{actual_dimensions}"
    )]
    EmbeddingSchemeMismatch {
        expected_signature: String,
        expected_dimensions: usize,
        actual_signature: String,
        actual_dimensions: usize,
    },

    #[error("query embedding dimension differs: expected {expected}, got {actual}")]
    QueryDimensionMismatch { expected: usize, actual: usize },

    #[error("expected model signature must not be empty")]
    EmptyExpectedModelSignature,

    #[error("chunk row {rowid} has no path")]
    MissingChunkPath { rowid: i64 },

    #[error("chunk row {rowid} path must be SQLite text, found {found}")]
    InvalidStoredPathType { rowid: i64, found: &'static str },

    #[error("chunk for {path:?} has no ordinal")]
    MissingChunkOrdinal { path: PathBuf },

    #[error("chunk {chunk:?} has no embedding text")]
    MissingEmbedText { chunk: ChunkId },

    #[error("chunk {chunk:?} has no vector key")]
    MissingVectorKey { chunk: ChunkId },

    #[error("chunk {chunk:?} has invalid vector key {value:?}: {source}")]
    InvalidVectorKey {
        chunk: ChunkId,
        value: String,
        #[source]
        source: VectorKeyParseError,
    },

    #[error("chunk {chunk:?} vector key differs: stored {stored}, expected {expected}")]
    VectorKeyMismatch {
        chunk: ChunkId,
        stored: VectorKey,
        expected: VectorKey,
    },

    #[error("chunk references missing vector {key}")]
    MissingVector { key: VectorKey },

    #[error("cached vector {key} model differs: expected {expected:?}, got {actual:?}")]
    CachedModelSignatureMismatch {
        key: VectorKey,
        expected: String,
        actual: String,
    },

    #[error("cached vector {key} dimension differs: expected {expected}, got {actual}")]
    CachedDimensionMismatch {
        key: VectorKey,
        expected: usize,
        actual: usize,
    },

    #[error(transparent)]
    VectorCache(#[from] VectorError),

    #[error("dense ranking failed: {0}")]
    Rank(#[source] RankError),
}

impl RetrievalError {
    /// True when dense lookup hit a cache that a writer is still mutating.
    pub fn is_transient_contention(&self) -> bool {
        match self {
            Self::VectorCache(error) => error.is_transient_contention(),
            _ => false,
        }
    }
}
