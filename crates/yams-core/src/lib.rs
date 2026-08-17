mod chunk;
mod corpus;
mod frontmatter;
mod query_log;
mod rank;
mod sanitize;
mod scan;
mod search;
mod select;
mod snippet;

pub use chunk::{Chunk, ChunkError, MAX_CHUNK, MIN_CHUNK, chunk, chunks_for_page, embed_text_for};
pub use corpus::{
    Corpus, CorpusKind, Discovery, DiscoveryError, DiscoveryNote, DiscoveryNoteKind,
    DiscoveryReport, corpora_for, discover_corpora, project_root,
};
pub use frontmatter::{ParsedPage, parse_frontmatter, title_for};
pub use query_log::{
    MAX_PROJECT_BYTES, MAX_QUERY_BYTES, QueryLogEligibility, QueryLogOutcome, QueryLogRecord,
    QueryLogSkip, append_query_log, query_hash,
};
pub use rank::{
    CANDIDATES, ChunkId, DenseCandidate, DenseRankedPage, HybridRankedPage, LEXICAL_WEIGHT,
    LexicalCandidate, NormalizedScore, PageId, RRF_K, RankError, RrfContribution, RrfScore,
    VectorSide, cosine, dense_rank, hybrid_rank, normalize_score, ranked_ids, rrf_explained,
    rrf_scores,
};
pub use sanitize::{TerminalText, sanitize_terminal};
pub use scan::{
    MAX_FILE_BYTES, PageRevision, ScanNote, ScanNoteKind, ScanReport, ScannedPage, scan_corpora,
};
pub use search::{
    ChunkMetadata, LexicalScore, PageMetadata, SearchError, SearchExplanation, SearchHit,
    SearchRequest, SearchResponse, compose_search,
};
pub use select::{
    DEFAULT_K, ExactMatch, GateHit, GateMode, GateReason, GateVerdict, HitExplanation,
    LiteralChunk, MAX_GAP, MIN_SCORE, PageLabels, RankContribution, RankedHit, SelectedChunk,
    SelectedHit, SelectionConfig, SelectionError, SelectionOutcome, exact_identifier_match, select,
};
pub use snippet::{
    SNIPPET_GAIN, SNIPPET_WIDTH, Snippet, SnippetError, SnippetStatistics, TermFrequency,
    WeightedTerm, query_terms, snippet, term_weights,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(i32)]
pub enum ExitCode {
    Ok = 0,
    Empty = 1,
    Usage = 2,
    Unsure = 3,
    Operational = 4,
}

impl From<ExitCode> for i32 {
    fn from(value: ExitCode) -> Self {
        value as Self
    }
}
