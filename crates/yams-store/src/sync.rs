use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use rusqlite::{Connection, ErrorCode, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};
use thiserror::Error;
use yams_core::{
    Chunk, ChunkError, CorpusKind, MAX_CHUNK, MIN_CHUNK, ScanNote, ScanReport, ScannedPage,
    chunks_for_page, parse_frontmatter,
};
use yams_embed::{Embedder, Embedding, EmbeddingError, EmbeddingRole};

use crate::project::{ProjectRootBinding, bind_project_root};
use crate::{
    CachedVector, EmbeddingScheme, EmbeddingSchemeError, StoreError, StoreHome, VectorCache,
    VectorError, VectorInsert, VectorKey, VectorKeyParseError, VectorMutationLease, open_project,
    read_embedding_scheme, vector_key, write_embedding_scheme,
};

const EMBED_BATCH_SIZE: usize = 32;
const SCHEME_FORMAT: &str = "yams-embedding-scheme-v1";
const CHUNK_LAYOUT: &str = "frontmatter-body;paragraph-line-hard-wrap;unicode-scalar-count";
const TITLE_LAYOUT: &str = "frontmatter-title-or-name-or-filename;max-200-unicode-scalars";
const EMBED_TEXT_LAYOUT: &str = "title+LF+LF+display-chunk";

/// Selects incremental reconciliation or an atomic wholesale replacement.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum SyncMode {
    #[default]
    Incremental,
    FullRebuild,
}

/// Deterministic counts and scanner notes from one committed synchronization.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SyncReport {
    pub changed: usize,
    pub removed: usize,
    pub embedded: usize,
    pub generation: i64,
    pub notes: Vec<ScanNote>,
}

/// One owned, revision-bound page update in a [`SyncPlan`].
///
/// The task contract intentionally leaves this type's exact representation to
/// the implementation. Its fields remain private so callers can inspect the
/// work but cannot forge source revisions or replace captured bytes behind an
/// executable update.
#[derive(Clone)]
pub struct PageUpsert {
    source: ScannedPage,
    path: String,
    corpus: &'static str,
    status: Option<String>,
    byte_length: i64,
    modified_ns: i64,
    device: i64,
    inode: i64,
    chunks: Vec<Chunk>,
    kind: UpsertKind,
}

impl std::fmt::Debug for PageUpsert {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("PageUpsert").finish_non_exhaustive()
    }
}

impl PageUpsert {
    /// Returns the canonical page path captured by the scanner.
    pub fn path(&self) -> &Path {
        &self.source.path
    }

    /// Returns the corpus whose precedence produced this page.
    pub const fn corpus(&self) -> CorpusKind {
        self.source.corpus
    }

    /// Returns the normalized persisted status, when recognized.
    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    /// Returns the SHA-256 of the exact retained source bytes.
    pub fn content_hash(&self) -> &str {
        &self.source.sha256
    }

    /// Returns the captured byte length.
    pub const fn byte_length(&self) -> u64 {
        self.source.byte_len
    }

    /// Returns the captured modification timestamp in nanoseconds.
    pub const fn modified_ns(&self) -> i128 {
        self.source.modified_ns
    }

    /// Returns the captured filesystem device identity.
    pub const fn device(&self) -> u64 {
        self.source.device
    }

    /// Returns the captured filesystem inode identity.
    pub const fn inode(&self) -> u64 {
        self.source.inode
    }

    /// Returns the deterministic model-neutral chunks built from retained bytes.
    pub fn chunks(&self) -> &[Chunk] {
        &self.chunks
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpsertKind {
    Replace,
    MetadataOnly,
}

/// Deterministic, owned synchronization work captured before embedding.
///
/// The private state binds the public work list to the project, embedding
/// scheme and exact scan snapshots. Execute a plan with
/// [`execute_sync_plan`]; dropping it safely abandons the work.
pub struct SyncPlan {
    pub generation: i64,
    pub upserts: Vec<PageUpsert>,
    pub deletions: Vec<PathBuf>,
    pub unknown: Vec<ScanNote>,
    previous_scheme: Option<EmbeddingScheme>,
    target_scheme: EmbeddingScheme,
    mode: SyncMode,
    notes: Vec<ScanNote>,
    scan: ScanReport,
    unchanged_vector_references: BTreeMap<PathBuf, BTreeSet<VectorKey>>,
    project_path: PathBuf,
    project_binding: ProjectRootBinding,
    seal: [u8; 32],
}

impl std::fmt::Debug for SyncPlan {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SyncPlan").finish_non_exhaustive()
    }
}

/// Failures that leave the project index transaction uncommitted.
#[derive(Debug, Error)]
pub enum SyncError {
    #[error(transparent)]
    Store(#[from] StoreError),

    #[error(transparent)]
    Vector(#[from] VectorError),

    #[error("could not embed passages: {0}")]
    Embedding(#[from] EmbeddingError),

    #[error(transparent)]
    Chunk(#[from] ChunkError),

    #[error("scanned page is not valid UTF-8: {path}")]
    InvalidUtf8 { path: PathBuf },

    #[error("scanned page metadata field {field} is outside SQLite's integer range: {path}")]
    MetadataOutOfRange { path: PathBuf, field: &'static str },

    #[error(
        "project index generation changed during synchronization: expected {expected}, found {actual}"
    )]
    ProjectChanged { expected: i64, actual: i64 },

    #[error(
        "project embedding scheme changed during synchronization: expected {expected:?}, found {actual:?}"
    )]
    ProjectSchemeChanged {
        expected: Option<EmbeddingScheme>,
        actual: Option<EmbeddingScheme>,
    },

    #[error("project index generation cannot advance past {generation}")]
    GenerationOverflow { generation: i64 },

    #[error("scanned source changed before synchronization could commit: {0:?}")]
    SourceChanged(ScanNote),

    #[error("embedding scheme cannot change while {retained:?} remain outside a positive scan")]
    IncompleteEmbeddingScheme {
        retained: Vec<PathBuf>,
        notes: Vec<ScanNote>,
    },

    #[error("incremental synchronization refuses a completely observed readable-empty scan")]
    IncompleteIncremental { notes: Vec<ScanNote> },

    #[error("full rebuild cannot replace indexed paths outside the complete scan: {uninspected:?}")]
    IncompleteFullRebuild {
        uninspected: Vec<PathBuf>,
        notes: Vec<ScanNote>,
    },

    #[error("project chunk for {path} contains an invalid vector key {value:?}: {source}")]
    InvalidProjectVectorKey {
        path: PathBuf,
        value: String,
        #[source]
        source: VectorKeyParseError,
    },

    #[error("cached vector {key} belongs to {actual:?}, expected {expected:?}")]
    CachedSignature {
        key: VectorKey,
        expected: String,
        actual: String,
    },

    #[error("cached vector {key} has {actual} dimensions, expected {expected}")]
    CachedDimensions {
        key: VectorKey,
        expected: usize,
        actual: usize,
    },

    #[error("vector cache does not contain required vector {key}")]
    MissingCachedVector { key: VectorKey },

    #[error("synchronization plan was altered after it was captured")]
    AlteredPlan,

    #[error("synchronization plan belongs to project store {planned}, not {actual}")]
    WrongPlanProject { planned: PathBuf, actual: PathBuf },

    #[error("captured project root changed before synchronization committed: {path}")]
    ProjectRootChanged { path: PathBuf },

    #[error(
        "synchronization plan embedding scheme no longer matches: expected {expected:?}, found {actual:?}"
    )]
    PlanSchemeChanged {
        expected: EmbeddingScheme,
        actual: EmbeddingScheme,
    },

    #[error(transparent)]
    Scheme(#[from] EmbeddingSchemeError),

    #[error("project store is busy while trying to {operation} at {path}: {source}")]
    ProjectBusy {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("project store is corrupt while trying to {operation} at {path}: {source}")]
    ProjectCorrupt {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("project store constraint failed while trying to {operation} at {path}: {source}")]
    ProjectConstraint {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("project store failed while trying to {operation} at {path}: {source}")]
    ProjectDatabase {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },
}

impl SyncError {
    /// True when a concurrent writer still owns the project or vector store.
    pub fn is_transient_contention(&self) -> bool {
        match self {
            Self::Store(error) => error.is_transient_contention(),
            Self::Vector(error) => error.is_transient_contention(),
            Self::ProjectBusy { .. } => true,
            _ => false,
        }
    }
}

#[derive(Clone, Debug)]
struct ExistingDoc {
    corpus: String,
    status: Option<String>,
    content_hash: String,
    byte_length: i64,
    modified_ns: i64,
    device: i64,
    inode: i64,
}

impl ExistingDoc {
    fn metadata_matches(&self, page: &ScannedPage) -> Result<bool, SyncError> {
        Ok(self.corpus == corpus_label(page.corpus)
            && self.content_hash == page.sha256
            && self.byte_length == sqlite_integer(page.byte_len, page, "byte length")?
            && self.modified_ns
                == i64::try_from(page.modified_ns).map_err(|_| SyncError::MetadataOutOfRange {
                    path: page.path.clone(),
                    field: "modified nanoseconds",
                })?
            && self.device == sqlite_integer(page.device, page, "device")?
            && self.inode == sqlite_integer(page.inode, page, "inode")?)
    }
}

#[derive(Clone, Debug)]
struct StagedChunk {
    ordinal: u32,
    text: String,
    embed_text: String,
    vector_key: VectorKey,
}

#[derive(Clone, Debug)]
struct StagedPage<'a> {
    page: &'a PageUpsert,
    chunks: Vec<StagedChunk>,
}

trait SyncHooks {
    fn after_project_snapshot(&mut self) {}
    fn vector_busy_timeout(&mut self) -> Option<Duration> {
        None
    }
    fn after_vector_verification(&mut self) {}
    fn before_final_revalidation(&mut self) {}
}

struct NoSyncHooks;

impl SyncHooks for NoSyncHooks {}

/// Derives the complete chunk/model identity stored in schema v2.
pub fn embedding_scheme_for(embedder: &dyn Embedder) -> Result<EmbeddingScheme, SyncError> {
    if embedder.signature().is_empty() {
        return Err(SyncError::Vector(VectorError::EmptyModelSignature));
    }
    let mut digest = Sha256::new();
    for field in [
        SCHEME_FORMAT.as_bytes(),
        embedder.signature().as_bytes(),
        &u64::try_from(embedder.dimensions())
            .map_err(|_| EmbeddingError::DimensionOverflow)?
            .to_be_bytes(),
        EmbeddingRole::Passage.as_str().as_bytes(),
        &u64::try_from(MIN_CHUNK)
            .expect("chunk bound fits u64")
            .to_be_bytes(),
        &u64::try_from(MAX_CHUNK)
            .expect("chunk bound fits u64")
            .to_be_bytes(),
        CHUNK_LAYOUT.as_bytes(),
        TITLE_LAYOUT.as_bytes(),
        EMBED_TEXT_LAYOUT.as_bytes(),
    ] {
        digest.update(
            u64::try_from(field.len())
                .expect("scheme field length fits u64")
                .to_be_bytes(),
        );
        digest.update(field);
    }
    EmbeddingScheme::new(format!("{:x}", digest.finalize()), embedder.dimensions())
        .map_err(SyncError::from)
}

/// Reconciles one already-captured scan without rereading page content.
pub fn synchronize(
    home: &StoreHome,
    project_root: &Path,
    scan: &ScanReport,
    embedder: &mut dyn Embedder,
    mode: SyncMode,
) -> Result<SyncReport, SyncError> {
    synchronize_with_all_hooks(home, project_root, scan, embedder, mode, &mut NoSyncHooks)
}

#[cfg(test)]
fn synchronize_with_hooks(
    home: &StoreHome,
    project_root: &Path,
    scan: &ScanReport,
    embedder: &mut dyn Embedder,
    mode: SyncMode,
    hooks: &mut dyn SyncHooks,
) -> Result<SyncReport, SyncError> {
    synchronize_with_all_hooks(home, project_root, scan, embedder, mode, hooks)
}

fn synchronize_with_all_hooks(
    home: &StoreHome,
    project_root: &Path,
    scan: &ScanReport,
    embedder: &mut dyn Embedder,
    mode: SyncMode,
    hooks: &mut dyn SyncHooks,
) -> Result<SyncReport, SyncError> {
    let plan = plan_synchronization_with_hooks(home, project_root, scan, embedder, mode, hooks)?;
    execute_sync_plan_with_hooks(home, project_root, plan, embedder, hooks)
}

/// Captures deterministic synchronization work without embedding or changing
/// the project index.
///
/// The returned plan retains exact scan bytes and revision bindings. Vector
/// reuse is deliberately deferred until execution so an idle plan holds no
/// global cache-mutation lease.
pub fn plan_synchronization(
    home: &StoreHome,
    project_root: &Path,
    scan: &ScanReport,
    embedder: &dyn Embedder,
    mode: SyncMode,
) -> Result<SyncPlan, SyncError> {
    plan_synchronization_with_hooks(home, project_root, scan, embedder, mode, &mut NoSyncHooks)
}

fn plan_synchronization_with_hooks(
    home: &StoreHome,
    project_root: &Path,
    scan: &ScanReport,
    embedder: &dyn Embedder,
    mode: SyncMode,
    hooks: &mut dyn SyncHooks,
) -> Result<SyncPlan, SyncError> {
    scan.validate_snapshot().map_err(SyncError::SourceChanged)?;
    let notes = sorted_notes(scan);
    let scheme = embedding_scheme_for(embedder)?;
    let (project_path, project_binding) = bind_project_root(home, project_root)?;
    let project = open_project(home, project_root).map_err(classify_project_open)?;
    verify_project_root(&project_binding)?;
    let generation = read_generation(&project)
        .map_err(|source| classify_project_sql("read project generation", &project_path, source))?;
    let previous_scheme = read_embedding_scheme(&project).map_err(|source| {
        classify_project_sql("read project embedding scheme", &project_path, source)
    })?;
    let existing = read_existing_docs(&project, &project_path)?;
    let rebuild_scheme = previous_scheme.as_ref() != Some(&scheme);

    let present = scan
        .present
        .iter()
        .map(|page| (page.path.clone(), page))
        .collect::<BTreeMap<_, _>>();
    if mode == SyncMode::Incremental {
        let empty_indexed_corpora = readable_empty_indexed_corpora(scan, &existing);
        if !empty_indexed_corpora.is_empty() {
            return Err(SyncError::IncompleteIncremental { notes });
        }
    }
    let incremental_deletions = existing
        .keys()
        .filter(|path| !present.contains_key(*path))
        .filter(|path| positively_absent(path, scan))
        .cloned()
        .collect::<Vec<_>>();
    let retained = existing
        .keys()
        .filter(|path| !present.contains_key(*path))
        .filter(|path| !incremental_deletions.contains(path))
        .cloned()
        .collect::<Vec<_>>();
    if mode == SyncMode::FullRebuild {
        let mut uninspected = scan
            .unknown
            .iter()
            .map(|note| note.path.clone())
            .chain(retained.iter().cloned())
            .collect::<Vec<_>>();
        uninspected.sort();
        uninspected.dedup();
        if !uninspected.is_empty() {
            return Err(SyncError::IncompleteFullRebuild { uninspected, notes });
        }
    } else if rebuild_scheme && !retained.is_empty() {
        return Err(SyncError::IncompleteEmbeddingScheme { retained, notes });
    }
    let deletions = if mode == SyncMode::FullRebuild {
        existing
            .keys()
            .filter(|path| !present.contains_key(*path))
            .cloned()
            .collect::<Vec<_>>()
    } else {
        incremental_deletions
    };
    let needs_namespace_proof = mode == SyncMode::FullRebuild || !deletions.is_empty();
    let unchanged_paths = existing
        .iter()
        .filter(|(path, prior)| {
            present
                .get(*path)
                .is_some_and(|page| !rebuild_scheme && prior.content_hash == page.sha256)
        })
        .map(|(path, _)| path.clone())
        .collect::<BTreeSet<_>>();
    let unchanged_vector_references =
        if mode == SyncMode::Incremental && !unchanged_paths.is_empty() {
            read_existing_vector_keys(&project, &project_path, &unchanged_paths)?
        } else {
            BTreeMap::new()
        };
    let missing_vector_paths = if unchanged_vector_references.is_empty() {
        BTreeSet::new()
    } else {
        let requested = unchanged_vector_references
            .values()
            .flat_map(|keys| keys.iter().copied())
            .collect::<BTreeSet<_>>();
        if requested.is_empty() {
            BTreeSet::new()
        } else {
            let vectors = open_vector_cache(home, hooks)?;
            let missing = vectors.missing(&requested)?;
            unchanged_vector_references
                .iter()
                .filter(|(_, keys)| keys.iter().any(|key| missing.contains(key)))
                .map(|(path, _)| path.clone())
                .collect::<BTreeSet<_>>()
        }
    };

    drop(project);
    hooks.after_project_snapshot();
    verify_project_root(&project_binding)?;
    if needs_namespace_proof {
        scan.revalidate_namespaces()
            .map_err(SyncError::SourceChanged)?;
    }

    let mut upserts = Vec::new();
    for (path, page) in &present {
        let prior = existing.get(path);
        let fully_stale = mode == SyncMode::FullRebuild
            || rebuild_scheme
            || missing_vector_paths.contains(path)
            || prior.is_none_or(|doc| doc.content_hash != page.sha256);
        if fully_stale {
            upserts.push(plan_page(page)?);
        } else if let Some(prior) = prior
            && !prior.metadata_matches(page)?
        {
            upserts.push(plan_metadata_page(page, prior.status.clone())?);
        }
    }
    let unknown = sorted_unknown(scan);
    let seal = plan_seal(generation, &upserts, &deletions, &unknown);
    Ok(SyncPlan {
        generation,
        upserts,
        deletions,
        unknown,
        previous_scheme,
        target_scheme: scheme,
        mode,
        notes,
        scan: scan.clone(),
        unchanged_vector_references,
        project_path,
        project_binding,
        seal,
    })
}

/// Embeds and atomically publishes a previously captured [`SyncPlan`].
pub fn execute_sync_plan(
    home: &StoreHome,
    project_root: &Path,
    plan: SyncPlan,
    embedder: &mut dyn Embedder,
) -> Result<SyncReport, SyncError> {
    execute_sync_plan_with_hooks(home, project_root, plan, embedder, &mut NoSyncHooks)
}

fn execute_sync_plan_with_hooks(
    home: &StoreHome,
    project_root: &Path,
    mut plan: SyncPlan,
    embedder: &mut dyn Embedder,
    hooks: &mut dyn SyncHooks,
) -> Result<SyncReport, SyncError> {
    if plan.seal
        != plan_seal(
            plan.generation,
            &plan.upserts,
            &plan.deletions,
            &plan.unknown,
        )
    {
        return Err(SyncError::AlteredPlan);
    }
    let project_path = home.project_path(project_root)?;
    if project_path != plan.project_path {
        return Err(SyncError::WrongPlanProject {
            planned: plan.project_path,
            actual: project_path,
        });
    }
    verify_project_root(&plan.project_binding)?;
    let actual_target_scheme = embedding_scheme_for(embedder)?;
    if actual_target_scheme != plan.target_scheme {
        return Err(SyncError::PlanSchemeChanged {
            expected: plan.target_scheme,
            actual: actual_target_scheme,
        });
    }
    let project = open_project(home, project_root).map_err(classify_project_open)?;
    verify_project_root(&plan.project_binding)?;
    verify_project_preconditions(
        &project,
        &project_path,
        plan.generation,
        plan.previous_scheme.as_ref(),
    )?;
    drop(project);
    if !plan.unchanged_vector_references.is_empty() {
        let vector_lease = VectorMutationLease::acquire(home)?;
        vector_lease.revalidate()?;
        let requested = plan
            .unchanged_vector_references
            .values()
            .flat_map(|keys| keys.iter().copied())
            .collect::<BTreeSet<_>>();
        let vectors = open_vector_cache(home, hooks)?;
        let missing = vectors.missing(&requested)?;
        let missing_paths = plan
            .unchanged_vector_references
            .iter()
            .filter(|(_, keys)| keys.iter().any(|key| missing.contains(key)))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        drop(vectors);
        drop(vector_lease);
        for path in missing_paths {
            let page = plan
                .scan
                .present
                .iter()
                .find(|page| page.path == path)
                .expect("every unchanged vector reference belongs to a captured page");
            let replacement = plan_page(page)?;
            if let Some(upsert) = plan.upserts.iter_mut().find(|upsert| upsert.path() == path) {
                *upsert = replacement;
            } else {
                plan.upserts.push(replacement);
            }
        }
        plan.upserts
            .sort_by(|left, right| left.path.cmp(&right.path));
    }
    let changed = plan.upserts.len();
    let removed = plan.deletions.len();
    let needs_commit = plan.mode == SyncMode::FullRebuild
        || changed > 0
        || removed > 0
        || plan.previous_scheme.as_ref() != Some(&plan.target_scheme);
    if !needs_commit {
        return Ok(SyncReport {
            changed: 0,
            removed: 0,
            embedded: 0,
            generation: plan.generation,
            notes: plan.notes,
        });
    }

    let model_signature = embedder.signature().to_owned();
    let staged = plan
        .upserts
        .iter()
        .filter(|page| page.kind == UpsertKind::Replace)
        .map(|page| stage_for_embedding(page, &model_signature))
        .collect::<Result<Vec<_>, _>>()?;

    let requested = staged
        .iter()
        .flat_map(|page| page.chunks.iter().map(|chunk| chunk.vector_key))
        .collect::<BTreeSet<_>>();
    let texts = staged
        .iter()
        .flat_map(|page| &page.chunks)
        .map(|chunk| (chunk.vector_key, chunk.embed_text.clone()))
        .collect::<BTreeMap<_, _>>();
    let initially_missing = if requested.is_empty() {
        BTreeSet::new()
    } else {
        let vectors = open_vector_cache(home, hooks)?;
        let cached = vectors.get_many(&requested)?;
        validate_cached_vectors(&cached, embedder)?;
        requested
            .difference(&cached.keys().copied().collect())
            .copied()
            .collect()
    };
    let mut embedded = BTreeMap::new();
    embed_keys(&initially_missing, &texts, embedder, &mut embedded)?;

    revalidate_upserts(&plan.upserts)?;
    revalidate_deletion_namespaces(&plan)?;

    // Embedding is intentionally outside the global mutation lease. Once the
    // lease is held, repeat every cache and project precondition before any
    // vector insertion, then retain it until project publication commits.
    let vector_lease = VectorMutationLease::acquire(home)?;
    vector_lease.revalidate()?;
    let mut project = open_project(home, project_root).map_err(classify_project_open)?;
    verify_project_root(&plan.project_binding)?;
    verify_project_preconditions(
        &project,
        &project_path,
        plan.generation,
        plan.previous_scheme.as_ref(),
    )?;
    if !requested.is_empty() {
        let mut vectors = open_vector_cache(home, hooks)?;
        let cached = vectors.get_many(&requested)?;
        validate_cached_vectors(&cached, embedder)?;
        for (key, (_, expected)) in &embedded {
            if cached
                .get(key)
                .is_some_and(|actual| actual.embedding() != expected)
            {
                return Err(SyncError::Vector(VectorError::VectorCollision {
                    key: *key,
                }));
            }
        }
        let missing_after_lease = requested
            .difference(&cached.keys().copied().collect())
            .copied()
            .collect::<BTreeSet<_>>();
        let newly_missing = missing_after_lease
            .difference(&embedded.keys().copied().collect())
            .copied()
            .collect::<BTreeSet<_>>();
        embed_keys(&newly_missing, &texts, embedder, &mut embedded)?;
        let inserts = missing_after_lease
            .iter()
            .map(|key| {
                let (text, embedding) = embedded
                    .get(key)
                    .expect("every missing vector was embedded before insertion");
                VectorInsert::new(
                    *key,
                    &model_signature,
                    EmbeddingRole::Passage,
                    text,
                    embedding,
                )
            })
            .collect::<Vec<_>>();
        vectors.insert_batch(&inserts)?;
        let committed_vectors = vectors.get_many(&requested)?;
        validate_cached_vectors(&committed_vectors, embedder)?;
        if let Some(key) = requested
            .iter()
            .find(|key| !committed_vectors.contains_key(key))
        {
            return Err(SyncError::MissingCachedVector { key: *key });
        }
    }
    vector_lease.revalidate()?;
    hooks.after_vector_verification();

    revalidate_deletion_namespaces(&plan)?;
    revalidate_upserts(&plan.upserts)?;

    let next_generation = plan
        .generation
        .checked_add(1)
        .ok_or(SyncError::GenerationOverflow {
            generation: plan.generation,
        })?;
    let transaction = project
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(|source| {
            classify_project_sql("begin immediate synchronization", &project_path, source)
        })?;
    verify_project_preconditions(
        &transaction,
        &project_path,
        plan.generation,
        plan.previous_scheme.as_ref(),
    )?;
    if plan.mode == SyncMode::FullRebuild {
        clear_project(&transaction).map_err(|source| {
            classify_project_sql("clear project for full rebuild", &project_path, source)
        })?;
    } else {
        for path in &plan.deletions {
            delete_page(&transaction, path).map_err(|source| {
                classify_project_sql("delete positively absent page", &project_path, source)
            })?;
        }
    }
    let staged_by_path = staged
        .iter()
        .map(|page| (page.page.path.as_str(), page))
        .collect::<BTreeMap<_, _>>();
    for page in &plan.upserts {
        match page.kind {
            UpsertKind::Replace => {
                let staged_page = staged_by_path
                    .get(page.path.as_str())
                    .expect("every replacement was staged");
                upsert_page(&transaction, staged_page, next_generation).map_err(|source| {
                    classify_project_sql("replace synchronized page", &project_path, source)
                })?;
            }
            UpsertKind::MetadataOnly => {
                update_page_metadata(&transaction, page, next_generation).map_err(|source| {
                    classify_project_sql("update synchronized page metadata", &project_path, source)
                })?;
            }
        }
    }
    write_embedding_scheme(&transaction, Some(&plan.target_scheme)).map_err(|source| {
        classify_project_sql("stamp project embedding scheme", &project_path, source)
    })?;
    transaction
        .execute(
            "UPDATE metadata SET generation = ?1 WHERE singleton = 1",
            [next_generation],
        )
        .map_err(|source| {
            classify_project_sql("advance project generation", &project_path, source)
        })?;
    hooks.before_final_revalidation();
    verify_project_root(&plan.project_binding)?;
    revalidate_deletion_namespaces(&plan)?;
    revalidate_upserts(&plan.upserts)?;
    vector_lease.revalidate()?;
    verify_project_root(&plan.project_binding)?;
    transaction.commit().map_err(|source| {
        classify_project_sql("commit project synchronization", &project_path, source)
    })?;

    Ok(SyncReport {
        changed,
        removed,
        embedded: embedded.len(),
        generation: next_generation,
        notes: plan.notes,
    })
}

fn validate_cached_vectors(
    cached_vectors: &BTreeMap<VectorKey, CachedVector>,
    embedder: &dyn Embedder,
) -> Result<(), SyncError> {
    for cached in cached_vectors.values() {
        if cached.model_signature() != embedder.signature() {
            return Err(SyncError::CachedSignature {
                key: cached.key(),
                expected: embedder.signature().to_owned(),
                actual: cached.model_signature().to_owned(),
            });
        }
        if cached.dimensions() != embedder.dimensions() {
            return Err(SyncError::CachedDimensions {
                key: cached.key(),
                expected: embedder.dimensions(),
                actual: cached.dimensions(),
            });
        }
    }
    Ok(())
}

fn verify_project_root(binding: &ProjectRootBinding) -> Result<(), SyncError> {
    if binding.revalidate() {
        Ok(())
    } else {
        Err(SyncError::ProjectRootChanged {
            path: binding.path().to_path_buf(),
        })
    }
}

fn open_vector_cache(
    home: &StoreHome,
    hooks: &mut dyn SyncHooks,
) -> Result<VectorCache, SyncError> {
    match hooks.vector_busy_timeout() {
        Some(timeout) => {
            VectorCache::open_with_busy_timeout(home, timeout).map_err(SyncError::from)
        }
        None => VectorCache::open(home).map_err(SyncError::from),
    }
}

fn embed_keys(
    keys: &BTreeSet<VectorKey>,
    texts: &BTreeMap<VectorKey, String>,
    embedder: &mut dyn Embedder,
    destination: &mut BTreeMap<VectorKey, (String, Embedding)>,
) -> Result<(), SyncError> {
    let dimensions = embedder.dimensions();
    let ordered = keys.iter().collect::<Vec<_>>();
    for batch in ordered.chunks(EMBED_BATCH_SIZE) {
        let batch_text = batch
            .iter()
            .map(|key| {
                texts
                    .get(key)
                    .expect("every requested vector key has exact embedding text")
                    .clone()
            })
            .collect::<Vec<_>>();
        let batch_embeddings = embedder.embed_passages(&batch_text)?;
        if batch_embeddings.len() != batch.len() {
            return Err(SyncError::Embedding(EmbeddingError::CardinalityMismatch {
                expected: batch.len(),
                actual: batch_embeddings.len(),
            }));
        }
        for ((key, text), embedding) in batch.iter().zip(batch_text).zip(batch_embeddings) {
            if embedding.dimensions() != dimensions {
                return Err(SyncError::Embedding(EmbeddingError::DimensionMismatch {
                    expected: dimensions,
                    actual: embedding.dimensions(),
                }));
            }
            destination.insert(**key, (text, embedding));
        }
    }
    Ok(())
}

fn positively_absent(path: &Path, scan: &ScanReport) -> bool {
    if scan.unknown.iter().any(|note| path.starts_with(&note.path)) {
        return false;
    }
    scan.oversized
        .iter()
        .chain(&scan.rejected)
        .any(|note| path.starts_with(&note.path))
        || scan
            .scanned_corpora
            .iter()
            .any(|root| path.starts_with(root))
}

fn plan_page(page: &ScannedPage) -> Result<PageUpsert, SyncError> {
    let source = std::str::from_utf8(page.content_bytes()).map_err(|_| SyncError::InvalidUtf8 {
        path: page.path.clone(),
    })?;
    let parsed = parse_frontmatter(source);
    let status = parsed
        .fields
        .get("status")
        .filter(|value| matches!(value.as_str(), "current" | "historical" | "in-progress"))
        .cloned();
    let chunks = chunks_for_page(&page.path, source)?;
    page_upsert(page, status, chunks, UpsertKind::Replace)
}

fn plan_metadata_page(page: &ScannedPage, status: Option<String>) -> Result<PageUpsert, SyncError> {
    page_upsert(page, status, Vec::new(), UpsertKind::MetadataOnly)
}

fn page_upsert(
    page: &ScannedPage,
    status: Option<String>,
    chunks: Vec<Chunk>,
    kind: UpsertKind,
) -> Result<PageUpsert, SyncError> {
    let path = page
        .path
        .to_str()
        .ok_or_else(|| SyncError::InvalidUtf8 {
            path: page.path.clone(),
        })?
        .to_owned();
    Ok(PageUpsert {
        source: page.clone(),
        path,
        corpus: corpus_label(page.corpus),
        status,
        byte_length: sqlite_integer(page.byte_len, page, "byte length")?,
        modified_ns: i64::try_from(page.modified_ns).map_err(|_| {
            SyncError::MetadataOutOfRange {
                path: page.path.clone(),
                field: "modified nanoseconds",
            }
        })?,
        device: sqlite_integer(page.device, page, "device")?,
        inode: sqlite_integer(page.inode, page, "inode")?,
        chunks,
        kind,
    })
}

fn stage_for_embedding<'a>(
    page: &'a PageUpsert,
    model_signature: &str,
) -> Result<StagedPage<'a>, SyncError> {
    let chunks = page
        .chunks
        .iter()
        .cloned()
        .map(|chunk| {
            Ok(StagedChunk {
                ordinal: chunk.ordinal,
                vector_key: vector_key(model_signature, EmbeddingRole::Passage, &chunk.embed_text)?,
                text: chunk.text,
                embed_text: chunk.embed_text,
            })
        })
        .collect::<Result<_, SyncError>>()?;
    Ok(StagedPage { page, chunks })
}

fn sqlite_integer(value: u64, page: &ScannedPage, field: &'static str) -> Result<i64, SyncError> {
    i64::try_from(value).map_err(|_| SyncError::MetadataOutOfRange {
        path: page.path.clone(),
        field,
    })
}

const fn corpus_label(corpus: CorpusKind) -> &'static str {
    match corpus {
        CorpusKind::Shared => "shared",
        CorpusKind::Private => "private",
        CorpusKind::Override => "override",
    }
}

fn read_generation(connection: &Connection) -> Result<i64, rusqlite::Error> {
    connection.query_row(
        "SELECT generation FROM metadata WHERE singleton = 1",
        [],
        |row| row.get(0),
    )
}

fn verify_project_preconditions(
    connection: &Connection,
    project_path: &Path,
    expected_generation: i64,
    expected_scheme: Option<&EmbeddingScheme>,
) -> Result<(), SyncError> {
    let actual_generation = read_generation(connection).map_err(|source| {
        classify_project_sql("recheck project generation", project_path, source)
    })?;
    if actual_generation != expected_generation {
        return Err(SyncError::ProjectChanged {
            expected: expected_generation,
            actual: actual_generation,
        });
    }
    let actual_scheme = read_embedding_scheme(connection).map_err(|source| {
        classify_project_sql("recheck project embedding scheme", project_path, source)
    })?;
    if actual_scheme.as_ref() != expected_scheme {
        return Err(SyncError::ProjectSchemeChanged {
            expected: expected_scheme.cloned(),
            actual: actual_scheme,
        });
    }
    Ok(())
}

fn read_existing_docs(
    connection: &Connection,
    project_path: &Path,
) -> Result<BTreeMap<PathBuf, ExistingDoc>, SyncError> {
    let mut statement = connection
        .prepare(
            "SELECT path, corpus, status, content_hash, byte_length, mtime_ns, device, inode \
             FROM docs ORDER BY path",
        )
        .map_err(|source| {
            classify_project_sql("prepare project document inventory", project_path, source)
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                PathBuf::from(row.get::<_, String>(0)?),
                ExistingDoc {
                    corpus: row.get(1)?,
                    status: row.get(2)?,
                    content_hash: row.get(3)?,
                    byte_length: row.get(4)?,
                    modified_ns: row.get(5)?,
                    device: row.get(6)?,
                    inode: row.get(7)?,
                },
            ))
        })
        .map_err(|source| {
            classify_project_sql("query project document inventory", project_path, source)
        })?;
    rows.collect::<Result<_, _>>().map_err(|source| {
        classify_project_sql("read project document inventory", project_path, source)
    })
}

fn read_existing_vector_keys(
    connection: &Connection,
    project_path: &Path,
    candidates: &BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, BTreeSet<VectorKey>>, SyncError> {
    let mut statement = connection
        .prepare("SELECT path, vector_hash FROM chunks ORDER BY path, ordinal")
        .map_err(|source| {
            classify_project_sql("prepare project vector-key inventory", project_path, source)
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|source| {
            classify_project_sql("query project vector-key inventory", project_path, source)
        })?;
    let mut result = BTreeMap::<PathBuf, BTreeSet<VectorKey>>::new();
    for row in rows {
        let (path, encoded) = row.map_err(|source| {
            classify_project_sql("read project vector-key inventory", project_path, source)
        })?;
        let path = PathBuf::from(path);
        if !candidates.contains(&path) {
            continue;
        }
        let key = encoded
            .parse()
            .map_err(|source| SyncError::InvalidProjectVectorKey {
                path: path.clone(),
                value: encoded,
                source,
            })?;
        result.entry(path).or_default().insert(key);
    }
    Ok(result)
}

fn readable_empty_indexed_corpora(
    scan: &ScanReport,
    existing: &BTreeMap<PathBuf, ExistingDoc>,
) -> Vec<PathBuf> {
    let mut empty = scan
        .scanned_corpora
        .iter()
        .filter(|root| {
            let has_unaccounted_indexed = existing.keys().any(|path| {
                path.starts_with(root)
                    && !scan
                        .oversized
                        .iter()
                        .chain(scan.rejected.iter())
                        .any(|note| path.starts_with(&note.path))
            });
            let has_present = scan.present.iter().any(|page| page.path.starts_with(root));
            has_unaccounted_indexed && !has_present
        })
        .cloned()
        .collect::<Vec<_>>();
    empty.sort();
    empty
}

fn upsert_page(
    transaction: &Transaction<'_>,
    page: &StagedPage<'_>,
    generation: i64,
) -> Result<(), rusqlite::Error> {
    let planned = page.page;
    transaction.execute(
        "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE path = ?1)",
        [&planned.path],
    )?;
    transaction.execute("DELETE FROM chunks WHERE path = ?1", [&planned.path])?;
    transaction.execute(
        "INSERT INTO docs \
         (path, corpus, status, content_hash, byte_length, mtime_ns, device, inode, generation) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9) \
         ON CONFLICT(path) DO UPDATE SET \
           corpus = excluded.corpus, status = excluded.status, \
           content_hash = excluded.content_hash, byte_length = excluded.byte_length, \
           mtime_ns = excluded.mtime_ns, device = excluded.device, \
           inode = excluded.inode, generation = excluded.generation",
        params![
            planned.path,
            planned.corpus,
            planned.status,
            planned.source.sha256,
            planned.byte_length,
            planned.modified_ns,
            planned.device,
            planned.inode,
            generation,
        ],
    )?;
    for chunk in &page.chunks {
        transaction.execute(
            "INSERT INTO chunks(path, ordinal, text, embed_text, vector_hash) \
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                planned.path,
                i64::from(chunk.ordinal),
                chunk.text,
                chunk.embed_text,
                chunk.vector_key.to_string(),
            ],
        )?;
        let rowid = transaction.last_insert_rowid();
        transaction.execute(
            "INSERT INTO chunks_fts(rowid, text) VALUES (?1, ?2)",
            params![rowid, chunk.text],
        )?;
    }
    Ok(())
}

fn update_page_metadata(
    transaction: &Transaction<'_>,
    page: &PageUpsert,
    generation: i64,
) -> Result<(), rusqlite::Error> {
    transaction.execute(
        "UPDATE docs SET corpus = ?2, status = ?3, content_hash = ?4, byte_length = ?5, \
         mtime_ns = ?6, device = ?7, inode = ?8, generation = ?9 WHERE path = ?1",
        params![
            page.path,
            page.corpus,
            page.status,
            page.source.sha256,
            page.byte_length,
            page.modified_ns,
            page.device,
            page.inode,
            generation,
        ],
    )?;
    Ok(())
}

fn delete_page(transaction: &Transaction<'_>, path: &Path) -> Result<(), rusqlite::Error> {
    let path = path
        .to_str()
        .expect("persisted SQLite paths were decoded from UTF-8 text");
    transaction.execute(
        "DELETE FROM chunks_fts WHERE rowid IN (SELECT id FROM chunks WHERE path = ?1)",
        [path],
    )?;
    transaction.execute("DELETE FROM docs WHERE path = ?1", [path])?;
    Ok(())
}

fn clear_project(transaction: &Transaction<'_>) -> Result<(), rusqlite::Error> {
    transaction.execute("DELETE FROM chunks_fts", [])?;
    transaction.execute("DELETE FROM docs", [])?;
    Ok(())
}

fn revalidate_upserts(pages: &[PageUpsert]) -> Result<(), SyncError> {
    for page in pages {
        page.source.revalidate().map_err(SyncError::SourceChanged)?;
    }
    Ok(())
}

fn revalidate_deletion_namespaces(plan: &SyncPlan) -> Result<(), SyncError> {
    if plan.mode == SyncMode::FullRebuild || !plan.deletions.is_empty() {
        plan.scan
            .revalidate_namespaces()
            .map_err(SyncError::SourceChanged)?;
    }
    Ok(())
}

fn sorted_notes(scan: &ScanReport) -> Vec<ScanNote> {
    sort_notes(
        scan.oversized
            .iter()
            .chain(&scan.rejected)
            .chain(&scan.unknown)
            .cloned()
            .collect(),
    )
}

fn sorted_unknown(scan: &ScanReport) -> Vec<ScanNote> {
    sort_notes(scan.unknown.clone())
}

fn sort_notes(mut notes: Vec<ScanNote>) -> Vec<ScanNote> {
    notes.sort_by(|left, right| {
        (&left.path, left.kind, &left.detail).cmp(&(&right.path, right.kind, &right.detail))
    });
    notes.dedup();
    notes
}

fn plan_seal(
    generation: i64,
    upserts: &[PageUpsert],
    deletions: &[PathBuf],
    unknown: &[ScanNote],
) -> [u8; 32] {
    let mut digest = Sha256::new();
    hash_plan_field(&mut digest, b"yams-sync-plan-v1");
    hash_plan_field(&mut digest, &generation.to_be_bytes());
    hash_plan_length(&mut digest, upserts.len());
    for page in upserts {
        hash_plan_field(&mut digest, b"upsert");
        hash_plan_field(&mut digest, page.source.path.as_os_str().as_bytes());
        hash_plan_field(&mut digest, page.corpus.as_bytes());
        match &page.status {
            Some(status) => {
                digest.update([1]);
                hash_plan_field(&mut digest, status.as_bytes());
            }
            None => digest.update([0]),
        }
        hash_plan_field(&mut digest, page.source.sha256.as_bytes());
        hash_plan_field(&mut digest, &page.byte_length.to_be_bytes());
        hash_plan_field(&mut digest, &page.modified_ns.to_be_bytes());
        hash_plan_field(&mut digest, &page.device.to_be_bytes());
        hash_plan_field(&mut digest, &page.inode.to_be_bytes());
        digest.update([match page.kind {
            UpsertKind::Replace => 1,
            UpsertKind::MetadataOnly => 2,
        }]);
        hash_plan_length(&mut digest, page.chunks.len());
        for chunk in &page.chunks {
            hash_plan_field(&mut digest, &chunk.ordinal.to_be_bytes());
            hash_plan_field(&mut digest, chunk.text.as_bytes());
            hash_plan_field(&mut digest, chunk.embed_text.as_bytes());
        }
    }
    hash_plan_length(&mut digest, deletions.len());
    for path in deletions {
        hash_plan_field(&mut digest, b"deletion");
        hash_plan_field(&mut digest, path.as_os_str().as_bytes());
    }
    hash_plan_length(&mut digest, unknown.len());
    for note in unknown {
        hash_plan_field(&mut digest, b"unknown");
        hash_plan_field(&mut digest, note.path.as_os_str().as_bytes());
        hash_plan_field(&mut digest, format!("{:?}", note.kind).as_bytes());
        hash_plan_field(&mut digest, note.detail.as_bytes());
    }
    digest.finalize().into()
}

fn hash_plan_length(digest: &mut Sha256, length: usize) {
    hash_plan_field(
        digest,
        &u64::try_from(length)
            .expect("in-memory plan collection length fits u64")
            .to_be_bytes(),
    );
}

fn hash_plan_field(digest: &mut Sha256, field: &[u8]) {
    digest.update(
        u64::try_from(field.len())
            .expect("in-memory plan field length fits u64")
            .to_be_bytes(),
    );
    digest.update(field);
}

fn classify_project_sql(
    operation: &'static str,
    path: &Path,
    source: rusqlite::Error,
) -> SyncError {
    let code = match &source {
        rusqlite::Error::SqliteFailure(failure, _) => Some(failure.code),
        _ => None,
    };
    match code {
        Some(ErrorCode::DatabaseBusy | ErrorCode::DatabaseLocked) => SyncError::ProjectBusy {
            operation,
            path: path.to_owned(),
            source,
        },
        Some(ErrorCode::DatabaseCorrupt | ErrorCode::NotADatabase) => SyncError::ProjectCorrupt {
            operation,
            path: path.to_owned(),
            source,
        },
        Some(ErrorCode::ConstraintViolation) => SyncError::ProjectConstraint {
            operation,
            path: path.to_owned(),
            source,
        },
        _ => SyncError::ProjectDatabase {
            operation,
            path: path.to_owned(),
            source,
        },
    }
}

fn classify_project_open(source: StoreError) -> SyncError {
    match source {
        StoreError::Database {
            operation,
            path,
            source,
        } => classify_project_sql(operation, &path, source),
        other => SyncError::Store(other),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::Duration;

    use rusqlite::Connection;
    use tempfile::tempdir;
    use yams_core::{Corpus, CorpusKind, scan_corpora};
    use yams_embed::{Embedder, Embedding, EmbeddingError, FakeEmbedder};

    use super::{SyncError, SyncHooks, SyncMode, synchronize, synchronize_with_hooks};
    use crate::{
        StoreError, StoreHome, VectorCache, VectorError, VectorKey, VectorMutationLease,
        open_project, read_embedding_scheme,
    };

    #[derive(Debug, Eq, PartialEq)]
    struct State {
        generation: i64,
        scheme: Option<crate::EmbeddingScheme>,
        docs: Vec<(String, String, i64)>,
        chunks: Vec<(String, i64, String, String)>,
        fts: Vec<(i64, String)>,
    }

    fn state(home: &StoreHome, root: &Path) -> State {
        let connection = open_project(home, root).unwrap();
        State {
            generation: connection
                .query_row(
                    "SELECT generation FROM metadata WHERE singleton = 1",
                    [],
                    |row| row.get(0),
                )
                .unwrap(),
            scheme: read_embedding_scheme(&connection).unwrap(),
            docs: rows3(
                &connection,
                "SELECT path, content_hash, generation FROM docs ORDER BY path",
            ),
            chunks: rows4(
                &connection,
                "SELECT path, ordinal, text, vector_hash FROM chunks ORDER BY path, ordinal",
            ),
            fts: rows2(
                &connection,
                "SELECT rowid, text FROM chunks_fts ORDER BY rowid",
            ),
        }
    }

    fn rows2(connection: &Connection, sql: &str) -> Vec<(i64, String)> {
        let mut statement = connection.prepare(sql).unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn rows3(connection: &Connection, sql: &str) -> Vec<(String, String, i64)> {
        let mut statement = connection.prepare(sql).unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    fn rows4(connection: &Connection, sql: &str) -> Vec<(String, i64, String, String)> {
        let mut statement = connection.prepare(sql).unwrap();
        statement
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    }

    struct ChangeBeforeCommit {
        path: PathBuf,
    }

    impl SyncHooks for ChangeBeforeCommit {
        fn before_final_revalidation(&mut self) {
            std::fs::write(&self.path, "changed after project staging").unwrap();
        }
    }

    struct PauseAfterVectorVerification {
        reached: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    }

    struct HoldVectorWriterAfterMissing {
        inner: FakeEmbedder,
        vector_path: PathBuf,
        holder: Option<Connection>,
    }

    struct PauseDuringEmbedding {
        inner: FakeEmbedder,
        reached: mpsc::Sender<()>,
        resume: mpsc::Receiver<()>,
    }

    impl Embedder for PauseDuringEmbedding {
        fn signature(&self) -> &str {
            self.inner.signature()
        }

        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }

        fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
            self.reached.send(()).unwrap();
            self.resume.recv_timeout(Duration::from_secs(2)).unwrap();
            self.inner.embed_passages(texts)
        }

        fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
            self.inner.embed_query(text)
        }
    }

    impl Embedder for HoldVectorWriterAfterMissing {
        fn signature(&self) -> &str {
            self.inner.signature()
        }

        fn dimensions(&self) -> usize {
            self.inner.dimensions()
        }

        fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
            let embeddings = self.inner.embed_passages(texts)?;
            let holder = Connection::open(&self.vector_path).unwrap();
            holder.execute_batch("BEGIN IMMEDIATE").unwrap();
            self.holder = Some(holder);
            Ok(embeddings)
        }

        fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
            self.inner.embed_query(text)
        }
    }

    struct ImmediateVectorBusy;

    impl SyncHooks for ImmediateVectorBusy {
        fn vector_busy_timeout(&mut self) -> Option<Duration> {
            Some(Duration::ZERO)
        }
    }

    struct WaitAfterProjectSnapshot {
        barrier: Arc<Barrier>,
    }

    /// Holds one racer inside its captured project snapshot until a peer that
    /// captured the same snapshot has finished publishing.
    struct WaitForPublishedPeer {
        barrier: Arc<Barrier>,
        published: mpsc::Receiver<()>,
    }

    #[derive(Clone, Copy)]
    enum AncestorReplacement {
        Directory,
        Symlink,
    }

    fn replace_ancestor(path: &Path, backup: &Path, replacement: AncestorReplacement) {
        std::fs::rename(path, backup).unwrap();
        match replacement {
            AncestorReplacement::Directory => {
                std::fs::create_dir(path).unwrap();
                std::fs::write(path.join("alpha.md"), "alpha original").unwrap();
            }
            AncestorReplacement::Symlink => symlink(backup, path).unwrap(),
        }
    }

    struct ReplaceAncestorAfterVectors {
        path: PathBuf,
        backup: PathBuf,
        replacement: AncestorReplacement,
    }

    impl SyncHooks for ReplaceAncestorAfterVectors {
        fn after_vector_verification(&mut self) {
            replace_ancestor(&self.path, &self.backup, self.replacement);
        }
    }

    struct ReplaceAncestorBeforeCommit {
        path: PathBuf,
        backup: PathBuf,
        replacement: AncestorReplacement,
    }

    #[derive(Clone, Copy)]
    enum NamespaceMutationPoint {
        AfterProjectSnapshot,
        BeforeCommit,
    }

    struct AddNamespaceEntry {
        point: NamespaceMutationPoint,
        path: PathBuf,
    }

    struct ReplaceProjectRootBeforeCommit {
        root: PathBuf,
        backup: PathBuf,
    }

    impl SyncHooks for ReplaceProjectRootBeforeCommit {
        fn before_final_revalidation(&mut self) {
            std::fs::rename(&self.root, &self.backup).unwrap();
            std::fs::create_dir(&self.root).unwrap();
        }
    }

    impl SyncHooks for AddNamespaceEntry {
        fn after_project_snapshot(&mut self) {
            if matches!(self.point, NamespaceMutationPoint::AfterProjectSnapshot) {
                std::fs::write(&self.path, "arrived after scan").unwrap();
            }
        }

        fn before_final_revalidation(&mut self) {
            if matches!(self.point, NamespaceMutationPoint::BeforeCommit) {
                std::fs::write(&self.path, "arrived before commit").unwrap();
            }
        }
    }

    impl SyncHooks for ReplaceAncestorBeforeCommit {
        fn before_final_revalidation(&mut self) {
            replace_ancestor(&self.path, &self.backup, self.replacement);
        }
    }

    impl SyncHooks for WaitAfterProjectSnapshot {
        fn after_project_snapshot(&mut self) {
            self.barrier.wait();
        }
    }

    impl SyncHooks for WaitForPublishedPeer {
        fn after_project_snapshot(&mut self) {
            self.barrier.wait();
            self.published.recv().unwrap();
        }
    }

    impl SyncHooks for PauseAfterVectorVerification {
        fn after_vector_verification(&mut self) {
            self.reached.send(()).unwrap();
            self.resume.recv().unwrap();
        }
    }

    #[test]
    fn final_revision_failure_rolls_back_docs_chunks_fts_generation_and_stamp() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("orrery");
        let corpus_path = root.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        let page_path = corpus_path.join("alpha.md");
        std::fs::write(&page_path, "alpha original").unwrap();
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let home = StoreHome::new(directory.path().join("state"));
        let mut embedder = FakeEmbedder::new();
        synchronize(
            &home,
            &root,
            &scan_corpora(std::slice::from_ref(&corpus)),
            &mut embedder,
            SyncMode::Incremental,
        )
        .unwrap();
        let before = state(&home, &root);

        std::fs::write(&page_path, "alpha replacement").unwrap();
        let scan = scan_corpora(&[corpus]);
        let error = synchronize_with_hooks(
            &home,
            &root,
            &scan,
            &mut embedder,
            SyncMode::Incremental,
            &mut ChangeBeforeCommit { path: page_path },
        )
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged(_)));
        assert_eq!(state(&home, &root), before);
    }

    #[test]
    fn full_rebuild_empty_scan_revalidates_namespace_before_vectors_and_commit() {
        for point in [
            NamespaceMutationPoint::AfterProjectSnapshot,
            NamespaceMutationPoint::BeforeCommit,
        ] {
            let directory = tempdir().unwrap();
            let root = directory.path().join("orrery");
            let corpus_path = root.join(".agents/memory");
            std::fs::create_dir_all(&corpus_path).unwrap();
            let original = corpus_path.join("alpha.md");
            std::fs::write(&original, "alpha original").unwrap();
            let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
            let home = StoreHome::new(directory.path().join("state"));
            let mut embedder = FakeEmbedder::new();
            synchronize(
                &home,
                &root,
                &scan_corpora(std::slice::from_ref(&corpus)),
                &mut embedder,
                SyncMode::Incremental,
            )
            .unwrap();
            let before = state(&home, &root);
            std::fs::remove_file(original).unwrap();
            let empty = scan_corpora(&[corpus]);

            let error = synchronize_with_hooks(
                &home,
                &root,
                &empty,
                &mut embedder,
                SyncMode::FullRebuild,
                &mut AddNamespaceEntry {
                    point,
                    path: corpus_path.join("late.md"),
                },
            )
            .unwrap_err();

            assert!(matches!(error, SyncError::SourceChanged(_)));
            assert_eq!(state(&home, &root), before);
        }
    }

    #[test]
    fn incremental_deletion_revalidates_namespace_before_vectors_and_commit() {
        for point in [
            NamespaceMutationPoint::AfterProjectSnapshot,
            NamespaceMutationPoint::BeforeCommit,
        ] {
            let directory = tempdir().unwrap();
            let root = directory.path().join("orrery");
            let corpus_path = root.join(".agents/memory");
            std::fs::create_dir_all(&corpus_path).unwrap();
            std::fs::write(corpus_path.join("alpha.md"), "alpha original").unwrap();
            let removed = corpus_path.join("beta.md");
            std::fs::write(&removed, "beta original").unwrap();
            let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
            let home = StoreHome::new(directory.path().join("state"));
            let mut embedder = FakeEmbedder::new();
            synchronize(
                &home,
                &root,
                &scan_corpora(std::slice::from_ref(&corpus)),
                &mut embedder,
                SyncMode::Incremental,
            )
            .unwrap();
            let before = state(&home, &root);
            std::fs::remove_file(removed).unwrap();
            let scan = scan_corpora(&[corpus]);

            let error = synchronize_with_hooks(
                &home,
                &root,
                &scan,
                &mut embedder,
                SyncMode::Incremental,
                &mut AddNamespaceEntry {
                    point,
                    path: corpus_path.join("late.md"),
                },
            )
            .unwrap_err();

            assert!(matches!(error, SyncError::SourceChanged(_)));
            assert_eq!(state(&home, &root), before);
        }
    }

    #[test]
    fn project_root_replacement_at_final_checkpoint_rolls_back_project_state() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("orrery");
        let corpus_path = directory.path().join("independent-corpus");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&corpus_path).unwrap();
        std::fs::write(corpus_path.join("alpha.md"), "alpha original").unwrap();
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Override).unwrap();
        let scan = scan_corpora(&[corpus]);
        let home = StoreHome::new(directory.path().join("state"));
        let before = state(&home, &root);
        let mut embedder = FakeEmbedder::new();

        let error = synchronize_with_hooks(
            &home,
            &root,
            &scan,
            &mut embedder,
            SyncMode::Incremental,
            &mut ReplaceProjectRootBeforeCommit {
                root: root.clone(),
                backup: directory.path().join("old-orrery"),
            },
        )
        .unwrap_err();

        assert!(matches!(error, SyncError::ProjectRootChanged { .. }));
        assert_eq!(state(&home, &root), before);
    }

    #[test]
    fn vector_writer_after_missing_is_typed_busy_and_publishes_no_project_rows() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("orrery");
        let corpus_path = root.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        std::fs::write(corpus_path.join("alpha.md"), "alpha original").unwrap();
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let scan = scan_corpora(&[corpus]);
        let home = StoreHome::new(directory.path().join("state"));
        let before = state(&home, &root);
        let mut embedder = HoldVectorWriterAfterMissing {
            inner: FakeEmbedder::new(),
            vector_path: home.vectors_path(),
            holder: None,
        };

        let error = synchronize_with_hooks(
            &home,
            &root,
            &scan,
            &mut embedder,
            SyncMode::Incremental,
            &mut ImmediateVectorBusy,
        )
        .unwrap_err();

        assert!(
            matches!(
                error,
                SyncError::Vector(crate::VectorError::Busy { .. })
                    | SyncError::Vector(crate::VectorError::Store(
                        crate::StoreError::UnsafeSidecar { .. }
                    ))
            ),
            "{error:?}"
        );
        assert_eq!(state(&home, &root), before);
    }

    #[test]
    fn unrelated_vector_mutation_lease_remains_available_during_embedding() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("orrery");
        let corpus_path = root.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        std::fs::write(corpus_path.join("alpha.md"), "alpha original").unwrap();
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let scan = scan_corpora(&[corpus]);
        let home = StoreHome::new(directory.path().join("state"));
        let worker_home = home.clone();
        let worker_root = root.clone();
        let (reached_tx, reached_rx) = mpsc::channel();
        let (resume_tx, resume_rx) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            let mut embedder = PauseDuringEmbedding {
                inner: FakeEmbedder::new(),
                reached: reached_tx,
                resume: resume_rx,
            };
            synchronize(
                &worker_home,
                &worker_root,
                &scan,
                &mut embedder,
                SyncMode::Incremental,
            )
        });

        reached_rx.recv_timeout(Duration::from_secs(2)).unwrap();
        let unrelated = VectorMutationLease::acquire(&home).unwrap();
        resume_tx.send(()).unwrap();
        drop(unrelated);

        worker.join().unwrap().unwrap();
        assert_eq!(state(&home, &root).generation, 1);
    }

    #[test]
    fn concurrent_first_syncs_publish_once_without_dangling_vectors_or_deadlock() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("orrery");
        let corpus_path = root.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        std::fs::write(corpus_path.join("alpha.md"), "alpha original").unwrap();
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let scan = scan_corpora(&[corpus]);
        let home = StoreHome::new(directory.path().join("state"));
        let barrier = Arc::new(Barrier::new(2));

        let handles = (0..2)
            .map(|_| {
                let thread_home = home.clone();
                let thread_root = root.clone();
                let thread_scan = scan.clone();
                let thread_barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    synchronize_with_hooks(
                        &thread_home,
                        &thread_root,
                        &thread_scan,
                        &mut FakeEmbedder::new(),
                        SyncMode::Incremental,
                        &mut WaitAfterProjectSnapshot {
                            barrier: thread_barrier,
                        },
                    )
                })
            })
            .collect::<Vec<_>>();
        let outcomes = handles
            .into_iter()
            .map(|handle| handle.join().unwrap())
            .collect::<Vec<_>>();

        let (published, refused): (Vec<_>, Vec<_>) =
            outcomes.iter().partition(|outcome| outcome.is_ok());
        assert_eq!(published.len(), 1, "{outcomes:?}");
        assert_eq!(refused.len(), 1, "{outcomes:?}");
        // Which refusal the loser earns depends only on how far it had walked
        // when the publisher took the vector mutation lease, so this asserts
        // the refusal class instead of a scheduling order the race
        // deliberately never imposes. Past its own project and vector opens
        // the loser parks on the lease and the project preconditions refuse
        // it; short of them the conservative openers refuse the publisher's
        // live rollback journal or WAL as transient contention. Every member
        // of the class publishes nothing, which is what exactly-once owes.
        // `stale_concurrent_first_sync_is_refused_by_the_project_generation_guard`
        // pins the generation guard itself without depending on scheduling.
        let error = refused[0].as_ref().unwrap_err();
        assert!(refuses_a_stale_or_contended_sync(error), "{outcomes:?}");
        assert_eq!(state(&home, &root).generation, 1);
        let referenced = project_vector_keys(&home, &root);
        assert!(
            !referenced.is_empty(),
            "the published sync must reference vectors, or the dangling check is vacuous"
        );
        let cached = VectorCache::open(&home)
            .unwrap()
            .get_many(&referenced)
            .unwrap();
        assert_eq!(cached.len(), referenced.len());
    }

    #[test]
    fn stale_concurrent_first_sync_is_refused_by_the_project_generation_guard() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("orrery");
        let corpus_path = root.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        std::fs::write(corpus_path.join("alpha.md"), "alpha original").unwrap();
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let scan = scan_corpora(&[corpus]);
        let home = StoreHome::new(directory.path().join("state"));
        let barrier = Arc::new(Barrier::new(2));
        let (published_sender, published_receiver) = mpsc::channel();

        let stale_home = home.clone();
        let stale_root = root.clone();
        let stale_scan = scan.clone();
        let stale_barrier = Arc::clone(&barrier);
        let stale = std::thread::spawn(move || {
            synchronize_with_hooks(
                &stale_home,
                &stale_root,
                &stale_scan,
                &mut FakeEmbedder::new(),
                SyncMode::Incremental,
                &mut WaitForPublishedPeer {
                    barrier: stale_barrier,
                    published: published_receiver,
                },
            )
        });

        // The barrier proves both racers captured the same empty generation-0
        // project before either published. The stale racer then holds that
        // snapshot, owning no connection or lease, until publication commits,
        // so the refusal below is the generation guard and never a conservative
        // opener refusing a live sidecar.
        let published = synchronize_with_hooks(
            &home,
            &root,
            &scan,
            &mut FakeEmbedder::new(),
            SyncMode::Incremental,
            &mut WaitAfterProjectSnapshot { barrier },
        )
        .unwrap();
        assert_eq!(published.generation, 1);
        published_sender.send(()).unwrap();

        let error = stale.join().unwrap().unwrap_err();
        assert!(
            matches!(
                error,
                SyncError::ProjectChanged {
                    expected: 0,
                    actual: 1
                }
            ),
            "{error:?}"
        );
        assert_eq!(state(&home, &root).generation, 1);
        let referenced = project_vector_keys(&home, &root);
        assert!(
            !referenced.is_empty(),
            "the published sync must reference vectors, or the dangling check is vacuous"
        );
        let cached = VectorCache::open(&home)
            .unwrap()
            .get_many(&referenced)
            .unwrap();
        assert_eq!(cached.len(), referenced.len());
    }

    fn assert_ancestor_replacement_is_refused(
        replacement: AncestorReplacement,
        at_final_check: bool,
    ) {
        let directory = tempdir().unwrap();
        let root = directory.path().join("orrery");
        let corpus_path = root.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        std::fs::write(corpus_path.join("alpha.md"), "alpha original").unwrap();
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let scan = scan_corpora(&[corpus]);
        let home = StoreHome::new(directory.path().join("state"));
        let before = state(&home, &root);
        let backup = root.join(".agents/memory-pinned-backup");
        let error = if at_final_check {
            synchronize_with_hooks(
                &home,
                &root,
                &scan,
                &mut FakeEmbedder::new(),
                SyncMode::Incremental,
                &mut ReplaceAncestorBeforeCommit {
                    path: corpus_path,
                    backup,
                    replacement,
                },
            )
        } else {
            synchronize_with_hooks(
                &home,
                &root,
                &scan,
                &mut FakeEmbedder::new(),
                SyncMode::Incremental,
                &mut ReplaceAncestorAfterVectors {
                    path: corpus_path,
                    backup,
                    replacement,
                },
            )
        }
        .unwrap_err();

        assert!(matches!(error, SyncError::SourceChanged(_)));
        assert_eq!(state(&home, &root), before);
    }

    #[test]
    fn nested_directory_replacement_is_refused_at_both_revision_checks() {
        assert_ancestor_replacement_is_refused(AncestorReplacement::Directory, false);
        assert_ancestor_replacement_is_refused(AncestorReplacement::Directory, true);
    }

    #[test]
    fn ancestor_symlink_replacement_is_refused_at_both_revision_checks() {
        assert_ancestor_replacement_is_refused(AncestorReplacement::Symlink, false);
        assert_ancestor_replacement_is_refused(AncestorReplacement::Symlink, true);
    }

    /// True for every refusal a losing racer can legitimately earn against a
    /// peer that is publishing the same generation.
    ///
    /// The project preconditions report the peer's advanced generation, or its
    /// stamped embedding scheme when the peer commits between those two reads,
    /// and the conservative openers report the peer's live SQLite artifacts as
    /// transient contention. None of them publishes project rows.
    fn refuses_a_stale_or_contended_sync(error: &SyncError) -> bool {
        matches!(
            error,
            SyncError::ProjectChanged { .. } | SyncError::ProjectSchemeChanged { .. }
        ) || error.is_transient_contention()
    }

    fn project_vector_keys(home: &StoreHome, root: &Path) -> BTreeSet<VectorKey> {
        let connection = open_project(home, root).unwrap();
        let mut statement = connection
            .prepare("SELECT vector_hash FROM chunks ORDER BY vector_hash")
            .unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|row| row.unwrap().parse().unwrap())
            .collect()
    }

    fn assert_gc_cannot_enter_vector_to_project_window(reuse_cached_vector: bool) {
        let directory = tempdir().unwrap();
        let root = directory.path().join("orrery");
        let corpus_path = root.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        let page_path = corpus_path.join("alpha.md");
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let home = StoreHome::new(directory.path().join("state"));
        if reuse_cached_vector {
            let mut embedder = FakeEmbedder::new();
            std::fs::write(&page_path, "alpha original").unwrap();
            synchronize(
                &home,
                &root,
                &scan_corpora(std::slice::from_ref(&corpus)),
                &mut embedder,
                SyncMode::Incremental,
            )
            .unwrap();
            std::fs::write(&page_path, "beta replacement").unwrap();
            synchronize(
                &home,
                &root,
                &scan_corpora(std::slice::from_ref(&corpus)),
                &mut embedder,
                SyncMode::Incremental,
            )
            .unwrap();
            std::fs::write(&page_path, "alpha original").unwrap();
        } else {
            std::fs::write(&page_path, "alpha original").unwrap();
        }
        let scan = scan_corpora(&[corpus]);
        let (reached_sender, reached_receiver) = mpsc::channel();
        let (resume_sender, resume_receiver) = mpsc::channel();
        let sync_home = home.clone();
        let sync_root = root.clone();
        let synchronizer = std::thread::spawn(move || {
            synchronize_with_hooks(
                &sync_home,
                &sync_root,
                &scan,
                &mut FakeEmbedder::new(),
                SyncMode::Incremental,
                &mut PauseAfterVectorVerification {
                    reached: reached_sender,
                    resume: resume_receiver,
                },
            )
        });
        reached_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let (gc_started_sender, gc_started_receiver) = mpsc::channel();
        let (gc_acquired_sender, gc_acquired_receiver) = mpsc::channel();
        let gc_home = home.clone();
        let gc_root = root.clone();
        let collector = std::thread::spawn(move || {
            gc_started_sender.send(()).unwrap();
            let lease = VectorMutationLease::acquire(&gc_home).unwrap();
            gc_acquired_sender.send(()).unwrap();
            let referenced = project_vector_keys(&gc_home, &gc_root);
            let mut cache = VectorCache::open(&gc_home).unwrap();
            cache.sweep_except(&lease, &referenced).unwrap()
        });
        gc_started_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let acquired_while_sync_paused = gc_acquired_receiver
            .recv_timeout(Duration::from_millis(100))
            .is_ok();
        resume_sender.send(()).unwrap();
        synchronizer.join().unwrap().unwrap();
        if !acquired_while_sync_paused {
            gc_acquired_receiver
                .recv_timeout(Duration::from_secs(2))
                .unwrap();
        }
        collector.join().unwrap();

        assert!(
            !acquired_while_sync_paused,
            "GC entered after vector verification but before the project commit"
        );
        let referenced = project_vector_keys(&home, &root);
        let cached = VectorCache::open(&home)
            .unwrap()
            .get_many(&referenced)
            .unwrap();
        assert_eq!(cached.len(), referenced.len());
    }

    #[test]
    fn new_vector_is_leased_through_the_project_commit() {
        assert_gc_cannot_enter_vector_to_project_window(false);
    }

    #[test]
    fn reusable_vector_is_leased_through_the_project_commit() {
        assert_gc_cannot_enter_vector_to_project_window(true);
    }

    // Characterization only: this lease-order guard also passed before the gc epoch fix.
    #[test]
    fn paused_first_sync_blocks_gc_lease_until_vectors_are_published() {
        let directory = tempdir().unwrap();
        let root = directory.path().join("orrery");
        let corpus_path = root.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        std::fs::write(corpus_path.join("alpha.md"), "alpha original").unwrap();
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let home = StoreHome::new(directory.path().join("state"));
        let scan = scan_corpora(&[corpus]);
        let (reached_sender, reached_receiver) = mpsc::channel();
        let (resume_sender, resume_receiver) = mpsc::channel();
        let sync_home = home.clone();
        let sync_root = root.clone();
        let synchronizer = std::thread::spawn(move || {
            synchronize_with_hooks(
                &sync_home,
                &sync_root,
                &scan,
                &mut FakeEmbedder::new(),
                SyncMode::Incremental,
                &mut PauseAfterVectorVerification {
                    reached: reached_sender,
                    resume: resume_receiver,
                },
            )
        });
        reached_receiver
            .recv_timeout(Duration::from_secs(2))
            .unwrap();

        let contention = VectorMutationLease::acquire_without_waiting(&home).unwrap_err();
        assert!(
            matches!(
                contention,
                VectorError::Store(StoreError::Busy {
                    operation: "coordinate vector references and garbage collection",
                    ..
                })
            ),
            "the zero-timeout probe must observe the synchronizer's held lease"
        );

        resume_sender.send(()).unwrap();
        synchronizer.join().unwrap().unwrap();
        let report = crate::gc(&home).unwrap();

        let referenced = project_vector_keys(&home, &root);
        assert!(
            !referenced.is_empty(),
            "the published sync must reference vectors, or the sweep is vacuous"
        );
        let cached = VectorCache::open(&home)
            .unwrap()
            .get_many(&referenced)
            .unwrap();
        assert_eq!(cached.len(), referenced.len());
        assert_eq!(report.removed, 0);
    }
}
