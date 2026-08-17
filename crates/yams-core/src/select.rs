use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;

use crate::{ExitCode, rank::NormalizedScore};

/// Shipped confidence floor, measured against the best cosine in the corpus.
pub const MIN_SCORE: f64 = 0.72;
/// Shipped relative cutoff, measured from the best hit actually shown.
pub const MAX_GAP: f64 = 0.05;
/// Number of page results returned when the caller does not override `-k`.
pub const DEFAULT_K: usize = 5;

/// Whether the configured gate filters results or is reported hypothetically.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateMode {
    Apply,
    Bypass,
}

/// Validated per-query selection controls.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SelectionConfig {
    limit: usize,
    min_score: f64,
    max_gap: f64,
    gate_mode: GateMode,
}

impl SelectionConfig {
    pub fn new(
        limit: usize,
        min_score: f64,
        max_gap: f64,
        gate_mode: GateMode,
    ) -> Result<Self, SelectionError> {
        if !min_score.is_finite() {
            return Err(SelectionError::NonFiniteMinScore);
        }
        if !(-1.0..=1.0).contains(&min_score) {
            return Err(SelectionError::MinScoreOutsideCosineRange);
        }
        if !max_gap.is_finite() {
            return Err(SelectionError::NonFiniteMaxGap);
        }
        if max_gap < 0.0 {
            return Err(SelectionError::NegativeMaxGap);
        }

        Ok(Self {
            limit,
            min_score,
            max_gap,
            gate_mode,
        })
    }

    pub const fn limit(self) -> usize {
        self.limit
    }

    pub const fn min_score(self) -> f64 {
        self.min_score
    }

    pub const fn max_gap(self) -> f64 {
        self.max_gap
    }

    pub const fn gate_mode(self) -> GateMode {
        self.gate_mode
    }
}

impl Default for SelectionConfig {
    fn default() -> Self {
        Self {
            limit: DEFAULT_K,
            min_score: MIN_SCORE,
            max_gap: MAX_GAP,
            gate_mode: GateMode::Apply,
        }
    }
}

/// One already-computed source contribution to a page's fused rank.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RankContribution {
    source: usize,
    rank: usize,
    weight: f64,
    score: f64,
}

impl RankContribution {
    pub const fn new(source: usize, rank: usize, weight: f64, score: f64) -> Self {
        Self {
            source,
            rank,
            weight,
            score,
        }
    }

    pub const fn source(self) -> usize {
        self.source
    }

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

/// Rank signals copied from the ranker, never recalculated during selection.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct HitExplanation {
    dense_rank: Option<usize>,
    bm25_rank: Option<usize>,
    rrf_score: Option<f64>,
    contributions: Vec<RankContribution>,
}

impl HitExplanation {
    pub const fn new(
        dense_rank: Option<usize>,
        bm25_rank: Option<usize>,
        rrf_score: Option<f64>,
        contributions: Vec<RankContribution>,
    ) -> Self {
        Self {
            dense_rank,
            bm25_rank,
            rrf_score,
            contributions,
        }
    }

    pub const fn dense_rank(&self) -> Option<usize> {
        self.dense_rank
    }

    pub const fn bm25_rank(&self) -> Option<usize> {
        self.bm25_rank
    }

    pub const fn rrf_score(&self) -> Option<f64> {
        self.rrf_score
    }

    pub fn contributions(&self) -> &[RankContribution] {
        &self.contributions
    }
}

/// Renderer-owned labels that selection carries without interpreting.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct PageLabels {
    corpus: Option<String>,
    status: Option<String>,
    project: Option<String>,
}

impl PageLabels {
    pub fn new(corpus: Option<&str>, status: Option<&str>, project: Option<&str>) -> Self {
        Self {
            corpus: corpus.map(str::to_owned),
            status: status.map(str::to_owned),
            project: project.map(str::to_owned),
        }
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
}

/// A chunk available for display after the page's ranking has been decided.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SelectedChunk {
    ordinal: u32,
    text: String,
}

impl SelectedChunk {
    pub fn new(ordinal: u32, text: impl Into<String>) -> Self {
        Self {
            ordinal,
            text: text.into(),
        }
    }

    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }

    pub fn text(&self) -> &str {
        &self.text
    }
}

/// Store-independent input from a ranker.
///
/// `final_rank` fixes the page's fused (or dense-only) order. Selection may
/// collapse a duplicate page or replace the final slot with an exact match,
/// but it never recomputes this order, the public cosine, or contributions.
#[derive(Clone, Debug, PartialEq)]
pub struct RankedHit {
    name: String,
    path: String,
    dense_chunk: SelectedChunk,
    lexical_chunk: Option<SelectedChunk>,
    score: NormalizedScore,
    final_rank: usize,
    explanation: HitExplanation,
    labels: PageLabels,
}

impl RankedHit {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        path: impl Into<String>,
        dense_chunk: SelectedChunk,
        lexical_chunk: Option<SelectedChunk>,
        score: NormalizedScore,
        final_rank: usize,
        explanation: HitExplanation,
        labels: PageLabels,
    ) -> Self {
        Self {
            name: name.into(),
            path: path.into(),
            dense_chunk,
            lexical_chunk,
            score,
            final_rank,
            explanation,
            labels,
        }
    }
}

/// One exact-text occurrence used by the pure unique-identifier check.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LiteralChunk<'text> {
    path: &'text str,
    ordinal: u32,
    text: &'text str,
}

impl<'text> LiteralChunk<'text> {
    pub const fn new(path: &'text str, ordinal: u32, text: &'text str) -> Self {
        Self {
            path,
            ordinal,
            text,
        }
    }
}

/// The lowest-ordinal literal occurrence on the one matching page.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExactMatch {
    path: String,
    ordinal: u32,
}

impl ExactMatch {
    pub fn new(path: impl Into<String>, ordinal: u32) -> Self {
        Self {
            path: path.into(),
            ordinal,
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub const fn ordinal(&self) -> u32 {
        self.ordinal
    }
}

/// Recognizes Python's deliberately narrow unique-identifier rescue.
pub fn exact_identifier_match(
    query: &str,
    lexical_leader: Option<&str>,
    chunks: &[LiteralChunk<'_>],
) -> Option<ExactMatch> {
    let token = single_ascii_identifier_token(query)?;
    let lower_camel_case = token.as_bytes()[0].is_ascii_lowercase()
        && token.as_bytes()[1..].iter().any(u8::is_ascii_uppercase);
    if !token.contains('_') && !token.as_bytes().iter().any(u8::is_ascii_digit) && !lower_camel_case
    {
        return None;
    }

    let mut matches = chunks
        .iter()
        .filter(|chunk| contains_identifier(chunk.text, token))
        .collect::<Vec<_>>();
    matches.sort_by(|left, right| {
        left.path
            .cmp(right.path)
            .then_with(|| left.ordinal.cmp(&right.ordinal))
    });

    let first = *matches.first()?;
    if matches.iter().any(|chunk| chunk.path != first.path) {
        return None;
    }
    if lexical_leader != Some(first.path) {
        return None;
    }
    Some(ExactMatch::new(first.path, first.ordinal))
}

fn single_ascii_identifier_token(query: &str) -> Option<&str> {
    let bytes = query.as_bytes();
    let mut tokens = Vec::with_capacity(2);
    let mut start = None;
    for (index, byte) in bytes.iter().copied().enumerate() {
        if is_identifier_byte(byte) {
            start.get_or_insert(index);
        } else if let Some(token_start) = start.take() {
            tokens.push(&query[token_start..index]);
            if tokens.len() > 1 {
                return None;
            }
        }
    }
    if let Some(token_start) = start {
        tokens.push(&query[token_start..]);
    }
    if tokens.len() == 1 {
        Some(tokens[0])
    } else {
        None
    }
}

fn contains_identifier(text: &str, token: &str) -> bool {
    let text = text.as_bytes();
    let token = token.as_bytes();
    text.windows(token.len())
        .enumerate()
        .any(|(start, window)| {
            window == token
                && (start == 0 || !is_identifier_byte(text[start - 1]))
                && (start + token.len() == text.len()
                    || !is_identifier_byte(text[start + token.len()]))
        })
}

const fn is_identifier_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// One page ready for snippets and renderer labels.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectedHit {
    name: String,
    path: String,
    selected_chunk: SelectedChunk,
    score: NormalizedScore,
    exact: bool,
    explanation: HitExplanation,
    labels: PageLabels,
}

impl SelectedHit {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub const fn selected_chunk(&self) -> &SelectedChunk {
        &self.selected_chunk
    }

    pub fn text(&self) -> &str {
        self.selected_chunk.text()
    }

    pub const fn score(&self) -> f64 {
        self.score.get()
    }

    pub const fn exact(&self) -> bool {
        self.exact
    }

    pub const fn explanation(&self) -> &HitExplanation {
        &self.explanation
    }

    pub const fn labels(&self) -> &PageLabels {
        &self.labels
    }
}

/// A path and already-normalized public score named by a gate decision.
#[derive(Clone, Debug, PartialEq)]
pub struct GateHit {
    path: String,
    score: f64,
}

impl GateHit {
    fn from_hit(hit: &SelectedHit) -> Self {
        Self {
            path: hit.path.clone(),
            score: hit.score.get(),
        }
    }

    pub fn path(&self) -> &str {
        &self.path
    }

    pub const fn score(&self) -> f64 {
        self.score
    }
}

/// The single cause chain recorded while applying the gate.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GateReason {
    NoHits,
    Floor,
    Gap,
    Passed,
}

/// Why the gate kept and removed each page.
#[derive(Clone, Debug, PartialEq)]
pub struct GateVerdict {
    baseline: f64,
    min_score: f64,
    max_gap: f64,
    no_hits: bool,
    floor_fired: bool,
    top: Option<f64>,
    floor_dropped: Vec<GateHit>,
    gap_dropped: Vec<GateHit>,
    rescued: Vec<GateHit>,
}

impl GateVerdict {
    pub const fn baseline(&self) -> f64 {
        self.baseline
    }

    pub const fn min_score(&self) -> f64 {
        self.min_score
    }

    pub const fn max_gap(&self) -> f64 {
        self.max_gap
    }

    pub const fn margin(&self) -> f64 {
        self.baseline - self.min_score
    }

    pub const fn no_hits(&self) -> bool {
        self.no_hits
    }

    pub const fn floor_fired(&self) -> bool {
        self.floor_fired
    }

    pub const fn top(&self) -> Option<f64> {
        self.top
    }

    pub fn floor_dropped(&self) -> &[GateHit] {
        &self.floor_dropped
    }

    pub fn gap_dropped(&self) -> &[GateHit] {
        &self.gap_dropped
    }

    pub fn rescued(&self) -> &[GateHit] {
        &self.rescued
    }

    pub fn reason(&self) -> GateReason {
        if self.no_hits {
            GateReason::NoHits
        } else if self.floor_fired {
            GateReason::Floor
        } else if !self.gap_dropped.is_empty() || !self.rescued.is_empty() {
            GateReason::Gap
        } else {
            GateReason::Passed
        }
    }
}

/// Selected hits plus the applied or hypothetical gate explanation.
#[derive(Clone, Debug, PartialEq)]
pub struct SelectionOutcome {
    query: String,
    hits: Vec<SelectedHit>,
    applied: bool,
    gate: Option<GateVerdict>,
    exit_code: ExitCode,
}

impl SelectionOutcome {
    pub fn query(&self) -> &str {
        &self.query
    }

    pub fn hits(&self) -> &[SelectedHit] {
        &self.hits
    }

    pub const fn applied(&self) -> bool {
        self.applied
    }

    pub const fn gate(&self) -> Option<&GateVerdict> {
        self.gate.as_ref()
    }

    pub const fn exit_code(&self) -> ExitCode {
        self.exit_code
    }
}

/// Collapses already-ranked chunks, performs exact rescue, then gates once.
pub fn select(
    query: &str,
    baseline: Option<NormalizedScore>,
    ranked: &[RankedHit],
    literal_chunks: &[LiteralChunk<'_>],
    lexical_leader: Option<&str>,
    config: SelectionConfig,
) -> Result<SelectionOutcome, SelectionError> {
    for hit in ranked {
        validate_hit(hit)?;
    }
    if baseline.is_none() && !ranked.is_empty() {
        return Err(SelectionError::MissingBaselineForCandidates);
    }

    let exact = exact_identifier_match(query, lexical_leader, literal_chunks);
    let mut ordered = ranked.to_vec();
    ordered.sort_by(|left, right| {
        left.final_rank
            .cmp(&right.final_rank)
            .then_with(|| left.path.cmp(&right.path))
            .then_with(|| left.dense_chunk.ordinal.cmp(&right.dense_chunk.ordinal))
    });
    let mut seen = BTreeSet::new();
    ordered.retain(|hit| seen.insert(hit.path.clone()));

    let mut selected = ordered
        .into_iter()
        .map(|hit| selected_hit(hit, exact.as_ref(), literal_chunks))
        .collect::<Vec<_>>();
    let exact_position = exact
        .as_ref()
        .and_then(|matched| selected.iter().position(|hit| hit.path == matched.path));

    let mut shown = selected
        .iter()
        .take(config.limit)
        .cloned()
        .collect::<Vec<_>>();
    if config.limit > 0
        && let Some(position) = exact_position
    {
        selected[position].exact = true;
        if let Some(shown_position) = shown
            .iter()
            .position(|hit| hit.path == selected[position].path)
        {
            shown[shown_position].exact = true;
        } else if shown.len() == config.limit {
            shown.pop();
            shown.push(selected[position].clone());
        } else {
            shown.push(selected[position].clone());
        }
    }

    let applied = config.gate_mode == GateMode::Apply;
    let (gated, gate) = match baseline {
        Some(baseline) => {
            let (gated, verdict) = apply_gate(shown.clone(), baseline.get(), config);
            (gated, Some(verdict))
        }
        None => (Vec::new(), None),
    };
    let hits = if applied { gated } else { shown };
    let exit_code = if !hits.is_empty() {
        ExitCode::Ok
    } else if baseline.is_none() {
        ExitCode::Empty
    } else {
        ExitCode::Unsure
    };

    Ok(SelectionOutcome {
        query: query.to_owned(),
        hits,
        applied,
        gate,
        exit_code,
    })
}

fn selected_hit(
    hit: RankedHit,
    exact: Option<&ExactMatch>,
    literal_chunks: &[LiteralChunk<'_>],
) -> SelectedHit {
    let exact_chunk = exact
        .filter(|matched| matched.path == hit.path)
        .and_then(|matched| {
            literal_chunks
                .iter()
                .find(|chunk| chunk.path == matched.path && chunk.ordinal == matched.ordinal)
                .map(|chunk| SelectedChunk::new(chunk.ordinal, chunk.text))
        });
    let selected_chunk = exact_chunk
        .or(hit.lexical_chunk.clone())
        .unwrap_or_else(|| hit.dense_chunk.clone());
    SelectedHit {
        name: hit.name,
        path: hit.path,
        selected_chunk,
        score: hit.score,
        exact: false,
        explanation: hit.explanation,
        labels: hit.labels,
    }
}

fn apply_gate(
    hits: Vec<SelectedHit>,
    baseline: f64,
    config: SelectionConfig,
) -> (Vec<SelectedHit>, GateVerdict) {
    if hits.is_empty() {
        return (
            Vec::new(),
            GateVerdict {
                baseline,
                min_score: config.min_score,
                max_gap: config.max_gap,
                no_hits: true,
                floor_fired: false,
                top: None,
                floor_dropped: Vec::new(),
                gap_dropped: Vec::new(),
                rescued: Vec::new(),
            },
        );
    }

    if baseline < config.min_score {
        let mut kept = Vec::new();
        let mut floor_dropped = Vec::new();
        let mut rescued = Vec::new();
        for hit in hits {
            if hit.exact {
                rescued.push(GateHit::from_hit(&hit));
                kept.push(hit);
            } else {
                floor_dropped.push(GateHit::from_hit(&hit));
            }
        }
        let top = maximum_score(&kept);
        return (
            kept,
            GateVerdict {
                baseline,
                min_score: config.min_score,
                max_gap: config.max_gap,
                no_hits: false,
                floor_fired: true,
                top,
                floor_dropped,
                gap_dropped: Vec::new(),
                rescued,
            },
        );
    }

    let anchor = maximum_score(&hits).expect("a nonempty validated hit set has a maximum");
    let mut kept = Vec::new();
    let mut gap_dropped = Vec::new();
    let mut rescued = Vec::new();
    for hit in hits {
        let passes = hit.score.get() >= anchor - config.max_gap;
        if passes || hit.exact {
            if !passes {
                rescued.push(GateHit::from_hit(&hit));
            }
            kept.push(hit);
        } else {
            gap_dropped.push(GateHit::from_hit(&hit));
        }
    }
    let top = maximum_score(&kept);
    (
        kept,
        GateVerdict {
            baseline,
            min_score: config.min_score,
            max_gap: config.max_gap,
            no_hits: false,
            floor_fired: false,
            top,
            floor_dropped: Vec::new(),
            gap_dropped,
            rescued,
        },
    )
}

fn maximum_score(hits: &[SelectedHit]) -> Option<f64> {
    hits.iter()
        .map(|hit| hit.score.get())
        .max_by(|left, right| left.total_cmp(right))
}

fn validate_hit(hit: &RankedHit) -> Result<(), SelectionError> {
    let invalid_rank = hit.final_rank == 0
        || hit.explanation.dense_rank == Some(0)
        || hit.explanation.bm25_rank == Some(0)
        || hit
            .explanation
            .rrf_score
            .is_some_and(|score| !score.is_finite())
        || hit.explanation.contributions.iter().any(|contribution| {
            contribution.rank == 0
                || !contribution.weight.is_finite()
                || contribution.weight < 0.0
                || !contribution.score.is_finite()
        });
    if invalid_rank {
        return Err(SelectionError::InvalidRankSignal {
            path: hit.path.clone(),
        });
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SelectionError {
    NonFiniteMinScore,
    MinScoreOutsideCosineRange,
    NonFiniteMaxGap,
    NegativeMaxGap,
    MissingBaselineForCandidates,
    InvalidRankSignal { path: String },
}

impl fmt::Display for SelectionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFiniteMinScore => formatter.write_str("minimum score must be finite"),
            Self::MinScoreOutsideCosineRange => {
                formatter.write_str("minimum score must be within [-1.0, 1.0]")
            }
            Self::NonFiniteMaxGap => formatter.write_str("maximum gap must be finite"),
            Self::NegativeMaxGap => formatter.write_str("maximum gap must be nonnegative"),
            Self::MissingBaselineForCandidates => {
                formatter.write_str("ranked candidates require a corpus baseline")
            }
            Self::InvalidRankSignal { path } => {
                write!(formatter, "rank explanation for {path} is invalid")
            }
        }
    }
}

impl Error for SelectionError {}
