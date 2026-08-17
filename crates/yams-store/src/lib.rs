mod home;
mod management;
mod project;
mod retrieve;
mod schema;
mod secure;
mod sync;
mod vector;

pub use home::StoreHome;
pub use management::{
    IndexInventory, ManagementError, ProjectInventory, ProjectRecord, Stats, gc, index_inventory,
    inventory, open_index, project_inventory, quarantine_vectors, reindex, stats,
};
pub use project::{
    EmbeddingScheme, EmbeddingSchemeError, PathKind, StoreError, open_project, path_as_utf8,
    read_embedding_scheme, write_embedding_scheme,
};
pub use retrieve::{
    DenseChunk, LEXICAL_OVERFETCH_CAP, LexicalChunk, RetrievalError, RetrievalSnapshot, fts_query,
};
pub use schema::{SCHEMA_VERSION, open_vectors, open_vectors_for_search};
pub use sync::{
    PageUpsert, SyncError, SyncMode, SyncPlan, SyncReport, embedding_scheme_for, execute_sync_plan,
    plan_synchronization, synchronize,
};
pub use vector::{
    CachedVector, SweepReport, VectorCache, VectorError, VectorInsert, VectorKey,
    VectorKeyParseError, VectorMutationLease, vector_key,
};
