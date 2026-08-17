use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use thiserror::Error;

use crate::{
    CANDIDATES, ChunkId, DenseCandidate, ExitCode, GateMode, GateVerdict, HitExplanation,
    LexicalCandidate, LiteralChunk, NormalizedScore, PageId, PageLabels, RankContribution,
    RankError, RankedHit, SNIPPET_GAIN, SNIPPET_WIDTH, SelectedChunk, SelectionConfig,
    SelectionError, SnippetError, SnippetStatistics, dense_rank, hybrid_rank, query_terms, select,
    snippet, term_weights,
};

/// Renderer-independent metadata for one indexed page.
#[derive(Clone, Eq, PartialEq)]
pub struct PageMetadata {
    id: PageId,
    name: String,
    labels: PageLabels,
}

impl fmt::Debug for PageMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PageMetadata")
            .field("path", &"<redacted>")
            .field("name_bytes", &self.name.len())
            .field("corpus_present", &self.labels.corpus().is_some())
            .field("status_present", &self.labels.status().is_some())
            .field("project_present", &self.labels.project().is_some())
            .finish()
    }
}

impl PageMetadata {
    pub fn new(id: PageId, name: impl Into<String>, labels: PageLabels) -> Self {
        Self {
            id,
            name: name.into(),
            labels,
        }
    }

    pub const fn id(&self) -> &PageId {
        &self.id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub const fn labels(&self) -> &PageLabels {
        &self.labels
    }
}

/// The exact, full body of one indexed chunk.
///
/// These bodies are also the literal corpus used by the narrow identifier
/// rescue. The composition kernel never opens a file or queries a store.
#[derive(Clone, Eq, PartialEq)]
pub struct ChunkMetadata {
    id: ChunkId,
    text: String,
}

impl fmt::Debug for ChunkMetadata {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ChunkMetadata")
            .field("path", &"<redacted>")
            .field("ordinal", &self.id.ordinal())
            .field("text_bytes", &self.text.len())
            .finish()
    }
}

impl ChunkMetadata {
    pub fn new(id: ChunkId, text: impl Into<String>) -> Self {
        Self {
            id,
            text: text.into(),
        }
    }

    pub const fn id(&self) -> &ChunkId {
        &self.id
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// One FTS chunk in the store's complete, unfiltered BM25 order.
#[derive(Clone, PartialEq)]
pub struct LexicalScore {
    id: ChunkId,
    bm25: f64,
    rank: usize,
}

impl fmt::Debug for LexicalScore {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LexicalScore")
            .field("path", &"<redacted>")
            .field("ordinal", &self.id.ordinal())
            .field("bm25", &self.bm25)
            .field("rank", &self.rank)
            .finish()
    }
}

impl LexicalScore {
    pub const fn new(id: ChunkId, bm25: f64, rank: usize) -> Self {
        Self { id, bm25, rank }
    }

    pub const fn id(&self) -> &ChunkId {
        &self.id
    }

    pub const fn bm25(&self) -> f64 {
        self.bm25
    }

    pub const fn rank(&self) -> usize {
        self.rank
    }
}

/// All already-loaded inputs needed to compose one search response.
pub struct SearchRequest<'input> {
    query: &'input str,
    query_embedding: &'input [f32],
    pages: &'input [PageMetadata],
    chunks: &'input [ChunkMetadata],
    dense: &'input [DenseCandidate<'input>],
    lexical: &'input [LexicalScore],
    snippet_statistics: &'input SnippetStatistics,
    selection: SelectionConfig,
}

impl<'input> SearchRequest<'input> {
    #[allow(clippy::too_many_arguments)]
    pub const fn new(
        query: &'input str,
        query_embedding: &'input [f32],
        pages: &'input [PageMetadata],
        chunks: &'input [ChunkMetadata],
        dense: &'input [DenseCandidate<'input>],
        lexical: &'input [LexicalScore],
        snippet_statistics: &'input SnippetStatistics,
        selection: SelectionConfig,
    ) -> Self {
        Self {
            query,
            query_embedding,
            pages,
            chunks,
            dense,
            lexical,
            snippet_statistics,
            selection,
        }
    }
}

/// One owned result body ready for either a text or JSON renderer.
///
/// Every string remains the exact untrusted value supplied by the store. JSON
/// rendering must escape it, and human rendering must pass it through the
/// terminal sanitizer at the presentation boundary.
#[derive(Clone, PartialEq)]
pub struct SearchHit {
    name: String,
    path: String,
    score: NormalizedScore,
    text: String,
    snippet: String,
    clipped_start: bool,
    clipped_end: bool,
    corpus: Option<String>,
    status: Option<String>,
    project: Option<String>,
    exact: bool,
}

impl fmt::Debug for SearchHit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchHit")
            .field("name_bytes", &self.name.len())
            .field("path", &"<redacted>")
            .field("score", &self.score)
            .field("text_bytes", &self.text.len())
            .field("snippet_bytes", &self.snippet.len())
            .field("clipped_start", &self.clipped_start)
            .field("clipped_end", &self.clipped_end)
            .field("corpus_present", &self.corpus.is_some())
            .field("status_present", &self.status.is_some())
            .field("project_present", &self.project.is_some())
            .field("exact", &self.exact)
            .finish()
    }
}

impl SearchHit {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub const fn score(&self) -> NormalizedScore {
        self.score
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn snippet(&self) -> &str {
        &self.snippet
    }

    pub const fn clipped_start(&self) -> bool {
        self.clipped_start
    }

    pub const fn clipped_end(&self) -> bool {
        self.clipped_end
    }

    pub fn corpus(&self) -> Option<&str> {
        self.corpus.as_deref()
    }

    pub fn status(&self) -> Option<&str> {
        self.status.as_deref()
    }

    pub fn project(&self) -> Option<&str> {
        self.project.as_deref()
    }

    pub const fn exact(&self) -> bool {
        self.exact
    }
}

/// Gate and rank signals computed by the same path that selected the hits.
#[derive(Clone, PartialEq)]
pub struct SearchExplanation {
    query: String,
    applied: bool,
    gate: Option<GateVerdict>,
    hits: BTreeMap<String, HitExplanation>,
}

impl fmt::Debug for SearchExplanation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchExplanation")
            .field("query_bytes", &self.query.len())
            .field("applied", &self.applied)
            .field("gate_present", &self.gate.is_some())
            .field("gate_reason", &self.gate.as_ref().map(GateVerdict::reason))
            .field("hit_count", &self.hits.len())
            .finish()
    }
}

impl SearchExplanation {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub const fn applied(&self) -> bool {
        self.applied
    }

    pub const fn gate(&self) -> Option<&GateVerdict> {
        self.gate.as_ref()
    }

    pub fn hits(&self) -> &BTreeMap<String, HitExplanation> {
        &self.hits
    }

    pub fn hit(&self, path: &str) -> Option<&HitExplanation> {
        self.hits.get(path)
    }
}

/// An owned result plus the explanation a caller may choose to render.
#[derive(Clone, PartialEq)]
pub struct SearchResponse {
    query: String,
    hits: Vec<SearchHit>,
    explanation: SearchExplanation,
    exit_code: ExitCode,
}

impl fmt::Debug for SearchResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchResponse")
            .field("query_bytes", &self.query.len())
            .field("hit_count", &self.hits.len())
            .field("applied", &self.explanation.applied)
            .field("gate_present", &self.explanation.gate.is_some())
            .field("exit_code", &self.exit_code)
            .finish()
    }
}

impl SearchResponse {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn hits(&self) -> &[SearchHit] {
        &self.hits
    }

    pub const fn explanation(&self) -> &SearchExplanation {
        &self.explanation
    }

    pub const fn exit_code(&self) -> ExitCode {
        self.exit_code
    }
}

/// Compose ranking, exact selection, gating, and snippets without I/O.
pub fn compose_search(request: SearchRequest<'_>) -> Result<SearchResponse, SearchError> {
    let mut page_by_id = BTreeMap::new();
    for page in request.pages {
        if page_by_id.insert(page.id.clone(), page).is_some() {
            return Err(SearchError::DuplicatePageMetadata {
                page: page.id.clone(),
            });
        }
    }
    let mut chunk_by_id = BTreeMap::new();
    for chunk in request.chunks {
        if !page_by_id.contains_key(chunk.id.page()) {
            return Err(SearchError::MissingPageMetadata {
                page: chunk.id.page().clone(),
            });
        }
        if chunk_by_id.insert(chunk.id.clone(), chunk).is_some() {
            return Err(SearchError::DuplicateChunkMetadata {
                chunk: chunk.id.clone(),
            });
        }
    }
    validate_dense(request.dense, &chunk_by_id)?;
    validate_lexical(request.lexical, &chunk_by_id)?;
    validate_statistics(
        request.query,
        request.snippet_statistics,
        request.chunks.len(),
    )?;

    let dense = dense_rank(request.query_embedding, request.dense)?;
    if dense.is_empty() {
        return Ok(empty_response(request.query, request.selection.gate_mode()));
    }

    let lexical = request
        .lexical
        .iter()
        .map(|candidate| LexicalCandidate::new(candidate.id.clone()))
        .collect::<Vec<_>>();
    let ranked = hybrid_rank(&dense, &lexical, dense.len().saturating_add(CANDIDATES))?;
    let ranked = ranked
        .iter()
        .enumerate()
        .map(|(index, hit)| {
            let page =
                page_by_id
                    .get(hit.page())
                    .ok_or_else(|| SearchError::MissingPageMetadata {
                        page: hit.page().clone(),
                    })?;
            let dense_chunk = selected_chunk(&chunk_by_id, hit.dense_chunk())?;
            let lexical_chunk = hit
                .lexical_chunk()
                .map(|id| selected_chunk(&chunk_by_id, id))
                .transpose()?;
            let explanation = HitExplanation::new(
                Some(hit.dense_rank()),
                hit.lexical_rank(),
                hit.fusion().map(|score| score.total()),
                hit.fusion()
                    .map(|score| {
                        score
                            .contributions()
                            .iter()
                            .map(|contribution| {
                                RankContribution::new(
                                    contribution.source(),
                                    contribution.rank(),
                                    contribution.weight(),
                                    contribution.score(),
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            );
            Ok(RankedHit::new(
                &page.name,
                page.id.as_str(),
                dense_chunk,
                lexical_chunk,
                hit.score(),
                index + 1,
                explanation,
                page.labels.clone(),
            ))
        })
        .collect::<Result<Vec<_>, SearchError>>()?;
    let literal = request
        .chunks
        .iter()
        .map(|chunk| LiteralChunk::new(chunk.id.page().as_str(), chunk.id.ordinal(), &chunk.text))
        .collect::<Vec<_>>();
    let dense_pages = dense
        .iter()
        .map(|page| page.page().clone())
        .collect::<BTreeSet<_>>();
    let mut seen = BTreeSet::new();
    let lexical_leader = lexical
        .iter()
        .filter(|candidate| seen.insert(candidate.page().clone()))
        .take(CANDIDATES)
        .find(|candidate| dense_pages.contains(candidate.page()))
        .map(|candidate| candidate.page().as_str());

    let bypass = SelectionConfig::new(
        request.selection.limit(),
        request.selection.min_score(),
        request.selection.max_gap(),
        GateMode::Bypass,
    )
    .expect("a validated selection remains valid when bypassing the gate");
    let baseline = dense.first().map(|page| page.score());
    let ungated = select(
        request.query,
        baseline,
        &ranked,
        &literal,
        lexical_leader,
        bypass,
    )?;
    let outcome = if request.selection.gate_mode() == GateMode::Bypass {
        ungated.clone()
    } else {
        select(
            request.query,
            baseline,
            &ranked,
            &literal,
            lexical_leader,
            request.selection,
        )?
    };

    let weights = term_weights(request.snippet_statistics)?;
    let hits = outcome
        .hits()
        .iter()
        .map(|hit| {
            let window = snippet(hit.text(), &weights, SNIPPET_WIDTH, SNIPPET_GAIN)?;
            Ok(SearchHit {
                name: hit.name().to_owned(),
                path: hit.path().to_owned(),
                score: crate::normalize_score(hit.score())?,
                text: hit.text().to_owned(),
                snippet: window.text,
                clipped_start: window.clipped_start,
                clipped_end: window.clipped_end,
                corpus: hit.labels().corpus().map(str::to_owned),
                status: hit.labels().status().map(str::to_owned),
                project: hit.labels().project().map(str::to_owned),
                exact: hit.exact(),
            })
        })
        .collect::<Result<Vec<_>, SearchError>>()?;
    let explained_hits = ungated
        .hits()
        .iter()
        .map(|hit| (hit.path().to_owned(), hit.explanation().clone()))
        .collect();
    let explanation = SearchExplanation {
        query: request.query.to_owned(),
        applied: outcome.applied(),
        gate: outcome.gate().cloned(),
        hits: explained_hits,
    };

    Ok(SearchResponse {
        query: request.query.to_owned(),
        hits,
        explanation,
        exit_code: outcome.exit_code(),
    })
}

fn validate_dense(
    dense: &[DenseCandidate<'_>],
    chunks: &BTreeMap<ChunkId, &ChunkMetadata>,
) -> Result<(), SearchError> {
    let mut seen = BTreeSet::new();
    for candidate in dense {
        if !chunks.contains_key(candidate.id()) {
            return Err(SearchError::MissingChunkMetadata {
                chunk: candidate.id().clone(),
            });
        }
        if !seen.insert(candidate.id().clone()) {
            return Err(SearchError::DuplicateDenseCandidate {
                chunk: candidate.id().clone(),
            });
        }
    }
    if let Some(missing) = chunks.keys().find(|chunk| !seen.contains(*chunk)) {
        return Err(SearchError::MissingDenseCandidate {
            chunk: missing.clone(),
        });
    }
    Ok(())
}

fn validate_lexical(
    lexical: &[LexicalScore],
    chunks: &BTreeMap<ChunkId, &ChunkMetadata>,
) -> Result<(), SearchError> {
    let mut seen = BTreeSet::new();
    let mut previous: Option<(f64, ChunkId)> = None;
    for (index, candidate) in lexical.iter().enumerate() {
        if !candidate.bm25.is_finite() {
            return Err(SearchError::NonFiniteBm25 {
                chunk: candidate.id.clone(),
            });
        }
        let expected = index + 1;
        if candidate.rank != expected {
            return Err(SearchError::InvalidLexicalRank {
                chunk: candidate.id.clone(),
                expected,
                actual: candidate.rank,
            });
        }
        if !chunks.contains_key(&candidate.id) {
            return Err(SearchError::MissingChunkMetadata {
                chunk: candidate.id.clone(),
            });
        }
        if !seen.insert(candidate.id.clone()) {
            return Err(SearchError::DuplicateLexicalCandidate {
                chunk: candidate.id.clone(),
            });
        }
        if let Some((previous_score, previous_id)) = &previous
            && candidate.bm25 < *previous_score
        {
            return Err(SearchError::LexicalOrder {
                previous: previous_id.clone(),
                current: candidate.id.clone(),
            });
        }
        previous = Some((candidate.bm25, candidate.id.clone()));
    }
    Ok(())
}

fn validate_statistics(
    query: &str,
    statistics: &SnippetStatistics,
    chunk_count: usize,
) -> Result<(), SearchError> {
    if statistics.total_chunks != chunk_count as u64 {
        return Err(SearchError::SnippetChunkCountMismatch {
            metadata: chunk_count,
            statistics: statistics.total_chunks,
        });
    }
    let mut actual = BTreeSet::new();
    for frequency in &statistics.frequencies {
        let term = frequency.term.to_ascii_lowercase();
        if !actual.insert(term.clone()) {
            return Err(SearchError::DuplicateSnippetTerm { term });
        }
    }
    if statistics.total_chunks == 0 {
        return Ok(());
    }
    let expected = query_terms(query).into_iter().collect::<BTreeSet<_>>();
    if expected != actual {
        return Err(SearchError::SnippetTermsMismatch {
            expected: expected.into_iter().collect(),
            actual: actual.into_iter().collect(),
        });
    }
    Ok(())
}

fn selected_chunk(
    chunks: &BTreeMap<ChunkId, &ChunkMetadata>,
    id: &ChunkId,
) -> Result<SelectedChunk, SearchError> {
    let chunk = chunks
        .get(id)
        .ok_or_else(|| SearchError::MissingChunkMetadata { chunk: id.clone() })?;
    Ok(SelectedChunk::new(id.ordinal(), &chunk.text))
}

fn empty_response(query: &str, gate_mode: GateMode) -> SearchResponse {
    SearchResponse {
        query: query.to_owned(),
        hits: Vec::new(),
        explanation: SearchExplanation {
            query: query.to_owned(),
            applied: gate_mode == GateMode::Apply,
            gate: None,
            hits: BTreeMap::new(),
        },
        exit_code: ExitCode::Empty,
    }
}

#[derive(Clone, Error, PartialEq)]
pub enum SearchError {
    #[error("search rank failed: {0}")]
    Rank(#[from] RankError),
    #[error("search selection failed: {0}")]
    Selection(#[from] SelectionError),
    #[error("search snippet failed: {0}")]
    Snippet(#[from] SnippetError),
    #[error("ranked page has no loaded metadata: {page:?}")]
    MissingPageMetadata { page: PageId },
    #[error("page metadata is duplicated: {page:?}")]
    DuplicatePageMetadata { page: PageId },
    #[error("ranked chunk has no loaded metadata: {chunk:?}")]
    MissingChunkMetadata { chunk: ChunkId },
    #[error("chunk metadata is duplicated: {chunk:?}")]
    DuplicateChunkMetadata { chunk: ChunkId },
    #[error("dense candidate is duplicated: {chunk:?}")]
    DuplicateDenseCandidate { chunk: ChunkId },
    #[error("loaded chunk has no dense candidate: {chunk:?}")]
    MissingDenseCandidate { chunk: ChunkId },
    #[error("lexical candidate is duplicated: {chunk:?}")]
    DuplicateLexicalCandidate { chunk: ChunkId },
    #[error("lexical BM25 score for {chunk:?} is not finite")]
    NonFiniteBm25 { chunk: ChunkId },
    #[error("lexical rank for {chunk:?} is {actual}, expected {expected}")]
    InvalidLexicalRank {
        chunk: ChunkId,
        expected: usize,
        actual: usize,
    },
    #[error("lexical BM25 order regressed between {previous:?} and {current:?}")]
    LexicalOrder { previous: ChunkId, current: ChunkId },
    #[error(
        "snippet statistics cover {statistics} chunks but {metadata} literal chunks were loaded"
    )]
    SnippetChunkCountMismatch { metadata: usize, statistics: u64 },
    #[error("snippet terms differ: expected {expected:?}, got {actual:?}")]
    SnippetTermsMismatch {
        expected: Vec<String>,
        actual: Vec<String>,
    },
    #[error("snippet term is duplicated: {term:?}")]
    DuplicateSnippetTerm { term: String },
}

impl fmt::Debug for SearchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Rank(_) => "Rank",
            Self::Selection(_) => "Selection",
            Self::Snippet(_) => "Snippet",
            Self::MissingPageMetadata { .. } => "MissingPageMetadata",
            Self::DuplicatePageMetadata { .. } => "DuplicatePageMetadata",
            Self::MissingChunkMetadata { .. } => "MissingChunkMetadata",
            Self::DuplicateChunkMetadata { .. } => "DuplicateChunkMetadata",
            Self::DuplicateDenseCandidate { .. } => "DuplicateDenseCandidate",
            Self::MissingDenseCandidate { .. } => "MissingDenseCandidate",
            Self::DuplicateLexicalCandidate { .. } => "DuplicateLexicalCandidate",
            Self::NonFiniteBm25 { .. } => "NonFiniteBm25",
            Self::InvalidLexicalRank { .. } => "InvalidLexicalRank",
            Self::LexicalOrder { .. } => "LexicalOrder",
            Self::SnippetChunkCountMismatch { .. } => "SnippetChunkCountMismatch",
            Self::SnippetTermsMismatch { .. } => "SnippetTermsMismatch",
            Self::DuplicateSnippetTerm { .. } => "DuplicateSnippetTerm",
        };
        formatter.debug_tuple(variant).field(&"<redacted>").finish()
    }
}
