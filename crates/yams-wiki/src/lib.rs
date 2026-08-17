//! Structural schema and mutation support for the shared memory wiki.

mod capabilities;
mod check;
mod durable;
mod index;
mod init;
mod lock;
mod schema;
mod write;

pub use capabilities::{Capabilities, CapabilityContracts, capabilities};
pub use check::{
    CapturedPage, CapturedPageOutcome, CheckError, WikiCheckReport, WikiCompatReport, WikiSnapshot,
    capture_wiki, check_wiki, compat_wiki, validate_wiki,
};
pub use durable::{
    CanonicalDigest, DurableError, ReindexOptions, ReindexResult, canonical_index_digest,
    reindex_wiki,
};
pub use index::{
    BEGIN_MARKER, END_MARKER, IndexCheck, IndexError, IndexPage, adopt_legacy, check_index,
    parse_index_page, rebuild_index,
};
pub use init::{
    AGENT_POLICY, ApplyExitClass, ApplyOutcome, ApplyResult, INDEX_TEMPLATE, InitConflict,
    InitError, InitInspection, InitManifest, InitMode, InitOperation, InitPlanRequest,
    LAYOUT_VERSION, LayoutClass, MEMORY_GITIGNORE, ManifestEnvelope, NodeKind, NodePrestate,
    OperationKind, PAGE_TEMPLATE, PolicyInspection, ProjectPageRequest, SCHEMA, apply_manifest,
    apply_manifest_classified, canonical_manifest_bytes, inspect_policy, inspect_repository,
    plan_repository, plan_request_from_inspection, sha256,
};
pub use lock::{
    LOCK_NAME, LOCK_TIMEOUT, LockError, LockGuard, LockLease, LockMode, Unisolated,
    UnisolatedReason, acquire_lock, acquire_lock_with_timeout,
};
pub use schema::{
    CreateRequest, Owner, PageType, ParsedWikiPage, RenderedUpdate, SchemaError, Status,
    UpdateRequest, parse_wiki_page, render_create, render_update, slugify,
};
pub use write::{WriteResult, write_json};
