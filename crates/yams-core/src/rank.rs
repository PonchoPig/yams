use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use thiserror::Error;

/// The reciprocal-rank offset used by the shipped hybrid ranker.
pub const RRF_K: usize = 60;
/// Maximum number of pages each source ranker contributes to fusion.
pub const CANDIDATES: usize = 25;
/// Relative contribution of lexical rank beside dense rank.
pub const LEXICAL_WEIGHT: f64 = 0.2;

/// Stable page identity: its absolute, canonical source path.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct PageId(PathBuf);

impl PageId {
    /// Builds an identity from a path already canonicalized by discovery.
    ///
    /// This is a pure lexical check: discovery remains responsible for
    /// resolving the filesystem path without following an unsafe candidate.
    pub fn from_canonical_path(path: impl AsRef<Path>) -> Result<Self, RankError> {
        let path = path.as_ref();
        let normalized = path.components().collect::<PathBuf>();
        let canonical_shape = path.is_absolute()
            && path.to_str().is_some()
            && normalized.as_os_str() == path.as_os_str()
            && !path
                .components()
                .any(|component| matches!(component, Component::CurDir | Component::ParentDir));
        if !canonical_shape {
            return Err(RankError::NonCanonicalPagePath {
                path: path.to_path_buf(),
            });
        }

        Ok(Self(path.to_path_buf()))
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }

    pub fn as_str(&self) -> &str {
        self.0.to_str().expect("PageId construction requires UTF-8")
    }
}

/// Stable chunk identity: a page path plus its zero-based chunk ordinal.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ChunkId {
    page: PageId,
    ordinal: u32,
}

impl ChunkId {
    pub const fn new(page: PageId, ordinal: u32) -> Self {
        Self { page, ordinal }
    }

    pub const fn page(&self) -> &PageId {
        &self.page
    }

    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }
}

/// Identifies which input failed vector validation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VectorSide {
    Left,
    Right,
}

/// Computes cosine similarity with `f64` products and accumulators.
pub fn cosine(left: &[f32], right: &[f32]) -> Result<f64, RankError> {
    if left.len() != right.len() {
        return Err(RankError::DimensionMismatch {
            left: left.len(),
            right: right.len(),
        });
    }

    let left_norm = checked_squared_norm(left, VectorSide::Left)?;
    let right_norm = checked_squared_norm(right, VectorSide::Right)?;
    let dot = left
        .iter()
        .zip(right)
        .map(|(left, right)| f64::from(*left) * f64::from(*right))
        .sum::<f64>();
    let score = dot / (left_norm.sqrt() * right_norm.sqrt());
    if !score.is_finite() {
        return Err(RankError::NonFiniteScore);
    }

    // Floating arithmetic can stray a few ulps past the mathematical range.
    Ok(score.clamp(-1.0, 1.0))
}

fn checked_squared_norm(values: &[f32], side: VectorSide) -> Result<f64, RankError> {
    if values.is_empty() {
        return Err(RankError::EmptyVector { side });
    }

    let mut squared_norm = 0.0;
    for (index, value) in values.iter().copied().enumerate() {
        if !value.is_finite() {
            return Err(RankError::NonFiniteComponent { side, index });
        }
        squared_norm += f64::from(value) * f64::from(value);
    }
    if squared_norm == 0.0 {
        return Err(RankError::ZeroNorm { side });
    }
    Ok(squared_norm)
}

/// A finite cosine rounded exactly as Python's four-decimal public score.
#[derive(Clone, Copy, Debug, PartialEq, PartialOrd)]
pub struct NormalizedScore(f64);

impl NormalizedScore {
    pub const fn get(self) -> f64 {
        self.0
    }
}

/// Normalizes a cosine before it reaches gating or public output.
pub fn normalize_score(score: f64) -> Result<NormalizedScore, RankError> {
    if !score.is_finite() {
        return Err(RankError::NonFiniteScore);
    }
    if !(-1.0..=1.0).contains(&score) {
        return Err(RankError::ScoreOutsideCosineRange);
    }

    // Rust and Python both render binary64 values to a requested decimal
    // precision with correctly rounded conversion. Parsing that decimal back
    // preserves Python's `round(score, 4)` boundary behavior, including cases
    // where multiplying by 10_000 first would create a false exact tie.
    let rounded = format!("{score:.4}")
        .parse::<f64>()
        .expect("finite decimal float formatting always parses");
    Ok(NormalizedScore(rounded))
}

/// One store-loaded chunk and its validated embedding bytes decoded to `f32`.
#[derive(Clone, Debug)]
pub struct DenseCandidate<'vector> {
    id: ChunkId,
    vector: &'vector [f32],
}

impl<'vector> DenseCandidate<'vector> {
    pub const fn new(id: ChunkId, vector: &'vector [f32]) -> Self {
        Self { id, vector }
    }

    pub const fn id(&self) -> &ChunkId {
        &self.id
    }

    pub const fn vector(&self) -> &'vector [f32] {
        self.vector
    }
}

/// A page's best dense chunk, ordered by its unrounded cosine.
#[derive(Clone, Debug, PartialEq)]
pub struct DenseRankedPage {
    chunk: ChunkId,
    score: NormalizedScore,
}

impl DenseRankedPage {
    pub fn page(&self) -> &PageId {
        self.chunk.page()
    }

    pub const fn chunk(&self) -> &ChunkId {
        &self.chunk
    }

    /// Returns the four-decimal value that gating and output must consume.
    pub const fn score(&self) -> NormalizedScore {
        self.score
    }
}

/// Ranks every valid chunk and collapses to the best chunk per canonical page.
pub fn dense_rank(
    query: &[f32],
    candidates: &[DenseCandidate<'_>],
) -> Result<Vec<DenseRankedPage>, RankError> {
    let mut chunks = candidates
        .iter()
        .map(|candidate| {
            Ok(ScoredChunk {
                id: candidate.id.clone(),
                score: cosine(query, candidate.vector)?,
            })
        })
        .collect::<Result<Vec<_>, RankError>>()?;

    chunks.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| left.id.cmp(&right.id))
    });

    let mut seen = BTreeSet::new();
    chunks
        .into_iter()
        .filter(|chunk| seen.insert(chunk.id.page().clone()))
        .map(|chunk| {
            Ok(DenseRankedPage {
                chunk: chunk.id,
                score: normalize_score(chunk.score)?,
            })
        })
        .collect()
}

#[derive(Clone, Debug)]
struct ScoredChunk {
    id: ChunkId,
    score: f64,
}

/// One source rank's exact contribution to a fused page score.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RrfContribution {
    source: usize,
    rank: usize,
    weight: f64,
    score: f64,
}

impl RrfContribution {
    /// Returns the zero-based source-ranking index.
    pub const fn source(self) -> usize {
        self.source
    }

    /// Returns this source's one-based rank.
    pub const fn rank(self) -> usize {
        self.rank
    }

    pub const fn weight(self) -> f64 {
        self.weight
    }

    pub const fn score(self) -> f64 {
        self.score
    }
}

/// A fused total and the source ranks used to compute it once.
#[derive(Clone, Debug, PartialEq)]
pub struct RrfScore {
    total: f64,
    contributions: Vec<RrfContribution>,
}

impl RrfScore {
    pub const fn total(&self) -> f64 {
        self.total
    }

    pub fn contributions(&self) -> &[RrfContribution] {
        &self.contributions
    }
}

/// Computes RRF totals together with the exact source contributions.
///
/// Repeated pages in a source are collapsed at their first occurrence before
/// one-based ranks are assigned, matching page-level source rankings.
pub fn rrf_explained(
    rankings: &[Vec<PageId>],
    weights: &[f64],
    rrf_k: usize,
) -> Result<BTreeMap<PageId, RrfScore>, RankError> {
    validate_weights(rankings.len(), weights)?;

    let mut scores: BTreeMap<PageId, RrfScore> = BTreeMap::new();
    for (source, (ranking, weight)) in rankings.iter().zip(weights).enumerate() {
        let mut seen_pages = BTreeSet::new();
        for (index, page) in ranking
            .iter()
            .filter(|page| seen_pages.insert((*page).clone()))
            .enumerate()
        {
            let rank = index + 1;
            let score = *weight / (rrf_k as f64 + rank as f64);
            if !score.is_finite() {
                return Err(RankError::NonFiniteRrfContribution {
                    source_index: source,
                    rank,
                });
            }
            let page_score = scores.entry(page.clone()).or_insert_with(|| RrfScore {
                total: 0.0,
                contributions: Vec::new(),
            });
            let total = page_score.total + score;
            if !total.is_finite() {
                return Err(RankError::NonFiniteFusedTotal { page: page.clone() });
            }
            page_score.total = total;
            page_score.contributions.push(RrfContribution {
                source,
                rank,
                weight: *weight,
                score,
            });
        }
    }
    Ok(scores)
}

/// Projects the single explained RRF calculation to numeric totals.
pub fn rrf_scores(
    rankings: &[Vec<PageId>],
    weights: &[f64],
    rrf_k: usize,
) -> Result<BTreeMap<PageId, f64>, RankError> {
    Ok(rrf_explained(rankings, weights, rrf_k)?
        .into_iter()
        .map(|(page, score)| (page, score.total))
        .collect())
}

/// Orders fused IDs by descending score and canonical page path for ties.
pub fn ranked_ids(scores: &BTreeMap<PageId, f64>, limit: usize) -> Vec<PageId> {
    let mut ranked = scores.iter().collect::<Vec<_>>();
    ranked.sort_by(|(left_id, left_score), (right_id, right_score)| {
        right_score
            .total_cmp(left_score)
            .then_with(|| left_id.cmp(right_id))
    });
    ranked
        .into_iter()
        .take(limit)
        .map(|(page, _)| page.clone())
        .collect()
}

fn validate_weights(rankings: usize, weights: &[f64]) -> Result<(), RankError> {
    if rankings != weights.len() {
        return Err(RankError::RankingWeightMismatch {
            rankings,
            weights: weights.len(),
        });
    }
    for (source, weight) in weights.iter().copied().enumerate() {
        if !weight.is_finite() {
            return Err(RankError::NonFiniteWeight {
                source_index: source,
            });
        }
        if weight < 0.0 {
            return Err(RankError::NegativeWeight {
                source_index: source,
            });
        }
    }
    Ok(())
}

/// One BM25-scored chunk in best-first source order.
///
/// Fusion collapses this stream to the first chunk for each canonical page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LexicalCandidate {
    chunk: ChunkId,
}

impl LexicalCandidate {
    pub const fn new(chunk: ChunkId) -> Self {
        Self { chunk }
    }

    pub fn page(&self) -> &PageId {
        self.chunk.page()
    }

    pub const fn chunk(&self) -> &ChunkId {
        &self.chunk
    }
}

/// One page after the dense and optional lexical ranks are fused.
#[derive(Clone, Debug, PartialEq)]
pub struct HybridRankedPage {
    dense: DenseRankedPage,
    lexical_chunk: Option<ChunkId>,
    dense_rank: usize,
    lexical_rank: Option<usize>,
    fusion: Option<RrfScore>,
}

impl HybridRankedPage {
    pub fn page(&self) -> &PageId {
        self.dense.page()
    }

    /// Returns the best-cosine chunk. Result selection may replace it with the
    /// best lexical chunk for display without changing this page's rank.
    pub const fn dense_chunk(&self) -> &ChunkId {
        self.dense.chunk()
    }

    /// Returns the page's first (best) BM25 chunk, when it ranked lexically.
    pub const fn lexical_chunk(&self) -> Option<&ChunkId> {
        self.lexical_chunk.as_ref()
    }

    /// Returns the BM25 chunk for display, falling back to the best cosine chunk.
    pub fn selected_chunk(&self) -> &ChunkId {
        self.lexical_chunk
            .as_ref()
            .unwrap_or_else(|| self.dense.chunk())
    }

    pub const fn score(&self) -> NormalizedScore {
        self.dense.score()
    }

    /// Returns the one-based position in the full dense page ranking.
    pub const fn dense_rank(&self) -> usize {
        self.dense_rank
    }

    /// Returns the one-based position in the unfiltered lexical page ranking.
    pub const fn lexical_rank(&self) -> Option<usize> {
        self.lexical_rank
    }

    /// Returns the RRF calculation, or `None` on the dense-only fallback.
    pub const fn fusion(&self) -> Option<&RrfScore> {
        self.fusion.as_ref()
    }
}

/// Applies the shipped page-level dense/lexical fusion to already-ranked pages.
///
/// `lexical` is the best-first BM25 chunk stream. Its first chunk per page is
/// retained, then the first 25 unique pages receive their unfiltered BM25
/// ranks. Pages without a dense vector keep their BM25 positions for explain
/// but are removed before the lexical RRF ranks are assigned.
pub fn hybrid_rank(
    dense: &[DenseRankedPage],
    lexical: &[LexicalCandidate],
    limit: usize,
) -> Result<Vec<HybridRankedPage>, RankError> {
    let dense_by_page = dense
        .iter()
        .enumerate()
        .map(|(index, page)| (page.page(), (index + 1, page)))
        .collect::<BTreeMap<_, _>>();
    let mut seen_lexical_pages = BTreeSet::new();
    let lexical_pages = lexical
        .iter()
        .filter(|candidate| seen_lexical_pages.insert(candidate.page().clone()))
        .take(CANDIDATES)
        .collect::<Vec<_>>();
    let lexical_rank =
        lexical_pages
            .iter()
            .enumerate()
            .fold(BTreeMap::new(), |mut ranks, (index, candidate)| {
                ranks.insert(candidate.page(), (index + 1, candidate.chunk()));
                ranks
            });
    let usable_lexical = lexical_pages
        .iter()
        .filter(|candidate| dense_by_page.contains_key(candidate.page()))
        .map(|candidate| candidate.page().clone())
        .collect::<Vec<_>>();

    if usable_lexical.is_empty() {
        return Ok(dense
            .iter()
            .take(limit)
            .enumerate()
            .map(|(index, page)| HybridRankedPage {
                dense: page.clone(),
                lexical_chunk: None,
                dense_rank: index + 1,
                lexical_rank: None,
                fusion: None,
            })
            .collect());
    }

    let rankings = vec![
        dense
            .iter()
            .take(CANDIDATES)
            .map(|page| page.page().clone())
            .collect(),
        usable_lexical,
    ];
    let scores = rrf_explained(&rankings, &[1.0, LEXICAL_WEIGHT], RRF_K)?;
    let mut order = scores.keys().cloned().collect::<Vec<_>>();
    order.sort_by(|left, right| {
        scores[right]
            .total
            .total_cmp(&scores[left].total)
            .then_with(|| left.cmp(right))
    });

    Ok(order
        .into_iter()
        .take(limit)
        .map(|page| {
            let (dense_rank, dense_page) = dense_by_page[&page];
            HybridRankedPage {
                dense: dense_page.clone(),
                lexical_chunk: lexical_rank.get(&page).map(|(_, chunk)| (*chunk).clone()),
                dense_rank,
                lexical_rank: lexical_rank.get(&page).map(|(rank, _)| *rank),
                fusion: Some(scores[&page].clone()),
            }
        })
        .collect())
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum RankError {
    #[error("page path is not an absolute canonical UTF-8 path: {path:?}")]
    NonCanonicalPagePath { path: PathBuf },

    #[error("vector dimensions differ: left has {left}, right has {right}")]
    DimensionMismatch { left: usize, right: usize },

    #[error("{side:?} vector is empty")]
    EmptyVector { side: VectorSide },

    #[error("{side:?} vector component {index} is not finite")]
    NonFiniteComponent { side: VectorSide, index: usize },

    #[error("{side:?} vector has zero norm")]
    ZeroNorm { side: VectorSide },

    #[error("ranking score is not finite")]
    NonFiniteScore,

    #[error("ranking score is outside the cosine range [-1, 1]")]
    ScoreOutsideCosineRange,

    #[error("ranking/weight length mismatch: {rankings} rankings, {weights} weights")]
    RankingWeightMismatch { rankings: usize, weights: usize },

    #[error("RRF source weight {source_index} is not finite")]
    NonFiniteWeight { source_index: usize },

    #[error("RRF source weight {source_index} is negative")]
    NegativeWeight { source_index: usize },

    #[error("RRF contribution at source {source_index}, rank {rank} is not finite")]
    NonFiniteRrfContribution { source_index: usize, rank: usize },

    #[error("RRF total for {page:?} is not finite")]
    NonFiniteFusedTotal { page: PageId },
}
