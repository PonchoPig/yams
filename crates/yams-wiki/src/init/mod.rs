use std::path::PathBuf;

mod apply;
mod assets;
mod inspect;
mod model;
mod plan;
mod policy;

pub use apply::{ApplyExitClass, ApplyOutcome, apply_manifest, apply_manifest_classified};
pub use assets::{
    AGENT_POLICY, INDEX_TEMPLATE, LAYOUT_VERSION, MEMORY_GITIGNORE, PAGE_TEMPLATE, SCHEMA, sha256,
};
pub use inspect::inspect_repository;
pub use model::{
    ApplyResult, InitConflict, InitInspection, InitManifest, InitMode, InitOperation,
    InitPlanRequest, LayoutClass, ManifestEnvelope, NodeKind, NodePrestate, OperationKind,
    ProjectPageRequest,
};
pub use plan::{canonical_manifest_bytes, plan_repository, plan_request_from_inspection};
pub use policy::{PolicyInspection, inspect_policy};

#[derive(Debug, thiserror::Error)]
pub enum InitError {
    #[error("could not {operation} {path}: {source}")]
    Io {
        operation: &'static str,
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("Git inspection failed: {0}")]
    Git(String),
    #[error("invalid repository root: {0}")]
    InvalidRoot(String),
    #[error("invalid initialization request: {0}")]
    InvalidRequest(String),
    #[error("repository layout conflict: {0}")]
    Conflict(String),
    #[error("approved repository state drifted: {0}")]
    Drift(String),
    #[error("candidate validation failed: {0}")]
    Candidate(String),
    #[error("manifest apply failed: {0}")]
    Apply(String),
    #[error("invalid initialization JSON: {0}")]
    Json(#[from] serde_json::Error),
}
