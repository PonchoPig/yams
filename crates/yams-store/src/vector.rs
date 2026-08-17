use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use rusqlite::{
    Connection, ErrorCode, OptionalExtension, TransactionBehavior, params, params_from_iter,
};
use sha2::{Digest, Sha256};
use thiserror::Error;
use yams_embed::{Embedding, EmbeddingError, EmbeddingRole};

use crate::secure::HeldVectorMutationLock;
use crate::{StoreError, StoreHome, open_vectors};

const VECTOR_KEY_BYTES: usize = 32;
const VECTOR_KEY_HEX_BYTES: usize = VECTOR_KEY_BYTES * 2;
const LOOKUP_BATCH_SIZE: usize = 128;
// Preserve the established Python cache's 15-second contention window so the
// Rust transition does not turn ordinary writer overlap into an early failure.
const DEFAULT_BUSY_TIMEOUT: Duration = Duration::from_secs(15);

/// A strong content address for one exact embedding input.
#[derive(Clone, Copy, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct VectorKey([u8; VECTOR_KEY_BYTES]);

impl VectorKey {
    /// Returns the raw SHA-256 digest bytes.
    pub const fn as_bytes(&self) -> &[u8; VECTOR_KEY_BYTES] {
        &self.0
    }
}

impl fmt::Debug for VectorKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_tuple("VectorKey")
            .field(&self.to_string())
            .finish()
    }
}

impl fmt::Display for VectorKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl FromStr for VectorKey {
    type Err = VectorKeyParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        if value.len() != VECTOR_KEY_HEX_BYTES {
            return Err(VectorKeyParseError::InvalidLength {
                actual: value.len(),
            });
        }

        let mut digest = [0_u8; VECTOR_KEY_BYTES];
        for (index, pair) in value.as_bytes().chunks_exact(2).enumerate() {
            let high = hex_digit(pair[0]).ok_or(VectorKeyParseError::InvalidHex { index })?;
            let low = hex_digit(pair[1]).ok_or(VectorKeyParseError::InvalidHex { index })?;
            digest[index] = (high << 4) | low;
        }
        Ok(Self(digest))
    }
}

fn hex_digit(value: u8) -> Option<u8> {
    match value {
        b'0'..=b'9' => Some(value - b'0'),
        b'a'..=b'f' => Some(value - b'a' + 10),
        _ => None,
    }
}

/// Derives a vector key from unambiguous, length-delimited UTF-8 fields.
pub fn vector_key(
    model_signature: &str,
    role: EmbeddingRole,
    text: &str,
) -> Result<VectorKey, VectorError> {
    if model_signature.is_empty() {
        return Err(VectorError::EmptyModelSignature);
    }

    let mut digest = Sha256::new();
    hash_field(&mut digest, model_signature.as_bytes())?;
    hash_field(&mut digest, role.as_str().as_bytes())?;
    hash_field(&mut digest, text.as_bytes())?;
    Ok(VectorKey(digest.finalize().into()))
}

fn hash_field(digest: &mut Sha256, value: &[u8]) -> Result<(), VectorError> {
    let length = u64::try_from(value.len()).map_err(|_| VectorError::KeyFieldTooLong)?;
    digest.update(length.to_be_bytes());
    digest.update(value);
    Ok(())
}

/// One caller-supplied vector and the exact fields that must derive its key.
#[derive(Clone, Copy, Debug)]
pub struct VectorInsert<'a> {
    key: VectorKey,
    model_signature: &'a str,
    role: EmbeddingRole,
    text: &'a str,
    embedding: &'a Embedding,
}

impl<'a> VectorInsert<'a> {
    pub const fn new(
        key: VectorKey,
        model_signature: &'a str,
        role: EmbeddingRole,
        text: &'a str,
        embedding: &'a Embedding,
    ) -> Self {
        Self {
            key,
            model_signature,
            role,
            text,
            embedding,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
struct PreparedVector {
    key: VectorKey,
    model_signature: String,
    dimensions: usize,
    bytes: Vec<u8>,
}

impl PreparedVector {
    fn validate(input: VectorInsert<'_>) -> Result<Self, VectorError> {
        let expected = vector_key(input.model_signature, input.role, input.text)?;
        if input.key != expected {
            return Err(VectorError::KeyMismatch {
                supplied: input.key,
                expected,
            });
        }

        let dimensions = input.embedding.dimensions();
        i64::try_from(dimensions).map_err(|_| VectorError::InvalidDimensions { dimensions })?;
        let bytes = input.embedding.to_le_bytes();
        Embedding::from_le_bytes(&bytes, dimensions)
            .map_err(|source| VectorError::InvalidEmbedding { source })?;
        Ok(Self {
            key: input.key,
            model_signature: input.model_signature.to_owned(),
            dimensions,
            bytes,
        })
    }
}

/// One validated vector read from the persistent cache.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedVector {
    key: VectorKey,
    model_signature: String,
    embedding: Embedding,
}

impl CachedVector {
    pub const fn key(&self) -> VectorKey {
        self.key
    }

    pub fn model_signature(&self) -> &str {
        &self.model_signature
    }

    pub fn dimensions(&self) -> usize {
        self.embedding.dimensions()
    }

    pub const fn embedding(&self) -> &Embedding {
        &self.embedding
    }
}

/// Deterministic vector garbage-collection counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct SweepReport {
    pub kept: usize,
    pub removed: usize,
}

/// Cross-process exclusion for vector-reference publication and garbage collection.
///
/// A synchronizer holds this lease from its final cache reuse decision until
/// the project transaction publishing those references commits. Garbage
/// collectors must acquire the same lease before taking their project
/// reference snapshot and retain it through [`VectorCache::sweep_except`].
#[derive(Debug)]
pub struct VectorMutationLease {
    inner: HeldVectorMutationLock,
}

impl VectorMutationLease {
    /// Acquires the descriptor-pinned store-home mutation lock with a bounded wait.
    pub fn acquire(home: &StoreHome) -> Result<Self, VectorError> {
        Ok(Self {
            inner: HeldVectorMutationLock::acquire(home)?,
        })
    }

    /// Acquires a shared snapshot lease. Concurrent readers may share it;
    /// exclusive publication and GC still wait.
    pub fn acquire_shared(home: &StoreHome) -> Result<Self, VectorError> {
        Ok(Self {
            inner: HeldVectorMutationLock::acquire_shared(home)?,
        })
    }

    #[cfg(test)]
    pub(crate) fn acquire_without_waiting(home: &StoreHome) -> Result<Self, VectorError> {
        Ok(Self {
            inner: HeldVectorMutationLock::acquire_without_waiting(home)?,
        })
    }

    #[cfg(test)]
    pub(crate) fn acquire_shared_without_waiting(home: &StoreHome) -> Result<Self, VectorError> {
        Ok(Self {
            inner: HeldVectorMutationLock::acquire_shared_without_waiting(home)?,
        })
    }

    pub(crate) fn validate_for(&self, home: &StoreHome) -> Result<(), VectorError> {
        let requested = home.vectors_path();
        let parent = requested
            .parent()
            .expect("StoreHome always places the vector cache under rust-v1");
        let resolved_parent =
            fs::canonicalize(parent).map_err(|source| StoreError::InspectPath {
                operation: "resolve vector-cache parent",
                path: parent.to_path_buf(),
                source,
            })?;
        let cache = resolved_parent.join(
            requested
                .file_name()
                .expect("StoreHome always names the vector cache"),
        );
        self.validate_path(&cache)
    }

    /// Rechecks that the private directory and named lock still identify the
    /// descriptor pinned when this lease was acquired.
    pub fn revalidate(&self) -> Result<(), VectorError> {
        self.inner.revalidate()?;
        Ok(())
    }

    fn validate_path(&self, cache: &Path) -> Result<(), VectorError> {
        self.revalidate()?;
        if self.inner.database_path() != cache {
            return Err(VectorError::WrongMutationLease {
                cache: cache.to_path_buf(),
                lease: self.inner.database_path().to_path_buf(),
            });
        }
        Ok(())
    }
}

/// Checked access to the shared content-addressed vector database.
pub struct VectorCache {
    connection: Connection,
    path: PathBuf,
}

impl VectorCache {
    pub fn open(home: &StoreHome) -> Result<Self, VectorError> {
        Self::open_with_busy_timeout(home, DEFAULT_BUSY_TIMEOUT)
    }

    /// Opens the cache for search without a full integrity scan.
    pub fn open_for_search(home: &StoreHome) -> Result<Self, VectorError> {
        let connection = crate::open_vectors_for_search(home)?;
        let path = connection
            .path()
            .map(PathBuf::from)
            .unwrap_or_else(|| home.vectors_path());
        connection
            .busy_timeout(DEFAULT_BUSY_TIMEOUT)
            .map_err(|source| classify_sql("configure vector busy timeout", &path, source))?;
        Ok(Self { connection, path })
    }

    pub(crate) fn open_with_busy_timeout(
        home: &StoreHome,
        busy_timeout: Duration,
    ) -> Result<Self, VectorError> {
        let connection = open_vectors(home)?;
        let path = connection
            .path()
            .map(PathBuf::from)
            .unwrap_or_else(|| home.vectors_path());
        connection
            .busy_timeout(busy_timeout)
            .map_err(|source| classify_sql("configure vector busy timeout", &path, source))?;
        Ok(Self { connection, path })
    }

    /// Returns exactly the requested keys that are absent from the cache.
    pub fn missing(
        &self,
        requested: &BTreeSet<VectorKey>,
    ) -> Result<BTreeSet<VectorKey>, VectorError> {
        let mut missing = requested.clone();
        for encoded in encoded_key_batches(requested) {
            let sql = format!(
                "SELECT hash FROM vectors WHERE hash IN ({}) ORDER BY hash",
                placeholders(encoded.len())
            );
            let mut statement = self.connection.prepare(&sql).map_err(|source| {
                classify_sql("prepare requested vector lookup", &self.path, source)
            })?;
            let rows = statement
                .query_map(params_from_iter(&encoded), |row| row.get::<_, String>(0))
                .map_err(|source| {
                    classify_sql("query requested vector keys", &self.path, source)
                })?;
            for row in rows {
                let encoded = row.map_err(|source| {
                    classify_sql("read requested vector key", &self.path, source)
                })?;
                let key = encoded
                    .parse()
                    .map_err(|source| VectorError::InvalidStoredKey {
                        value: encoded,
                        source,
                    })?;
                if !missing.remove(&key) {
                    return Err(VectorError::UnexpectedStoredKey { stored: key });
                }
            }
        }
        Ok(missing)
    }

    /// Inserts a batch atomically after validating every supplied record.
    pub fn insert_batch(&mut self, input: &[VectorInsert<'_>]) -> Result<(), VectorError> {
        let mut prepared = BTreeMap::<VectorKey, PreparedVector>::new();
        for item in input {
            let item = PreparedVector::validate(*item)?;
            if let Some(previous) = prepared.get(&item.key) {
                if previous != &item {
                    return Err(VectorError::VectorCollision { key: item.key });
                }
            } else {
                prepared.insert(item.key, item);
            }
        }
        if prepared.is_empty() {
            return Ok(());
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| {
                classify_sql("begin vector-cache insert transaction", &self.path, source)
            })?;
        for item in prepared.values() {
            let existing = raw_vector(&transaction, item.key, &self.path)?;
            if let Some(existing) = existing {
                if existing != *item {
                    return Err(VectorError::VectorCollision { key: item.key });
                }
                continue;
            }
            transaction
                .execute(
                    "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
                     VALUES (?1, ?2, ?3, ?4)",
                    params![
                        item.key.to_string(),
                        item.model_signature,
                        item.dimensions as i64,
                        item.bytes,
                    ],
                )
                .map_err(|source| classify_sql("insert cached vector", &self.path, source))?;
        }
        transaction
            .commit()
            .map_err(|source| classify_sql("commit cached vector batch", &self.path, source))
    }

    /// Loads requested records in ascending key order, omitting absent keys.
    pub fn get_many(
        &self,
        requested: &BTreeSet<VectorKey>,
    ) -> Result<BTreeMap<VectorKey, CachedVector>, VectorError> {
        let mut found = BTreeMap::new();
        for encoded in encoded_key_batches(requested) {
            let sql = format!(
                "SELECT hash, model_signature, dimensions, bytes FROM vectors \
                 WHERE hash IN ({}) ORDER BY hash",
                placeholders(encoded.len())
            );
            let mut statement = self.connection.prepare(&sql).map_err(|source| {
                classify_sql("prepare requested vector load", &self.path, source)
            })?;
            let rows = statement
                .query_map(params_from_iter(&encoded), |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, Vec<u8>>(3)?,
                    ))
                })
                .map_err(|source| classify_sql("query requested vectors", &self.path, source))?;
            for row in rows {
                let (encoded_key, model_signature, dimensions, bytes) = row
                    .map_err(|source| classify_sql("read requested vector", &self.path, source))?;
                let key = encoded_key
                    .parse()
                    .map_err(|source| VectorError::InvalidStoredKey {
                        value: encoded_key,
                        source,
                    })?;
                if !requested.contains(&key) {
                    return Err(VectorError::UnexpectedStoredKey { stored: key });
                }
                if model_signature.is_empty() {
                    return Err(VectorError::InvalidStoredModelSignature { key });
                }
                let dimensions = checked_stored_dimensions(key, dimensions)?;
                let embedding = Embedding::from_le_bytes(&bytes, dimensions)
                    .map_err(|source| VectorError::InvalidStoredEmbedding { key, source })?;
                let cached = CachedVector {
                    key,
                    model_signature,
                    embedding,
                };
                if found.insert(key, cached).is_some() {
                    return Err(VectorError::DuplicateStoredVector { key });
                }
            }
        }
        Ok(found)
    }

    /// Atomically removes every vector not present in the exact reference set.
    ///
    /// The caller must acquire `lease` before taking the project-reference
    /// snapshot used to build `referenced`, then retain it through this call.
    /// Requiring the lease here prevents a stale pre-publication snapshot from
    /// waiting for a synchronizer and deleting the vectors it just published.
    pub fn sweep_except(
        &mut self,
        lease: &VectorMutationLease,
        referenced: &BTreeSet<VectorKey>,
    ) -> Result<SweepReport, VectorError> {
        let initial = self.keys()?;
        self.sweep_snapshot(lease, &initial, referenced)
    }

    /// Atomically removes only keys present in `initial` and absent from the
    /// current live-reference set. Keys published after the initial snapshot
    /// are retained conservatively.
    pub fn sweep_snapshot(
        &mut self,
        lease: &VectorMutationLease,
        initial: &BTreeSet<VectorKey>,
        referenced: &BTreeSet<VectorKey>,
    ) -> Result<SweepReport, VectorError> {
        self.validate_lease(lease)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(|source| {
                classify_sql("begin vector-cache sweep transaction", &self.path, source)
            })?;
        let stored = {
            let mut statement = transaction
                .prepare("SELECT hash FROM vectors ORDER BY hash")
                .map_err(|source| classify_sql("prepare vector sweep", &self.path, source))?;
            let rows = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|source| classify_sql("query vector sweep", &self.path, source))?;
            let mut stored = Vec::new();
            for row in rows {
                let encoded =
                    row.map_err(|source| classify_sql("read vector sweep", &self.path, source))?;
                let key = encoded
                    .parse()
                    .map_err(|source| VectorError::InvalidStoredKey {
                        value: encoded,
                        source,
                    })?;
                stored.push(key);
            }
            stored
        };

        let mut report = SweepReport::default();
        for key in stored {
            if !initial.contains(&key) || referenced.contains(&key) {
                report.kept += 1;
            } else {
                let removed = transaction
                    .execute("DELETE FROM vectors WHERE hash = ?1", [key.to_string()])
                    .map_err(|source| {
                        classify_sql("delete unreferenced vector", &self.path, source)
                    })?;
                report.removed += removed;
            }
        }
        lease.revalidate()?;
        transaction
            .commit()
            .map_err(|source| classify_sql("commit vector-cache sweep", &self.path, source))?;
        Ok(report)
    }

    /// Returns all persisted vector keys without reading vector blobs.
    pub fn keys(&self) -> Result<BTreeSet<VectorKey>, VectorError> {
        let mut statement = self
            .connection
            .prepare("SELECT hash FROM vectors ORDER BY hash")
            .map_err(|source| classify_sql("prepare vector-key inventory", &self.path, source))?;
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .map_err(|source| classify_sql("query vector-key inventory", &self.path, source))?;
        let mut keys = BTreeSet::new();
        for row in rows {
            let encoded = row
                .map_err(|source| classify_sql("read vector-key inventory", &self.path, source))?;
            let key = encoded
                .parse()
                .map_err(|source| VectorError::InvalidStoredKey {
                    value: encoded,
                    source,
                })?;
            if !keys.insert(key) {
                return Err(VectorError::DuplicateStoredVector { key });
            }
        }
        Ok(keys)
    }

    fn validate_lease(&self, lease: &VectorMutationLease) -> Result<(), VectorError> {
        lease.validate_path(&self.path)
    }
}

fn encoded_key_batches(requested: &BTreeSet<VectorKey>) -> Vec<Vec<String>> {
    let mut batches = Vec::new();
    let mut batch = Vec::with_capacity(LOOKUP_BATCH_SIZE);
    for key in requested {
        batch.push(key.to_string());
        if batch.len() == LOOKUP_BATCH_SIZE {
            batches.push(batch);
            batch = Vec::with_capacity(LOOKUP_BATCH_SIZE);
        }
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    batches
}

fn placeholders(count: usize) -> String {
    std::iter::repeat_n("?", count)
        .collect::<Vec<_>>()
        .join(", ")
}

fn raw_vector(
    connection: &Connection,
    key: VectorKey,
    path: &Path,
) -> Result<Option<PreparedVector>, VectorError> {
    let row = connection
        .query_row(
            "SELECT hash, model_signature, dimensions, bytes FROM vectors WHERE hash = ?1",
            [key.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            },
        )
        .optional()
        .map_err(|source| classify_sql("load cached vector", path, source))?;
    let Some((encoded_key, model_signature, dimensions, bytes)) = row else {
        return Ok(None);
    };
    let stored_key = encoded_key
        .parse()
        .map_err(|source| VectorError::InvalidStoredKey {
            value: encoded_key,
            source,
        })?;
    if stored_key != key {
        return Err(VectorError::StoredKeyMismatch {
            requested: key,
            stored: stored_key,
        });
    }
    if model_signature.is_empty() {
        return Err(VectorError::InvalidStoredModelSignature { key });
    }
    let dimensions = checked_stored_dimensions(key, dimensions)?;
    Ok(Some(PreparedVector {
        key,
        model_signature,
        dimensions,
        bytes,
    }))
}

fn checked_stored_dimensions(key: VectorKey, dimensions: i64) -> Result<usize, VectorError> {
    let value = usize::try_from(dimensions)
        .map_err(|_| VectorError::InvalidStoredDimensions { key, dimensions })?;
    if value == 0 {
        return Err(VectorError::InvalidStoredDimensions { key, dimensions });
    }
    Ok(value)
}

fn classify_sql(operation: &'static str, path: &Path, source: rusqlite::Error) -> VectorError {
    let code = match &source {
        rusqlite::Error::SqliteFailure(failure, _) => Some(failure.code),
        _ => None,
    };
    match code {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => VectorError::Busy {
            operation,
            path: path.to_path_buf(),
            source,
        },
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => VectorError::Corrupt {
            operation,
            path: path.to_path_buf(),
            source,
        },
        Some(ErrorCode::ConstraintViolation) => VectorError::Constraint {
            operation,
            path: path.to_path_buf(),
            source,
        },
        _ => VectorError::Database {
            operation,
            path: path.to_path_buf(),
            source,
        },
    }
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum VectorKeyParseError {
    #[error("vector key must contain 64 lowercase hexadecimal characters, got {actual}")]
    InvalidLength { actual: usize },

    #[error("vector key contains invalid lowercase hexadecimal at byte pair {index}")]
    InvalidHex { index: usize },
}

#[derive(Debug, Error)]
pub enum VectorError {
    #[error("model signature must not be empty")]
    EmptyModelSignature,

    #[error("vector key field is too long to encode")]
    KeyFieldTooLong,

    #[error("supplied vector key {supplied} does not match expected key {expected}")]
    KeyMismatch {
        supplied: VectorKey,
        expected: VectorKey,
    },

    #[error("embedding dimension {dimensions} cannot be stored")]
    InvalidDimensions { dimensions: usize },

    #[error("embedding failed checked byte validation: {source}")]
    InvalidEmbedding {
        #[source]
        source: EmbeddingError,
    },

    #[error("vector key {key} collides with different cached metadata or bytes")]
    VectorCollision { key: VectorKey },

    #[error("vector mutation lease for {lease} cannot protect cache {cache}")]
    WrongMutationLease { cache: PathBuf, lease: PathBuf },

    #[error("persisted vector key {value:?} is invalid: {source}")]
    InvalidStoredKey {
        value: String,
        #[source]
        source: VectorKeyParseError,
    },

    #[error("requested vector key {requested} resolved to persisted key {stored}")]
    StoredKeyMismatch {
        requested: VectorKey,
        stored: VectorKey,
    },

    #[error("vector lookup returned unrequested key {stored}")]
    UnexpectedStoredKey { stored: VectorKey },

    #[error("vector lookup returned duplicate rows for key {key}")]
    DuplicateStoredVector { key: VectorKey },

    #[error("persisted vector {key} has an empty model signature")]
    InvalidStoredModelSignature { key: VectorKey },

    #[error("persisted vector {key} has invalid dimensions {dimensions}")]
    InvalidStoredDimensions { key: VectorKey, dimensions: i64 },

    #[error("persisted vector {key} failed embedding validation: {source}")]
    InvalidStoredEmbedding {
        key: VectorKey,
        #[source]
        source: EmbeddingError,
    },

    #[error(transparent)]
    Store(#[from] StoreError),

    #[error("vector store is busy while trying to {operation} at {path}: {source}")]
    Busy {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("vector store is corrupt while trying to {operation} at {path}: {source}")]
    Corrupt {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("vector store constraint failed while trying to {operation} at {path}: {source}")]
    Constraint {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("vector store failed while trying to {operation} at {path}: {source}")]
    Database {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
}

impl VectorError {
    /// True when the cache is busy or the underlying store is mid-write.
    pub fn is_transient_contention(&self) -> bool {
        match self {
            Self::Store(error) => error.is_transient_contention(),
            Self::Busy { .. } => true,
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use rusqlite::Connection;
    use rusqlite::trace::{TraceEvent, TraceEventCodes};
    use tempfile::tempdir;
    use yams_embed::{Embedding, EmbeddingRole};

    use super::{VectorCache, VectorError, VectorInsert, vector_key};
    use crate::{StoreError, StoreHome};

    static GET_MANY_STATEMENTS: AtomicUsize = AtomicUsize::new(0);
    static MISSING_STATEMENTS: AtomicUsize = AtomicUsize::new(0);
    static MISSING_BLOB_READS: AtomicUsize = AtomicUsize::new(0);

    fn count_get_many_statement(event: TraceEvent<'_>) {
        if let TraceEvent::Stmt(statement, _) = event
            && statement
                .sql()
                .starts_with("SELECT hash, model_signature, dimensions, bytes FROM vectors")
        {
            GET_MANY_STATEMENTS.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn count_missing_statement(event: TraceEvent<'_>) {
        if let TraceEvent::Stmt(statement, _) = event {
            let sql = statement.sql();
            if sql.starts_with("SELECT hash FROM vectors WHERE hash IN") {
                MISSING_STATEMENTS.fetch_add(1, Ordering::Relaxed);
            }
            if sql.starts_with("SELECT") && sql.contains("FROM vectors") && sql.contains("bytes") {
                MISSING_BLOB_READS.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn requested_keys(count: usize) -> BTreeSet<super::VectorKey> {
        (0..count)
            .map(|index| {
                vector_key(
                    "fixture-model-v1",
                    EmbeddingRole::Passage,
                    &format!("text-{index}"),
                )
                .unwrap()
            })
            .collect()
    }

    #[test]
    fn get_many_uses_one_statement_per_bounded_key_batch() {
        let directory = tempdir().unwrap();
        let home = StoreHome::new(directory.path());
        let cache = VectorCache::open(&home).unwrap();
        let requested = requested_keys(257);
        GET_MANY_STATEMENTS.store(0, Ordering::Relaxed);
        cache.connection.trace_v2(
            TraceEventCodes::SQLITE_TRACE_STMT,
            Some(count_get_many_statement),
        );

        assert!(cache.get_many(&requested).unwrap().is_empty());
        assert_eq!(GET_MANY_STATEMENTS.load(Ordering::Relaxed), 3);
    }

    #[test]
    fn missing_uses_one_statement_per_bounded_key_batch() {
        let directory = tempdir().unwrap();
        let home = StoreHome::new(directory.path());
        let cache = VectorCache::open(&home).unwrap();
        let requested = requested_keys(257);
        MISSING_STATEMENTS.store(0, Ordering::Relaxed);
        MISSING_BLOB_READS.store(0, Ordering::Relaxed);
        cache.connection.trace_v2(
            TraceEventCodes::SQLITE_TRACE_STMT,
            Some(count_missing_statement),
        );

        assert_eq!(cache.missing(&requested).unwrap(), requested);
        assert_eq!(MISSING_STATEMENTS.load(Ordering::Relaxed), 3);
        assert_eq!(MISSING_BLOB_READS.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn default_busy_timeout_matches_the_fifteen_second_compatibility_contract() {
        let directory = tempdir().unwrap();
        let home = StoreHome::new(directory.path());
        let cache = VectorCache::open(&home).unwrap();

        let timeout_ms = cache
            .connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get::<_, i64>(0))
            .unwrap();

        assert_eq!(timeout_ms, 15_000);
    }

    #[test]
    fn contention_test_uses_an_explicit_short_timeout_seam() {
        let directory = tempdir().unwrap();
        let home = StoreHome::new(directory.path());
        let value = Embedding::new(vec![1.0]).unwrap();
        let key = vector_key("fixture-model-v1", EmbeddingRole::Passage, "alpha").unwrap();
        let mut cache =
            VectorCache::open_with_busy_timeout(&home, Duration::from_millis(1)).unwrap();
        let blocker = Connection::open(home.vectors_path()).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();

        let error = cache
            .insert_batch(&[VectorInsert::new(
                key,
                "fixture-model-v1",
                EmbeddingRole::Passage,
                "alpha",
                &value,
            )])
            .unwrap_err();

        assert!(matches!(error, VectorError::Busy { .. }));
        blocker.execute_batch("ROLLBACK").unwrap();
    }

    #[test]
    fn busy_and_store_integrity_are_transient_contention() {
        assert!(
            VectorError::Busy {
                operation: "insert",
                path: PathBuf::from("/tmp/vectors.sqlite3"),
                source: rusqlite::Error::InvalidQuery,
            }
            .is_transient_contention()
        );
        assert!(
            VectorError::Store(StoreError::Integrity {
                path: PathBuf::from("/tmp/vectors.sqlite3"),
                detail: "wrong # of entries in index".into(),
            })
            .is_transient_contention()
        );
        assert!(!VectorError::EmptyModelSignature.is_transient_contention());
    }
}
