use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::fd::OwnedFd;
use std::os::unix::ffi::OsStringExt;
use std::path::{Component, Path, PathBuf};

use fastembed::{
    EmbeddingModel, InitOptionsUserDefined, Pooling, QuantizationMode, TextEmbedding,
    TokenizerFiles, UserDefinedEmbeddingModel,
};
use hf_hub::api::sync::ApiBuilder;
use hf_hub::{Repo, RepoType};
use rustix::fs::{self as rfs, AtFlags, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;
use rustix::process::geteuid;
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{ConstructionLease, ConstructionLockError, Embedder, Embedding, EmbeddingError};

/// Hugging Face model identifier selected by the retrieval evaluation.
pub const JINA_MODEL_ID: &str = "jinaai/jina-embeddings-v2-base-en";
/// The immutable upstream snapshot this build is allowed to download and load.
///
/// Provenance is recorded in `docs/release/jina-reference.md`: on 2026-08-11
/// this commit was confirmed equal to upstream `main` of [`JINA_MODEL_ID`]
/// through the Hugging Face API, and every artifact's local bytes were
/// verified against upstream metadata. Downloads are revision-qualified with
/// this value, so a moving upstream `main` can never change which weights a
/// new install receives, and the offline loader resolves this snapshot
/// directly instead of trusting a mutable `refs` entry inside the cache.
pub const JINA_REVISION: &str = "322d4d7e2f35e84137961a65af894fda0385eb7a";
/// SHA-256 over the artifact bytes of [`JINA_REVISION`], in the domain and
/// order the private `artifacts_digest` helper defines.
///
/// Release-owned, established with [`JINA_REVISION`] and mirrored by
/// `scripts/release-reference.env`; the
/// `pinned_provenance_matches_the_release_reference` test keeps the two in
/// sync. Every constructed model is checked against this value and fails
/// closed on any difference.
pub const JINA_ARTIFACTS_SHA256: &str =
    "3feec2cc49819ff4af53f2cc895902915a2dfef0f1130adf01667a30c38a6890";
/// Exact output dimension of the selected model.
pub const JINA_DIMENSIONS: usize = 768;
/// Tokenizer truncation bound used by both constructors.
pub const JINA_MAX_LENGTH: usize = 8192;
/// Every output-affecting runtime boundary, assembled in one testable place.
///
/// The executing fastembed/ort/ONNX Runtime stack and target platform can
/// change model output even when every explicit setting stays the same, so
/// each component is bound into the signature: any change here moves
/// constructed embedders into a new vector namespace via
/// [`signature_settings`] and the existing safe-rebuild path.
///
/// The version fields are hand-maintained literals because Cargo exposes no
/// compile-time macro for a dependency's resolved version; the
/// `pinned_runtime_versions_match_the_lockfile` test guards `fastembed` and
/// `ort_crate` from drifting out of sync with `Cargo.lock` instead.
#[derive(Clone, Debug)]
pub(crate) struct RuntimeIdentity {
    /// Pinned literal, enforced against `Cargo.lock` by test.
    pub fastembed: &'static str,
    /// Pinned literal, enforced against `Cargo.lock` by test.
    pub ort_crate: &'static str,
    /// Pinned literal, not enforced against any lockfile.
    pub onnx_runtime: &'static str,
    /// Derived from the compilation target, not a hand-maintained literal.
    pub target_os: &'static str,
    /// Derived from the compilation target, not a hand-maintained literal.
    pub target_arch: &'static str,
    /// Fixed by design: the only provider Yams configures.
    pub execution_provider: &'static str,
}

impl RuntimeIdentity {
    pub fn pinned() -> Self {
        Self {
            fastembed: "5.17.4",      // enforced against Cargo.lock by test
            ort_crate: "2.0.0-rc.13", // enforced against Cargo.lock by test
            // NOT enforced by pinned_runtime_versions_match_the_lockfile — Cargo.lock has no
            // linked-library version to check against. When bumping ort_crate, look up the ONNX
            // Runtime version it vendors and update this literal by hand.
            onnx_runtime: "1.28.0",
            // derived from the compilation target, not a hand-maintained literal
            target_os: std::env::consts::OS,
            // derived from the compilation target, not a hand-maintained literal
            target_arch: std::env::consts::ARCH,
            execution_provider: "cpu", // the only provider Yams configures
        }
    }
}

/// Stable signature prefix for every non-artifact output-affecting setting.
///
/// Each constructed instance appends its immutable snapshot revision and a
/// digest of the exact artifact bytes loaded into ONNX Runtime.
pub(crate) fn signature_settings(identity: &RuntimeIdentity) -> String {
    format!(
        "jinaai/jina-embeddings-v2-base-en|fastembed={}|\
         dimensions=768|pooling=mean|quantization=none|max_length=8192|\
         query_prefix=|passage_prefix=|intra_threads=1|\
         ort={}|onnxruntime={}|target={}-{}|ep={}",
        identity.fastembed,
        identity.ort_crate,
        identity.onnx_runtime,
        identity.target_os,
        identity.target_arch,
        identity.execution_provider,
    )
}

const MODEL_REPOSITORY_DIR: &str = "models--jinaai--jina-embeddings-v2-base-en";
const REQUIRED_ARTIFACTS: [&str; 5] = [
    "model.onnx",
    "tokenizer.json",
    "config.json",
    "special_tokens_map.json",
    "tokenizer_config.json",
];
const MODEL_BYTES_MAX: usize = 1024 * 1024 * 1024;
const TOKENIZER_BYTES_MAX: usize = 64 * 1024 * 1024;
const CONFIG_BYTES_MAX: usize = 4 * 1024 * 1024;
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK)
    .union(OFlags::DIRECTORY);
const FILE_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::NONBLOCK);

/// A 768-dimensional Jina v2 base-English adapter with validated output.
pub struct JinaEmbedder {
    model: Box<dyn Backend>,
    signature: String,
}

impl JinaEmbedder {
    /// Constructs exclusively from explicitly resolved local cache artifacts.
    ///
    /// This path never calls fastembed's network-capable constructor.
    pub fn offline(
        model_cache: impl AsRef<Path>,
        lock_dir: impl AsRef<Path>,
    ) -> Result<Self, JinaError> {
        let model_cache = model_cache.as_ref();
        let built = with_construction_lease(lock_dir.as_ref(), || build_offline(model_cache))?;
        Ok(Self {
            model: Box::new(built.model),
            signature: built.signature,
        })
    }

    /// Explicitly permits fastembed to populate and load the supplied cache.
    pub fn online(
        model_cache: impl AsRef<Path>,
        lock_dir: impl AsRef<Path>,
    ) -> Result<Self, JinaError> {
        let model_cache = model_cache.as_ref();
        let built = with_construction_lease(lock_dir.as_ref(), || build_online(model_cache))?;
        Ok(Self {
            model: Box::new(built.model),
            signature: built.signature,
        })
    }

    #[cfg(test)]
    fn from_backend(backend: impl Backend + 'static) -> Self {
        Self {
            model: Box::new(backend),
            signature: "injected-jina-backend".to_owned(),
        }
    }

    fn embed(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let vectors = self.model.embed(texts).map_err(EmbeddingError::Backend)?;
        validate_vectors(texts.len(), vectors)
    }
}

trait Backend: Send {
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String>;
}

impl Backend for TextEmbedding {
    fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        TextEmbedding::embed(self, texts, None).map_err(|error| error.to_string())
    }
}

impl Embedder for JinaEmbedder {
    fn signature(&self) -> &str {
        &self.signature
    }

    fn dimensions(&self) -> usize {
        JINA_DIMENSIONS
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        self.embed(texts)
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        let mut vectors = self.embed(&[text.to_owned()])?;
        vectors.pop().ok_or(EmbeddingError::CardinalityMismatch {
            expected: 1,
            actual: 0,
        })
    }
}

/// Typed model-cache, construction-lock, metadata, and ONNX construction failures.
#[derive(Debug, Error)]
pub enum JinaError {
    #[error(transparent)]
    ConstructionLock(#[from] ConstructionLockError),

    #[error(
        "cached Jina artifact {artifact} is missing under {cache_dir}; network is off; retry with YAMS_ALLOW_NET=1 to populate the model cache"
    )]
    MissingOfflineArtifact {
        artifact: &'static str,
        cache_dir: PathBuf,
    },

    #[error("could not read cached Jina artifact {artifact} at {path}: {source}")]
    ArtifactRead {
        artifact: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error(
        "the Jina model cache under {cache_dir} holds no snapshot {revision}, the revision this build of Yams is pinned to; an earlier release may have cached a superseded snapshot; run `YAMS_ALLOW_NET=1 yams --index` to download the pinned one (Yams never deletes what it cached: superseded snapshots are directories of symlinks, and the artifact bytes they point at live in the shared blobs directory)"
    )]
    PinnedSnapshotMissing {
        revision: String,
        cache_dir: PathBuf,
    },

    #[error(
        "the Jina download completed but left no snapshot {revision} under {cache_dir}: the endpoint served a different commit than the revision Yams requested; this is a provenance failure, not a transient one, so retrying and working around it are both wrong; verify the integrity of the network path and the Hugging Face endpoint in use, and report this"
    )]
    PinnedSnapshotNotServed {
        revision: String,
        cache_dir: PathBuf,
    },

    #[error(
        "cached Jina artifacts in {snapshot_dir} are not the release-verified bytes of pinned snapshot {revision}: expected sha256 {expected}, computed {actual}; Yams will not load unverified model weights; delete {snapshot_dir} and run `YAMS_ALLOW_NET=1 yams --index` to fetch the pinned snapshot again, and report a mismatch that survives a clean re-download rather than working around it"
    )]
    PinnedArtifactsMismatch {
        snapshot_dir: PathBuf,
        revision: String,
        expected: &'static str,
        actual: String,
    },

    #[error("could not construct the Jina embedding model: {0}")]
    ModelConstruction(String),

    #[error("could not download the Jina embedding model: {0}")]
    ModelDownload(String),

    #[error("fastembed's Jina v2 metadata does not match Yams's frozen contract: {0}")]
    ModelMetadata(String),

    #[error("unsafe offline Jina cache at {path}: {reason}")]
    UnsafeOfflineCache { path: PathBuf, reason: String },

    #[error("offline Jina cache binding changed at {path}")]
    OfflineCacheRebound { path: PathBuf },

    #[error("cached Jina artifact {artifact} at {path} exceeds the {maximum}-byte offline bound")]
    OfflineArtifactTooLarge {
        artifact: &'static str,
        path: PathBuf,
        maximum: usize,
    },
}

fn with_construction_lease<T>(
    lock_dir: &Path,
    construct: impl FnOnce() -> Result<T, JinaError>,
) -> Result<T, JinaError> {
    let lease = ConstructionLease::acquire(lock_dir)?;
    let result = construct();
    let revalidated = lease.revalidate();
    drop(lease);
    revalidated?;
    result
}

struct BuiltJina {
    model: TextEmbedding,
    signature: String,
}

/// Loads the pinned snapshot from `model_cache`, refusing to hand any bytes to
/// ONNX Runtime until they are proven to be the release-verified artifacts.
///
/// Both public constructors funnel through here, so the provenance check
/// covers the freshly downloaded cache and the long-lived one alike. Nothing
/// extra is read to perform it: resolution already hashes every artifact byte
/// to derive the vector-namespace signature.
///
/// Because the gate precedes ONNX construction, no cheap fixture can reach
/// [`JinaError::ModelConstruction`] any more: bytes that satisfy the digest
/// are the real model. That path is now only reachable from a genuine runtime
/// failure, and is left untested by design rather than by omission.
fn build_offline(model_cache: &Path) -> Result<BuiltJina, JinaError> {
    let metadata = selected_metadata()?;
    let mut snapshot = resolve_offline_snapshot(model_cache, JINA_REVISION)?;
    snapshot.verify_pinned_provenance()?;
    let signature = snapshot.signature().to_owned();
    let tokenizer_files = TokenizerFiles {
        tokenizer_file: snapshot.take_artifact("tokenizer.json")?,
        config_file: snapshot.take_artifact("config.json")?,
        special_tokens_map_file: snapshot.take_artifact("special_tokens_map.json")?,
        tokenizer_config_file: snapshot.take_artifact("tokenizer_config.json")?,
    };
    let model =
        UserDefinedEmbeddingModel::new(snapshot.take_artifact("model.onnx")?, tokenizer_files)
            .with_pooling(metadata.pooling)
            .with_quantization(metadata.quantization);
    let options = InitOptionsUserDefined::new()
        .with_max_length(JINA_MAX_LENGTH)
        .with_intra_threads(1);
    let model = TextEmbedding::try_new_from_user_defined(model, options);
    snapshot.revalidate()?;
    let model = model.map_err(|error| JinaError::ModelConstruction(error.to_string()))?;
    Ok(BuiltJina { model, signature })
}

fn build_online(model_cache: &Path) -> Result<BuiltJina, JinaError> {
    build_online_impl(ApiBuilder::new(), model_cache, None)
}

/// Shared online-construction path for [`build_online`] and the
/// endpoint-overriding test seam below.
///
/// `builder` supplies whatever cache — and therefore whatever ambient token,
/// if any — the caller started from; [`ApiBuilder::with_token`] is forced to
/// `None` immediately after regardless, so no caller, production or test,
/// can ever send a Hugging Face credential for this public repository.
fn build_online_impl(
    builder: ApiBuilder,
    model_cache: &Path,
    endpoint: Option<&str>,
) -> Result<BuiltJina, JinaError> {
    selected_metadata()?;
    // A cache that already holds the pinned snapshot needs no network at all.
    // Otherwise only the two absence failures are worth a download; everything
    // else describes a cache that fetching cannot repair — hf-hub keeps an
    // existing snapshot entry rather than replacing it — so those are reported
    // as they are instead of after a needless 550 MB fetch.
    match build_offline(model_cache) {
        Ok(built) => return Ok(built),
        Err(error) if !download_can_repair(&error) => return Err(error),
        Err(_) => {}
    }
    let mut builder = builder
        .with_cache_dir(model_cache.to_path_buf())
        // Jina v2 is public; Yams does not need or accept an ambient
        // Hugging Face credential.
        .with_token(None)
        .with_progress(false);
    if let Some(endpoint) = endpoint {
        builder = builder.with_endpoint(endpoint.to_owned());
    }
    let api = builder
        .build()
        .map_err(|error| JinaError::ModelDownload(error.to_string()))?;
    // Revision-qualified, never the repository default: `main` moves, and a
    // moved `main` would otherwise be accepted as the model silently.
    let repository = api.repo(Repo::with_revision(
        JINA_MODEL_ID.to_owned(),
        RepoType::Model,
        JINA_REVISION.to_owned(),
    ));
    for artifact in REQUIRED_ARTIFACTS {
        repository
            .get(artifact)
            .map_err(|error| JinaError::ModelDownload(error.to_string()))?;
    }
    build_offline(model_cache).map_err(|error| after_download(error, model_cache))
}

/// Whether downloading could plausibly turn `error` into a loadable cache.
///
/// Only absence qualifies. hf-hub keeps whatever snapshot entry already exists
/// rather than replacing it, so a cache that is present but hostile, rebound,
/// oversized, or off-digest stays exactly as it is across a fetch; treating
/// those as retryable would spend the download and then report the same fault.
fn download_can_repair(error: &JinaError) -> bool {
    matches!(
        error,
        JinaError::MissingOfflineArtifact { .. } | JinaError::PinnedSnapshotMissing { .. }
    )
}

/// Rewrites the one diagnosis whose remediation stops making sense once a
/// download has already run.
///
/// A download that reports success and still leaves no pinned snapshot means
/// the endpoint answered with a different commit than the one requested:
/// hf-hub names the snapshot directory from the server's `x-repo-commit`
/// header without checking it against the revision asked for. Telling that
/// operator to download again would loop forever, because the next run
/// short-circuits on the `refs/<revision>` pointer this one wrote.
fn after_download(error: JinaError, model_cache: &Path) -> JinaError {
    match error {
        JinaError::PinnedSnapshotMissing { revision, .. } => JinaError::PinnedSnapshotNotServed {
            revision,
            cache_dir: model_cache.to_path_buf(),
        },
        other => other,
    }
}

/// Test-only seam: exercises the exact request-construction and
/// artifact-download path `build_online` uses, against an explicit
/// endpoint and an explicit, test-owned ambient-token source, so tests can
/// prove no Hugging Face credential is ever sent without touching the real
/// network, the real `~/.cache`, or a real token.
#[cfg(any(test, feature = "test-support"))]
pub fn build_online_with_endpoint(model_cache: &Path, endpoint: &str) -> Result<(), JinaError> {
    let ambient = hf_hub::Cache::new(model_cache.to_path_buf());
    build_online_impl(ApiBuilder::from_cache(ambient), model_cache, Some(endpoint)).map(drop)
}

struct SelectedMetadata {
    pooling: Pooling,
    quantization: QuantizationMode,
}

fn selected_metadata() -> Result<SelectedMetadata, JinaError> {
    let model = EmbeddingModel::JinaEmbeddingsV2BaseEN;
    let info = TextEmbedding::get_model_info(&model)
        .map_err(|error| JinaError::ModelMetadata(error.to_string()))?;
    if info.model_code != JINA_MODEL_ID {
        return Err(JinaError::ModelMetadata(format!(
            "expected model ID {JINA_MODEL_ID}, got {}",
            info.model_code
        )));
    }
    if info.model_file != REQUIRED_ARTIFACTS[0] {
        return Err(JinaError::ModelMetadata(format!(
            "expected model file {}, got {}",
            REQUIRED_ARTIFACTS[0], info.model_file
        )));
    }
    if info.dim != JINA_DIMENSIONS {
        return Err(JinaError::ModelMetadata(format!(
            "expected {JINA_DIMENSIONS} dimensions, got {}",
            info.dim
        )));
    }
    if !info.additional_files.is_empty() {
        return Err(JinaError::ModelMetadata(format!(
            "expected no additional model files, got {:?}",
            info.additional_files
        )));
    }
    if info.output_key.is_some() {
        return Err(JinaError::ModelMetadata(
            "expected no selected output key".to_owned(),
        ));
    }
    let pooling = TextEmbedding::get_default_pooling_method(&model)
        .ok_or_else(|| JinaError::ModelMetadata("missing pooling metadata".to_owned()))?;
    if pooling != Pooling::Mean {
        return Err(JinaError::ModelMetadata(format!(
            "expected mean pooling, got {pooling:?}"
        )));
    }
    let quantization = TextEmbedding::get_quantization_mode(&model);
    if quantization != QuantizationMode::None {
        return Err(JinaError::ModelMetadata(format!(
            "expected no quantization, got {quantization:?}"
        )));
    }
    Ok(SelectedMetadata {
        pooling,
        quantization,
    })
}

trait ResolveHooks {
    fn after_artifact_opened(&mut self, _artifact: &'static str) {}
    fn after_artifact_read(&mut self, _artifact: &'static str) {}
}

struct SystemResolveHooks;

impl ResolveHooks for SystemResolveHooks {}

struct ResolvedSnapshot {
    cache: PinnedRoot,
    repository: PinnedDirectory,
    snapshots: PinnedDirectory,
    snapshot: PinnedDirectory,
    blobs: PinnedDirectory,
    artifacts: Vec<PinnedArtifact>,
    revision: String,
    artifacts_sha256: String,
    signature: String,
}

impl ResolvedSnapshot {
    fn signature(&self) -> &str {
        &self.signature
    }

    /// Fail-closed provenance gate: the resolved bytes must be exactly the
    /// release-verified artifacts of [`JINA_REVISION`].
    ///
    /// Resolution already selects the snapshot directory by its pinned name,
    /// so the revision is re-asserted only to keep the two pins from ever
    /// being reported inconsistently.
    fn verify_pinned_provenance(&self) -> Result<(), JinaError> {
        if self.revision == JINA_REVISION && self.artifacts_sha256 == JINA_ARTIFACTS_SHA256 {
            return Ok(());
        }
        Err(JinaError::PinnedArtifactsMismatch {
            snapshot_dir: self.snapshot.path.clone(),
            revision: JINA_REVISION.to_owned(),
            expected: JINA_ARTIFACTS_SHA256,
            actual: self.artifacts_sha256.clone(),
        })
    }

    fn take_artifact(&mut self, name: &'static str) -> Result<Vec<u8>, JinaError> {
        self.artifacts
            .iter_mut()
            .find(|artifact| artifact.name == name)
            .map(|artifact| std::mem::take(&mut artifact.bytes))
            .ok_or_else(|| {
                JinaError::ModelConstruction(format!("resolved artifact {name} missing"))
            })
    }

    fn revalidate(&self) -> Result<(), JinaError> {
        self.cache.revalidate()?;
        self.repository.revalidate(&self.cache.fd)?;
        self.snapshots.revalidate(&self.repository.fd)?;
        self.snapshot.revalidate(&self.snapshots.fd)?;
        self.blobs.revalidate(&self.repository.fd)?;
        for artifact in &self.artifacts {
            artifact.revalidate(&self.snapshot.fd, &self.blobs.fd)?;
        }
        Ok(())
    }
}

fn resolve_offline_snapshot(
    model_cache: &Path,
    revision: &str,
) -> Result<ResolvedSnapshot, JinaError> {
    let mut hooks = SystemResolveHooks;
    resolve_offline_snapshot_with_hooks(model_cache, revision, &mut hooks)
}

/// Fail-closed shape check on the one caller-supplied value that becomes a
/// path component.
///
/// Production only ever passes [`JINA_REVISION`], but the resolver hands this
/// string straight to `openat`, so the confinement it claims is enforced here
/// rather than assumed: nothing that is not one 40-character lowercase
/// hexadecimal commit can name a directory to open.
fn validate_revision(revision: &str, model_cache: &Path) -> Result<(), JinaError> {
    if revision.len() == 40
        && revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Ok(());
    }
    Err(unsafe_cache(
        model_cache.to_path_buf(),
        "a snapshot revision must be one 40-character lowercase hexadecimal commit",
    ))
}

/// Resolves exactly the requested snapshot out of a Hugging Face cache.
///
/// The snapshot directory is addressed by the caller's revision, never by the
/// cache's own mutable `refs` entries: which weights load is a property of
/// this build, not of a file any later download can rewrite.
fn resolve_offline_snapshot_with_hooks(
    model_cache: &Path,
    revision: &str,
    hooks: &mut impl ResolveHooks,
) -> Result<ResolvedSnapshot, JinaError> {
    validate_revision(revision, model_cache)?;
    let cache = PinnedRoot::open(model_cache)?;
    let repository = PinnedDirectory::open_child(
        &cache.fd,
        &cache.canonical_path,
        MODEL_REPOSITORY_DIR,
        MissingEntry::Unpopulated(model_cache),
    )?;
    let snapshots = PinnedDirectory::open_child(
        &repository.fd,
        &repository.path,
        "snapshots",
        MissingEntry::Unpopulated(model_cache),
    )?;
    let snapshot = PinnedDirectory::open_child(
        &snapshots.fd,
        &snapshots.path,
        revision,
        MissingEntry::PinnedSnapshot {
            cache_dir: model_cache,
            revision,
        },
    )?;
    let blobs = PinnedDirectory::open_child(
        &repository.fd,
        &repository.path,
        "blobs",
        MissingEntry::Unpopulated(model_cache),
    )?;

    let mut artifacts = Vec::with_capacity(REQUIRED_ARTIFACTS.len());
    for artifact in REQUIRED_ARTIFACTS {
        artifacts.push(read_artifact_from_snapshot(
            &snapshot,
            &blobs,
            artifact,
            model_cache,
            hooks,
        )?);
    }
    let artifacts_sha256 = artifacts_digest(&artifacts);
    let signature = resolved_signature(revision, &artifacts_sha256);
    let resolved = ResolvedSnapshot {
        cache,
        repository,
        snapshots,
        snapshot,
        blobs,
        artifacts,
        revision: revision.to_owned(),
        artifacts_sha256,
        signature,
    };
    resolved.revalidate()?;
    Ok(resolved)
}

fn read_artifact_from_snapshot(
    snapshot: &PinnedDirectory,
    blobs: &PinnedDirectory,
    artifact: &'static str,
    cache_dir: &Path,
    hooks: &mut impl ResolveHooks,
) -> Result<PinnedArtifact, JinaError> {
    let path = snapshot.path.join(artifact);
    let entry =
        rfs::statat(&snapshot.fd, artifact, AtFlags::SYMLINK_NOFOLLOW).map_err(|error| {
            if error == Errno::NOENT {
                missing_artifact(cache_dir, artifact)
            } else {
                unsafe_cache(
                    path.clone(),
                    format!("could not inspect snapshot entry: {error}"),
                )
            }
        })?;
    let kind = FileType::from_raw_mode(entry.st_mode);
    if kind.is_file() {
        let mut file = PinnedFile::open(
            &snapshot.fd,
            &snapshot.path,
            OsStr::new(artifact),
            artifact,
            Some(entry_state(&entry)),
            cache_dir,
        )?;
        hooks.after_artifact_opened(artifact);
        file.read_bounded(artifact_bound(artifact), artifact)?;
        file.revalidate(&snapshot.fd)?;
        hooks.after_artifact_read(artifact);
        let bytes = std::mem::take(&mut file.bytes);
        return Ok(PinnedArtifact {
            name: artifact,
            bytes,
            binding: ArtifactBinding::Direct(file),
        });
    }
    if kind.is_symlink() {
        validate_symlink(&path, &entry)?;
        let link_state = entry_state(&entry);
        let target = read_link(&snapshot.fd, artifact, &path)?;
        verify_symlink(&snapshot.fd, artifact, &path, link_state, &target)?;
        let blob_name = confined_blob_name(&target, &path)?;
        let mut blob = PinnedFile::open(
            &blobs.fd,
            &blobs.path,
            &blob_name,
            artifact,
            None,
            cache_dir,
        )?;
        hooks.after_artifact_opened(artifact);
        blob.read_bounded(artifact_bound(artifact), artifact)?;
        blob.revalidate(&blobs.fd)?;
        verify_symlink(&snapshot.fd, artifact, &path, link_state, &target)?;
        hooks.after_artifact_read(artifact);
        let bytes = std::mem::take(&mut blob.bytes);
        return Ok(PinnedArtifact {
            name: artifact,
            bytes,
            binding: ArtifactBinding::Symlink {
                link_state,
                link_target: target,
                link_path: path,
                blob,
            },
        });
    }
    Err(unsafe_cache(
        path,
        "snapshot entry is neither a regular file nor a confined Hugging Face blob symlink",
    ))
}

fn artifact_bound(artifact: &'static str) -> usize {
    match artifact {
        "model.onnx" => MODEL_BYTES_MAX,
        "tokenizer.json" => TOKENIZER_BYTES_MAX,
        "config.json" | "special_tokens_map.json" | "tokenizer_config.json" => CONFIG_BYTES_MAX,
        _ => 0,
    }
}

/// Digests every loaded artifact byte, length-prefixed and name-bound, under a
/// versioned domain.
///
/// This is the value [`JINA_ARTIFACTS_SHA256`] pins; changing the domain,
/// order, or framing invalidates that constant and the release reference with
/// it.
fn artifacts_digest(artifacts: &[PinnedArtifact]) -> String {
    let mut digest = Sha256::new();
    digest.update(b"yams-jina-artifacts-v1\0");
    for artifact in artifacts {
        digest.update((artifact.name.len() as u64).to_le_bytes());
        digest.update(artifact.name.as_bytes());
        digest.update((artifact.bytes.len() as u64).to_le_bytes());
        digest.update(&artifact.bytes);
    }
    lower_hex(&digest.finalize())
}

fn resolved_signature(revision: &str, artifacts_sha256: &str) -> String {
    format!(
        "{}|snapshot={revision}|artifacts_sha256={artifacts_sha256}",
        signature_settings(&RuntimeIdentity::pinned()),
    )
}

fn lower_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(char::from(HEX[usize::from(byte >> 4)]));
        output.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    output
}

struct PinnedRoot {
    fd: OwnedFd,
    state: EntryState,
    requested_path: PathBuf,
    canonical_path: PathBuf,
}

impl PinnedRoot {
    fn open(path: &Path) -> Result<Self, JinaError> {
        let requested_path = absolute_path(path)?;
        let canonical_path = match fs::canonicalize(&requested_path) {
            Ok(path) => path,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(missing_artifact(path, REQUIRED_ARTIFACTS[0]));
            }
            Err(error) => {
                return Err(unsafe_cache(
                    requested_path,
                    format!("model cache is unavailable: {error}"),
                ));
            }
        };
        let fd = rfs::open(&canonical_path, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
            unsafe_cache(
                canonical_path.clone(),
                format!("could not open model cache without following links: {error}"),
            )
        })?;
        let stat = rfs::fstat(&fd).map_err(|error| {
            unsafe_cache(
                canonical_path.clone(),
                format!("could not inspect model cache: {error}"),
            )
        })?;
        validate_directory(&canonical_path, &stat)?;
        let root = Self {
            fd,
            state: entry_state(&stat),
            requested_path,
            canonical_path,
        };
        root.revalidate()?;
        Ok(root)
    }

    fn revalidate(&self) -> Result<(), JinaError> {
        let resolved = fs::canonicalize(&self.requested_path)
            .map_err(|_| rebound(self.requested_path.clone()))?;
        if resolved != self.canonical_path {
            return Err(rebound(self.requested_path.clone()));
        }
        let stat = rfs::fstat(&self.fd).map_err(|error| {
            unsafe_cache(
                self.canonical_path.clone(),
                format!("could not reinspect model cache descriptor: {error}"),
            )
        })?;
        validate_directory(&self.canonical_path, &stat)?;
        if entry_state(&stat) != self.state {
            return Err(rebound(self.canonical_path.clone()));
        }
        let named = rfs::open(&self.canonical_path, DIRECTORY_FLAGS, Mode::empty())
            .map_err(|_| rebound(self.canonical_path.clone()))?;
        let named = rfs::fstat(&named).map_err(|_| rebound(self.canonical_path.clone()))?;
        if entry_state(&named) != self.state {
            return Err(rebound(self.canonical_path.clone()));
        }
        Ok(())
    }
}

struct PinnedDirectory {
    fd: OwnedFd,
    state: EntryState,
    path: PathBuf,
    name: OsString,
}

/// How an absent cache directory is diagnosed, so each level of the Hugging
/// Face layout reports the remediation that actually applies to it.
#[derive(Clone, Copy)]
enum MissingEntry<'a> {
    /// Nothing has populated this cache yet: the network-off remediation.
    Unpopulated(&'a Path),
    /// The cache exists but carries no snapshot for the pinned revision.
    PinnedSnapshot {
        cache_dir: &'a Path,
        revision: &'a str,
    },
}

impl MissingEntry<'_> {
    fn into_error(self) -> JinaError {
        match self {
            Self::Unpopulated(cache_dir) => missing_artifact(cache_dir, REQUIRED_ARTIFACTS[0]),
            Self::PinnedSnapshot {
                cache_dir,
                revision,
            } => JinaError::PinnedSnapshotMissing {
                revision: revision.to_owned(),
                cache_dir: cache_dir.to_path_buf(),
            },
        }
    }
}

impl PinnedDirectory {
    fn open_child(
        parent: &OwnedFd,
        parent_path: &Path,
        name: &str,
        missing: MissingEntry<'_>,
    ) -> Result<Self, JinaError> {
        let path = parent_path.join(name);
        let fd = rfs::openat(parent, name, DIRECTORY_FLAGS, Mode::empty()).map_err(|error| {
            if error == Errno::NOENT {
                missing.into_error()
            } else {
                unsafe_cache(
                    path.clone(),
                    format!("could not open cache directory without following links: {error}"),
                )
            }
        })?;
        let stat = rfs::fstat(&fd).map_err(|error| {
            unsafe_cache(
                path.clone(),
                format!("could not inspect cache directory: {error}"),
            )
        })?;
        validate_directory(&path, &stat)?;
        let directory = Self {
            fd,
            state: entry_state(&stat),
            path,
            name: name.into(),
        };
        directory.revalidate(parent)?;
        Ok(directory)
    }

    fn revalidate(&self, parent: &OwnedFd) -> Result<(), JinaError> {
        let opened = rfs::fstat(&self.fd).map_err(|error| {
            unsafe_cache(
                self.path.clone(),
                format!("could not reinspect cache directory descriptor: {error}"),
            )
        })?;
        validate_directory(&self.path, &opened)?;
        if entry_state(&opened) != self.state {
            return Err(rebound(self.path.clone()));
        }
        let named = rfs::statat(parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| rebound(self.path.clone()))?;
        validate_directory(&self.path, &named)?;
        if entry_state(&named) != self.state {
            return Err(rebound(self.path.clone()));
        }
        Ok(())
    }
}

struct PinnedFile {
    fd: OwnedFd,
    state: EntryState,
    path: PathBuf,
    name: OsString,
    bytes: Vec<u8>,
}

impl PinnedFile {
    fn open(
        parent: &OwnedFd,
        parent_path: &Path,
        name: &OsStr,
        diagnostic_artifact: &'static str,
        expected: Option<EntryState>,
        missing_cache: &Path,
    ) -> Result<Self, JinaError> {
        let path = parent_path.join(name);
        let fd = rfs::openat(parent, name, FILE_FLAGS, Mode::empty()).map_err(|error| {
            if error == Errno::NOENT {
                missing_artifact(missing_cache, diagnostic_artifact)
            } else {
                unsafe_cache(
                    path.clone(),
                    format!("could not open cache file without following links: {error}"),
                )
            }
        })?;
        let stat = rfs::fstat(&fd).map_err(|error| {
            unsafe_cache(
                path.clone(),
                format!("could not inspect cache file descriptor: {error}"),
            )
        })?;
        validate_regular(&path, &stat)?;
        let state = entry_state(&stat);
        if expected.is_some_and(|expected| expected != state) {
            return Err(rebound(path));
        }
        let file = Self {
            fd,
            state,
            path,
            name: name.to_owned(),
            bytes: Vec::new(),
        };
        file.revalidate(parent)?;
        Ok(file)
    }

    fn read_bounded(&mut self, maximum: usize, artifact: &'static str) -> Result<(), JinaError> {
        if self.state.size > maximum as u64 {
            return Err(JinaError::OfflineArtifactTooLarge {
                artifact,
                path: self.path.clone(),
                maximum,
            });
        }
        let mut bytes = Vec::with_capacity(self.state.size as usize);
        loop {
            let remaining = maximum.saturating_sub(bytes.len());
            let mut buffer = [0_u8; 64 * 1024];
            let limit = buffer.len().min(remaining.saturating_add(1));
            let read = match rustix::io::read(&self.fd, &mut buffer[..limit]) {
                Ok(read) => read,
                Err(Errno::INTR) => continue,
                Err(error) => {
                    return Err(JinaError::ArtifactRead {
                        artifact,
                        path: self.path.clone(),
                        source: std::io::Error::from_raw_os_error(error.raw_os_error()),
                    });
                }
            };
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
            if bytes.len() > maximum {
                return Err(JinaError::OfflineArtifactTooLarge {
                    artifact,
                    path: self.path.clone(),
                    maximum,
                });
            }
        }
        if bytes.is_empty() {
            return Err(unsafe_cache(self.path.clone(), "cache file is empty"));
        }
        self.bytes = bytes;
        Ok(())
    }

    fn revalidate(&self, parent: &OwnedFd) -> Result<(), JinaError> {
        let opened = rfs::fstat(&self.fd).map_err(|error| {
            unsafe_cache(
                self.path.clone(),
                format!("could not reinspect cache file descriptor: {error}"),
            )
        })?;
        validate_regular(&self.path, &opened)?;
        if entry_state(&opened) != self.state {
            return Err(rebound(self.path.clone()));
        }
        let named = rfs::statat(parent, &self.name, AtFlags::SYMLINK_NOFOLLOW)
            .map_err(|_| rebound(self.path.clone()))?;
        validate_regular(&self.path, &named)?;
        if entry_state(&named) != self.state {
            return Err(rebound(self.path.clone()));
        }
        Ok(())
    }
}

struct PinnedArtifact {
    name: &'static str,
    bytes: Vec<u8>,
    binding: ArtifactBinding,
}

enum ArtifactBinding {
    Direct(PinnedFile),
    Symlink {
        link_state: EntryState,
        link_target: OsString,
        link_path: PathBuf,
        blob: PinnedFile,
    },
}

impl PinnedArtifact {
    fn revalidate(&self, snapshot: &OwnedFd, blobs: &OwnedFd) -> Result<(), JinaError> {
        match &self.binding {
            ArtifactBinding::Direct(file) => file.revalidate(snapshot),
            ArtifactBinding::Symlink {
                link_state,
                link_target,
                link_path,
                blob,
            } => {
                verify_symlink(snapshot, self.name, link_path, *link_state, link_target)?;
                blob.revalidate(blobs)
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct EntryState {
    device: u64,
    inode: u64,
    mode: u32,
    owner: u32,
    links: u64,
    size: u64,
    modified_seconds: i64,
    modified_nanoseconds: i64,
    changed_seconds: i64,
    changed_nanoseconds: i64,
}

#[allow(clippy::unnecessary_cast)]
fn entry_state(stat: &Stat) -> EntryState {
    EntryState {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
        mode: stat.st_mode as u32,
        owner: stat.st_uid as u32,
        links: stat.st_nlink as u64,
        size: stat.st_size as u64,
        modified_seconds: stat.st_mtime as i64,
        modified_nanoseconds: stat.st_mtime_nsec as i64,
        changed_seconds: stat.st_ctime as i64,
        changed_nanoseconds: stat.st_ctime_nsec as i64,
    }
}

fn validate_directory(path: &Path, stat: &Stat) -> Result<(), JinaError> {
    if !FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Err(unsafe_cache(path.to_path_buf(), "path is not a directory"));
    }
    validate_owned_unwritable(path, stat)
}

fn validate_regular(path: &Path, stat: &Stat) -> Result<(), JinaError> {
    if !FileType::from_raw_mode(stat.st_mode).is_file() {
        return Err(unsafe_cache(
            path.to_path_buf(),
            "path is not a regular file",
        ));
    }
    if stat.st_nlink != 1 {
        return Err(unsafe_cache(
            path.to_path_buf(),
            format!(
                "file must have exactly one hard link, found {}",
                stat.st_nlink
            ),
        ));
    }
    if stat.st_size < 0 {
        return Err(unsafe_cache(path.to_path_buf(), "file has a negative size"));
    }
    validate_owned_unwritable(path, stat)
}

fn validate_symlink(path: &Path, stat: &Stat) -> Result<(), JinaError> {
    if !FileType::from_raw_mode(stat.st_mode).is_symlink() {
        return Err(unsafe_cache(path.to_path_buf(), "path is not a symlink"));
    }
    if stat.st_uid != geteuid().as_raw() {
        return Err(unsafe_cache(
            path.to_path_buf(),
            "symlink is not owned by the effective user",
        ));
    }
    if stat.st_nlink != 1 {
        return Err(unsafe_cache(
            path.to_path_buf(),
            format!(
                "symlink must have exactly one hard link, found {}",
                stat.st_nlink
            ),
        ));
    }
    Ok(())
}

fn validate_owned_unwritable(path: &Path, stat: &Stat) -> Result<(), JinaError> {
    if stat.st_uid != geteuid().as_raw() {
        return Err(unsafe_cache(
            path.to_path_buf(),
            "path is not owned by the effective user",
        ));
    }
    let mode = stat.st_mode as u32 & 0o7777;
    if mode & 0o022 != 0 {
        return Err(unsafe_cache(
            path.to_path_buf(),
            format!("mode {mode:04o} permits another user to write"),
        ));
    }
    Ok(())
}

fn read_link(parent: &OwnedFd, name: &str, path: &Path) -> Result<OsString, JinaError> {
    let target = rfs::readlinkat(parent, name, Vec::with_capacity(256)).map_err(|error| {
        unsafe_cache(
            path.to_path_buf(),
            format!("could not read snapshot symlink: {error}"),
        )
    })?;
    Ok(OsString::from_vec(target.to_bytes().to_vec()))
}

fn confined_blob_name(target: &OsStr, path: &Path) -> Result<OsString, JinaError> {
    let mut components = Path::new(target).components();
    let valid = matches!(components.next(), Some(Component::ParentDir))
        && matches!(components.next(), Some(Component::ParentDir))
        && matches!(components.next(), Some(Component::Normal(name)) if name == "blobs");
    let blob = match (valid, components.next(), components.next()) {
        (true, Some(Component::Normal(blob)), None) if !blob.is_empty() => blob.to_owned(),
        _ => {
            return Err(unsafe_cache(
                path.to_path_buf(),
                "snapshot symlink must target exactly ../../blobs/<blob>",
            ));
        }
    };
    Ok(blob)
}

fn verify_symlink(
    parent: &OwnedFd,
    name: &str,
    path: &Path,
    expected: EntryState,
    target: &OsStr,
) -> Result<(), JinaError> {
    let current = rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|_| rebound(path.to_path_buf()))?;
    validate_symlink(path, &current)?;
    if entry_state(&current) != expected {
        return Err(rebound(path.to_path_buf()));
    }
    if read_link(parent, name, path)? != target {
        return Err(rebound(path.to_path_buf()));
    }
    Ok(())
}

fn absolute_path(path: &Path) -> Result<PathBuf, JinaError> {
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    std::env::current_dir()
        .map(|current| current.join(path))
        .map_err(|error| {
            unsafe_cache(
                path.to_path_buf(),
                format!("could not resolve path: {error}"),
            )
        })
}

fn missing_artifact(cache_dir: &Path, artifact: &'static str) -> JinaError {
    JinaError::MissingOfflineArtifact {
        artifact,
        cache_dir: cache_dir.to_path_buf(),
    }
}

fn unsafe_cache(path: PathBuf, reason: impl Into<String>) -> JinaError {
    JinaError::UnsafeOfflineCache {
        path,
        reason: reason.into(),
    }
}

fn rebound(path: PathBuf) -> JinaError {
    JinaError::OfflineCacheRebound { path }
}

fn validate_vectors(
    expected_cardinality: usize,
    vectors: Vec<Vec<f32>>,
) -> Result<Vec<Embedding>, EmbeddingError> {
    if vectors.len() != expected_cardinality {
        return Err(EmbeddingError::CardinalityMismatch {
            expected: expected_cardinality,
            actual: vectors.len(),
        });
    }
    vectors
        .into_iter()
        .map(|vector| {
            if vector.len() != JINA_DIMENSIONS {
                return Err(EmbeddingError::DimensionMismatch {
                    expected: JINA_DIMENSIONS,
                    actual: vector.len(),
                });
            }
            Embedding::new(vector)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;
    use std::path::Path;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use super::*;

    #[test]
    fn every_runtime_component_change_produces_a_distinct_signature() {
        let base = RuntimeIdentity::pinned();
        let mut variants = vec![base.clone()];
        variants.push(RuntimeIdentity {
            fastembed: "9.9.9",
            ..base.clone()
        });
        variants.push(RuntimeIdentity {
            ort_crate: "9.9.9",
            ..base.clone()
        });
        variants.push(RuntimeIdentity {
            onnx_runtime: "9.9.9",
            ..base.clone()
        });
        variants.push(RuntimeIdentity {
            target_os: "other-os",
            ..base.clone()
        });
        variants.push(RuntimeIdentity {
            target_arch: "other-arch",
            ..base.clone()
        });
        variants.push(RuntimeIdentity {
            execution_provider: "other-ep",
            ..base.clone()
        });
        let signatures: std::collections::BTreeSet<_> =
            variants.iter().map(signature_settings).collect();
        assert_eq!(
            signatures.len(),
            variants.len(),
            "every component must alter the namespace"
        );
    }

    #[test]
    fn pinned_runtime_versions_match_the_lockfile() {
        let lock =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/../../Cargo.lock"))
                .unwrap();
        let pinned = RuntimeIdentity::pinned();
        for (name, version) in [("fastembed", pinned.fastembed), ("ort", pinned.ort_crate)] {
            assert!(
                lock.contains(&format!("name = \"{name}\"\nversion = \"{version}\"")),
                "{name} {version} drifted from Cargo.lock — update RuntimeIdentity::pinned()"
            );
        }
    }

    #[test]
    fn pinned_revision_is_one_lowercase_hexadecimal_commit() {
        assert_eq!(JINA_REVISION.len(), 40);
        assert_eq!(JINA_ARTIFACTS_SHA256.len(), 64);
        for pin in [JINA_REVISION, JINA_ARTIFACTS_SHA256] {
            assert!(
                pin.bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()),
                "{pin} must be lowercase hexadecimal, and must never name a path component"
            );
        }
    }

    #[test]
    fn pinned_provenance_matches_the_release_reference() {
        let pins = format!("snapshot={JINA_REVISION}|artifacts_sha256={JINA_ARTIFACTS_SHA256}");
        for relative in [
            "/../../scripts/release-reference.env",
            "/../../docs/release/jina-reference.md",
        ] {
            let path = format!("{}{relative}", env!("CARGO_MANIFEST_DIR"));
            let recorded = std::fs::read_to_string(&path).unwrap();
            assert!(
                recorded.contains(&pins),
                "{path} disagrees with the pins compiled into this build; re-establish both together"
            );
        }
    }

    #[test]
    fn signature_settings_freeze_every_output_affecting_component() {
        let expected = format!(
            "jinaai/jina-embeddings-v2-base-en|fastembed=5.17.4|dimensions=768|pooling=mean|quantization=none|max_length=8192|query_prefix=|passage_prefix=|intra_threads=1|ort=2.0.0-rc.13|onnxruntime=1.28.0|target={}-{}|ep=cpu",
            std::env::consts::OS,
            std::env::consts::ARCH,
        );
        assert_eq!(signature_settings(&RuntimeIdentity::pinned()), expected);
    }

    struct StubBackend {
        seen: Arc<Mutex<Vec<Vec<String>>>>,
        reply: Result<Vec<Vec<f32>>, String>,
    }

    impl Backend for StubBackend {
        fn embed(&mut self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            self.seen.lock().unwrap().push(texts.to_vec());
            self.reply.clone()
        }
    }

    fn vector(first: f32) -> Vec<f32> {
        let mut vector = vec![0.0; JINA_DIMENSIONS];
        vector[0] = first;
        vector
    }

    #[test]
    fn passage_texts_reach_fastembed_unchanged() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = StubBackend {
            seen: Arc::clone(&seen),
            reply: Ok(vec![vector(2.0), vector(3.0)]),
        };
        let mut embedder = JinaEmbedder::from_backend(backend);
        let passages = vec![
            "passage: stays literal".to_owned(),
            " query\ttext ".to_owned(),
        ];

        embedder.embed_passages(&passages).unwrap();

        assert_eq!(*seen.lock().unwrap(), vec![passages]);
    }

    #[test]
    fn query_text_reaches_fastembed_unchanged() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let backend = StubBackend {
            seen: Arc::clone(&seen),
            reply: Ok(vec![vector(2.0)]),
        };
        let mut embedder = JinaEmbedder::from_backend(backend);

        embedder.embed_query(" query: stays literal ").unwrap();

        assert_eq!(
            *seen.lock().unwrap(),
            vec![vec![" query: stays literal ".to_owned()]]
        );
    }

    #[test]
    fn backend_cardinality_and_dimensions_are_exactly_validated() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut cardinality = JinaEmbedder::from_backend(StubBackend {
            seen: Arc::clone(&seen),
            reply: Ok(vec![vector(1.0)]),
        });
        assert_eq!(
            cardinality.embed_passages(&["one".to_owned(), "two".to_owned()]),
            Err(EmbeddingError::CardinalityMismatch {
                expected: 2,
                actual: 1,
            })
        );

        let mut dimensions = JinaEmbedder::from_backend(StubBackend {
            seen,
            reply: Ok(vec![vec![1.0; JINA_DIMENSIONS - 1]]),
        });
        assert_eq!(
            dimensions.embed_query("query"),
            Err(EmbeddingError::DimensionMismatch {
                expected: JINA_DIMENSIONS,
                actual: JINA_DIMENSIONS - 1,
            })
        );
    }

    #[test]
    fn backend_vectors_are_normalized_and_backend_failures_remain_typed() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut normalized = JinaEmbedder::from_backend(StubBackend {
            seen: Arc::clone(&seen),
            reply: Ok(vec![vector(4.0)]),
        });
        assert_eq!(normalized.embed_query("query").unwrap().values()[0], 1.0);

        let mut failed = JinaEmbedder::from_backend(StubBackend {
            seen,
            reply: Err("injected inference failure".to_owned()),
        });
        assert_eq!(
            failed.embed_query("query"),
            Err(EmbeddingError::Backend(
                "injected inference failure".to_owned()
            ))
        );
    }

    #[test]
    fn construction_closure_runs_once_and_releases_before_return() {
        let lock_dir = tempfile::tempdir().unwrap();
        let calls = AtomicUsize::new(0);

        let value = with_construction_lease(lock_dir.path(), || {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok::<_, JinaError>(42)
        })
        .unwrap();

        assert_eq!(value, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        let _first = ConstructionLease::acquire(lock_dir.path()).unwrap();
        let _second = ConstructionLease::acquire(lock_dir.path()).unwrap();
    }

    #[test]
    fn construction_revalidates_the_slot_binding_before_release() {
        let lock_dir = tempfile::tempdir().unwrap();
        let slot = lock_dir.path().join(".yams-model-load-0.lock");
        let stale = lock_dir.path().join("stale.lock");

        let error = with_construction_lease(lock_dir.path(), || {
            fs::rename(&slot, &stale).unwrap();
            fs::File::create(&slot).unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&slot, fs::Permissions::from_mode(0o600)).unwrap();
            }
            Ok::<_, JinaError>(())
        })
        .unwrap_err();

        assert!(matches!(
            error,
            JinaError::ConstructionLock(ConstructionLockError::Rebound { .. })
        ));
    }

    #[test]
    fn selected_fastembed_metadata_matches_the_frozen_signature() {
        let metadata = selected_metadata().unwrap();

        assert_eq!(metadata.pooling, Pooling::Mean);
        assert_eq!(metadata.quantization, QuantizationMode::None);
    }

    const REVISION_A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const REVISION_B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    struct RebindArtifactAfterOpen {
        artifact_path: PathBuf,
        stale_path: PathBuf,
    }

    impl ResolveHooks for RebindArtifactAfterOpen {
        fn after_artifact_opened(&mut self, artifact: &'static str) {
            if artifact == "model.onnx" {
                fs::rename(&self.artifact_path, &self.stale_path).unwrap();
                fs::write(&self.artifact_path, b"replacement model").unwrap();
            }
        }
    }

    struct RebindSnapshotAfterFirstArtifact {
        snapshot_path: PathBuf,
        stale_path: PathBuf,
        switched: bool,
    }

    impl ResolveHooks for RebindSnapshotAfterFirstArtifact {
        fn after_artifact_read(&mut self, _artifact: &'static str) {
            if !self.switched {
                self.switched = true;
                fs::rename(&self.snapshot_path, &self.stale_path).unwrap();
                fs::create_dir(&self.snapshot_path).unwrap();
            }
        }
    }

    #[test]
    fn resolved_signature_binds_revision_and_every_artifact_byte() {
        let baseline_cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(baseline_cache.path(), REVISION_A, b"first model");
        let baseline = resolve_offline_snapshot(baseline_cache.path(), REVISION_A).unwrap();

        let other_revision_cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(other_revision_cache.path(), REVISION_B, b"first model");
        let other_revision =
            resolve_offline_snapshot(other_revision_cache.path(), REVISION_B).unwrap();
        assert_ne!(baseline.signature(), other_revision.signature());

        for artifact in REQUIRED_ARTIFACTS {
            let changed_cache = tempfile::tempdir().unwrap();
            write_direct_snapshot(changed_cache.path(), REVISION_A, b"first model");
            fs::write(
                snapshot_path(changed_cache.path(), REVISION_A).join(artifact),
                b"one different artifact",
            )
            .unwrap();
            let changed = resolve_offline_snapshot(changed_cache.path(), REVISION_A).unwrap();
            assert_ne!(baseline.signature(), changed.signature(), "{artifact}");
        }

        assert!(
            baseline
                .signature()
                .contains(&format!("snapshot={REVISION_A}"))
        );
        assert!(
            baseline
                .signature()
                .contains(&format!("artifacts_sha256={}", baseline.artifacts_sha256))
        );
    }

    #[test]
    fn only_the_requested_snapshot_loads_whatever_refs_say() {
        let cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(cache.path(), REVISION_A, b"requested model");
        write_snapshot_only(cache.path(), REVISION_B, b"other model");
        // A `refs` entry naming the other snapshot cannot redirect the load:
        // the revision is a property of the build, not of the cache.
        fs::write(
            cache.path().join(MODEL_REPOSITORY_DIR).join("refs/main"),
            REVISION_B,
        )
        .unwrap();

        let resolved = resolve_offline_snapshot(cache.path(), REVISION_A).unwrap();

        assert_eq!(resolved.revision, REVISION_A);
        assert_eq!(
            resolved
                .artifacts
                .iter()
                .find(|artifact| artifact.name == "model.onnx")
                .unwrap()
                .bytes,
            b"requested model"
        );
    }

    #[test]
    fn an_absent_requested_snapshot_names_it_and_the_remediation() {
        let cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(cache.path(), REVISION_A, b"superseded model");

        let error = resolve_offline_snapshot(cache.path(), REVISION_B)
            .err()
            .expect("a cache without the requested snapshot must fail closed");

        assert!(
            matches!(&error, JinaError::PinnedSnapshotMissing { revision, .. }
                if revision == REVISION_B),
            "{error:?}"
        );
        assert!(error.to_string().contains("YAMS_ALLOW_NET=1 yams --index"));
    }

    #[test]
    fn a_revision_that_is_not_a_commit_never_reaches_openat() {
        let cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(cache.path(), REVISION_A, b"model");

        for revision in [
            "..",
            "../../..",
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            "",
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/aaa",
        ] {
            let error = resolve_offline_snapshot(cache.path(), revision)
                .err()
                .unwrap_or_else(|| panic!("{revision:?} must fail closed"));

            assert!(
                matches!(error, JinaError::UnsafeOfflineCache { .. }),
                "{revision:?}: {error:?}"
            );
        }
    }

    #[test]
    fn only_absence_is_worth_a_download() {
        let repairable = [
            JinaError::MissingOfflineArtifact {
                artifact: "model.onnx",
                cache_dir: PathBuf::from("/cache"),
            },
            JinaError::PinnedSnapshotMissing {
                revision: REVISION_A.to_owned(),
                cache_dir: PathBuf::from("/cache"),
            },
        ];
        for error in &repairable {
            assert!(download_can_repair(error), "{error:?}");
        }

        let untouched_by_a_download = [
            JinaError::UnsafeOfflineCache {
                path: PathBuf::from("/cache"),
                reason: "hostile".to_owned(),
            },
            JinaError::OfflineCacheRebound {
                path: PathBuf::from("/cache"),
            },
            JinaError::OfflineArtifactTooLarge {
                artifact: "model.onnx",
                path: PathBuf::from("/cache"),
                maximum: MODEL_BYTES_MAX,
            },
            JinaError::PinnedArtifactsMismatch {
                snapshot_dir: PathBuf::from("/cache"),
                revision: JINA_REVISION.to_owned(),
                expected: JINA_ARTIFACTS_SHA256,
                actual: "beef".to_owned(),
            },
        ];
        for error in &untouched_by_a_download {
            assert!(!download_can_repair(error), "{error:?}");
        }
    }

    #[test]
    fn a_download_that_serves_another_commit_is_a_provenance_failure() {
        let cache = Path::new("/cache");

        let served = after_download(
            JinaError::PinnedSnapshotMissing {
                revision: JINA_REVISION.to_owned(),
                cache_dir: cache.to_path_buf(),
            },
            cache,
        );

        assert!(
            matches!(&served, JinaError::PinnedSnapshotNotServed { revision, .. }
                if revision == JINA_REVISION),
            "{served:?}"
        );
        let message = served.to_string();
        assert!(message.contains("served a different commit"), "{message}");
        assert!(!message.contains("yams --index"), "{message}");

        // Every other diagnosis already reads correctly after a download.
        let unchanged = after_download(
            JinaError::OfflineCacheRebound {
                path: cache.to_path_buf(),
            },
            cache,
        );
        assert!(matches!(unchanged, JinaError::OfflineCacheRebound { .. }));
    }

    #[test]
    fn hugging_face_blob_symlinks_are_confined_and_resolved() {
        let cache = tempfile::tempdir().unwrap();
        write_symlink_snapshot(cache.path(), REVISION_A);

        let snapshot = resolve_offline_snapshot(cache.path(), REVISION_A).unwrap();

        assert!(
            snapshot
                .signature()
                .contains(&format!("snapshot={REVISION_A}"))
        );
        assert_eq!(
            snapshot
                .artifacts
                .iter()
                .find(|artifact| artifact.name == "model.onnx")
                .unwrap()
                .bytes,
            b"model.onnx blob"
        );
    }

    #[test]
    fn escaping_snapshot_symlink_is_rejected_without_reading_its_target() {
        let cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(cache.path(), REVISION_A, b"model");
        let snapshot = snapshot_path(cache.path(), REVISION_A);
        let model = snapshot.join("model.onnx");
        let victim = cache.path().join("victim");
        fs::write(&victim, b"do not read through the cache").unwrap();
        fs::remove_file(&model).unwrap();
        symlink("../../../../victim", &model).unwrap();

        let error = resolve_offline_snapshot(cache.path(), REVISION_A)
            .err()
            .expect("an escaping artifact link must fail closed");

        assert!(matches!(error, JinaError::UnsafeOfflineCache { .. }));
        assert_eq!(fs::read(victim).unwrap(), b"do not read through the cache");
    }

    #[test]
    fn fifo_snapshot_and_artifact_are_rejected_without_blocking() {
        for entry in ["snapshot", "artifact"] {
            let cache = tempfile::tempdir().unwrap();
            write_direct_snapshot(cache.path(), REVISION_A, b"model");
            let snapshot = snapshot_path(cache.path(), REVISION_A);
            let path = if entry == "snapshot" {
                fs::remove_dir_all(&snapshot).unwrap();
                snapshot
            } else {
                let artifact = snapshot.join("model.onnx");
                fs::remove_file(&artifact).unwrap();
                artifact
            };
            assert!(
                Command::new("mkfifo")
                    .arg(&path)
                    .status()
                    .unwrap()
                    .success()
            );

            let started = Instant::now();
            let error = resolve_offline_snapshot(cache.path(), REVISION_A)
                .err()
                .expect("a FIFO cache entry must fail closed");

            assert!(
                matches!(error, JinaError::UnsafeOfflineCache { .. }),
                "{entry}: {error:?}"
            );
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    #[test]
    fn symlinked_snapshot_directory_is_rejected() {
        let cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(cache.path(), REVISION_A, b"model");
        let snapshot = snapshot_path(cache.path(), REVISION_A);
        let replacement = snapshot.with_file_name("replacement-snapshot");
        fs::rename(&snapshot, &replacement).unwrap();
        symlink(&replacement, &snapshot).unwrap();

        let error = resolve_offline_snapshot(cache.path(), REVISION_A)
            .err()
            .expect("the pinned snapshot must not be followed through a symlink");

        assert!(matches!(error, JinaError::UnsafeOfflineCache { .. }));
    }

    #[test]
    fn sparse_oversized_artifact_is_rejected_before_reading() {
        let cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(cache.path(), REVISION_A, b"model");
        let model = snapshot_path(cache.path(), REVISION_A).join("model.onnx");
        fs::File::options()
            .write(true)
            .open(&model)
            .unwrap()
            .set_len(MODEL_BYTES_MAX as u64 + 1)
            .unwrap();

        let error = resolve_offline_snapshot(cache.path(), REVISION_A)
            .err()
            .expect("a model above the hard byte bound must fail closed");

        assert!(matches!(
            error,
            JinaError::OfflineArtifactTooLarge {
                artifact: "model.onnx",
                maximum: MODEL_BYTES_MAX,
                ..
            }
        ));
    }

    #[test]
    fn hardlinked_artifact_is_rejected() {
        let cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(cache.path(), REVISION_A, b"model");
        let snapshot = snapshot_path(cache.path(), REVISION_A);
        fs::hard_link(
            snapshot.join("model.onnx"),
            cache.path().join("second-model-link"),
        )
        .unwrap();

        let error = resolve_offline_snapshot(cache.path(), REVISION_A)
            .err()
            .expect("a multiply linked artifact must fail closed");

        assert!(matches!(error, JinaError::UnsafeOfflineCache { .. }));
    }

    #[test]
    fn artifact_name_replacement_after_descriptor_open_is_rebound() {
        let cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(cache.path(), REVISION_A, b"model");
        let snapshot = snapshot_path(cache.path(), REVISION_A);
        let mut hooks = RebindArtifactAfterOpen {
            artifact_path: snapshot.join("model.onnx"),
            stale_path: snapshot.join("stale-model.onnx"),
        };

        let error = resolve_offline_snapshot_with_hooks(cache.path(), REVISION_A, &mut hooks)
            .err()
            .expect("artifact replacement must invalidate the pinned descriptor");

        assert!(matches!(error, JinaError::OfflineCacheRebound { .. }));
    }

    #[test]
    fn snapshot_directory_replacement_during_reads_is_rebound() {
        let cache = tempfile::tempdir().unwrap();
        write_direct_snapshot(cache.path(), REVISION_A, b"model");
        let snapshot = snapshot_path(cache.path(), REVISION_A);
        let mut hooks = RebindSnapshotAfterFirstArtifact {
            stale_path: snapshot.with_file_name("stale-snapshot"),
            snapshot_path: snapshot,
            switched: false,
        };

        let error = resolve_offline_snapshot_with_hooks(cache.path(), REVISION_A, &mut hooks)
            .err()
            .expect("snapshot replacement must invalidate the pinned directory");

        assert!(matches!(error, JinaError::OfflineCacheRebound { .. }));
    }

    fn write_direct_snapshot(cache: &Path, revision: &str, model: &[u8]) -> PathBuf {
        let repository = cache.join("models--jinaai--jina-embeddings-v2-base-en");
        fs::create_dir_all(repository.join("refs")).unwrap();
        fs::create_dir_all(repository.join("blobs")).unwrap();
        let ref_path = repository.join("refs/main");
        fs::write(&ref_path, revision).unwrap();
        write_snapshot_only(cache, revision, model);
        ref_path
    }

    fn write_snapshot_only(cache: &Path, revision: &str, model: &[u8]) {
        let snapshot = snapshot_path(cache, revision);
        fs::create_dir_all(&snapshot).unwrap();
        for (artifact, bytes) in [
            ("model.onnx", model),
            ("tokenizer.json", b"tokenizer".as_slice()),
            ("config.json", b"config".as_slice()),
            ("special_tokens_map.json", b"special".as_slice()),
            ("tokenizer_config.json", b"tokenizer config".as_slice()),
        ] {
            fs::write(snapshot.join(artifact), bytes).unwrap();
        }
    }

    fn snapshot_path(cache: &Path, revision: &str) -> PathBuf {
        cache
            .join(MODEL_REPOSITORY_DIR)
            .join("snapshots")
            .join(revision)
    }

    fn write_symlink_snapshot(cache: &Path, revision: &str) {
        let repository = cache.join(MODEL_REPOSITORY_DIR);
        let snapshot = repository.join("snapshots").join(revision);
        let blobs = repository.join("blobs");
        fs::create_dir_all(repository.join("refs")).unwrap();
        fs::create_dir_all(&snapshot).unwrap();
        fs::create_dir_all(&blobs).unwrap();
        fs::write(repository.join("refs/main"), revision).unwrap();
        for (index, artifact) in REQUIRED_ARTIFACTS.into_iter().enumerate() {
            let blob = format!("blob-{index}");
            fs::write(blobs.join(&blob), format!("{artifact} blob")).unwrap();
            symlink(format!("../../blobs/{blob}"), snapshot.join(artifact)).unwrap();
        }
    }
}
