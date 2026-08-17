//! Pure, owned rendering for direct and service search responses.
//!
//! JSON retains untrusted strings exactly and lets `serde_json` escape them.
//! Human output sanitizes each dynamic field before surrounding it with the
//! small trusted styling vocabulary accepted by `yams_core`.

use std::{
    collections::BTreeMap,
    fmt::{self, Write as _},
};

use serde_json::{Map, Number, Value};
use thiserror::Error;
use yams_core::{CorpusKind, TerminalText, sanitize_terminal};

/// One selected page and the exact display chunk chosen for it.
#[derive(Clone, PartialEq)]
pub struct SearchHit {
    pub name: String,
    pub path: String,
    pub score: f64,
    pub text: String,
    pub snippet: String,
    pub clipped_start: bool,
    pub clipped_end: bool,
    pub corpus: CorpusKind,
    pub exact: bool,
    pub status: Option<String>,
    pub explanation: Option<HitExplanation>,
}

/// Rank signals attached only when the caller requested an explanation.
#[derive(Clone, PartialEq)]
pub struct HitExplanation {
    pub dense_rank: Option<usize>,
    pub bm25_rank: Option<usize>,
    pub rrf_score: Option<f64>,
}

/// A page and its public score removed or rescued by the gate.
#[derive(Clone, PartialEq)]
pub struct GateEntry {
    pub path: String,
    pub score: f64,
}

/// The complete gate decision, including hypothetical `--no-gate` outcomes.
#[derive(Clone, PartialEq)]
pub struct GateVerdict {
    pub baseline: f64,
    pub min_score: f64,
    pub max_gap: f64,
    pub no_hits: bool,
    pub floor_fired: bool,
    pub top: Option<f64>,
    pub floor_dropped: Vec<GateEntry>,
    pub gap_dropped: Vec<GateEntry>,
    pub rescued: Vec<GateEntry>,
}

/// Query-level context for the opt-in explain envelope.
#[derive(Clone, PartialEq)]
pub struct SearchExplanation {
    pub query: String,
    pub applied: bool,
    pub gate: Option<GateVerdict>,
}

/// One project's presentation-neutral search result.
#[derive(Clone, PartialEq)]
pub struct SearchResponse {
    pub hits: Vec<SearchHit>,
    pub explanation: Option<SearchExplanation>,
}

/// A project root and its independently ranked hits for `--all`.
#[derive(Clone, PartialEq)]
pub struct ProjectSearchResponse {
    pub project: String,
    pub hits: Vec<SearchHit>,
}

/// Styling selected by the process that owns the output stream.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Styling {
    Plain,
    Ansi,
}

/// Text layout selected by the direct or grouped renderer.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TextOptions {
    full: bool,
    show_path: bool,
    indent: usize,
    styling: Styling,
}

impl TextOptions {
    /// Layout for one project's ordinary direct search.
    pub const fn single(full: bool, styling: Styling) -> Self {
        Self {
            full,
            show_path: true,
            indent: 0,
            styling,
        }
    }

    /// Indented layout for a project inside an `--all` result.
    pub const fn grouped(full: bool, styling: Styling) -> Self {
        Self {
            full,
            show_path: false,
            indent: 2,
            styling,
        }
    }
}

/// UTF-8-aware bounded formatting sink used by direct text rendering.
///
/// Writes that would cross the byte cap are refused as a whole, so retained
/// output always remains valid UTF-8 and never grows past the configured cap.
pub struct BoundedBuffer {
    inner: String,
    cap: usize,
    overflowed: bool,
}

impl BoundedBuffer {
    /// Maximum bytes retained independently for each direct output stream.
    pub const DIRECT_STREAM_CAP: usize = 4 * 1024 * 1024;

    /// Construct an empty sink with the supplied byte cap.
    pub fn new(cap: usize) -> Self {
        Self {
            inner: String::new(),
            cap,
            overflowed: false,
        }
    }

    /// Whether this sink has refused a write that would exceed its cap.
    pub const fn overflowed(&self) -> bool {
        self.overflowed
    }

    /// Consume the sink and return only the bytes accepted before overflow.
    pub fn into_string(self) -> String {
        self.inner
    }

    fn is_empty(&self) -> bool {
        self.inner.is_empty()
    }
}

impl fmt::Write for BoundedBuffer {
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.overflowed
            || self
                .inner
                .len()
                .checked_add(value.len())
                .is_none_or(|length| length > self.cap)
        {
            self.overflowed = true;
            return Err(fmt::Error);
        }
        self.inner.push_str(value);
        Ok(())
    }
}

/// A typed refusal to emit invalid or incomplete response data.
#[derive(Error, Eq, PartialEq)]
pub enum RenderError {
    #[error("renderer field `{field}` must be finite")]
    NonFinite { field: &'static str },
    #[error("renderer could not serialize JSON: {0}")]
    Json(String),
    #[error("explained response is missing rank signals for a redacted hit")]
    MissingHitExplanation { path: String },
    #[error("renderer field `{field}` must be a one-based rank")]
    InvalidRank { field: &'static str },
    #[error("renderer output exceeds the direct stream limit")]
    OutputLimit,
}

impl From<fmt::Error> for RenderError {
    fn from(_: fmt::Error) -> Self {
        Self::OutputLimit
    }
}

#[derive(Clone, Copy)]
struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}

impl fmt::Debug for SearchHit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchHit")
            .field("name", &Redacted)
            .field("path", &Redacted)
            .field("score", &self.score)
            .field("text", &Redacted)
            .field("snippet", &Redacted)
            .field("clipped_start", &self.clipped_start)
            .field("clipped_end", &self.clipped_end)
            .field("corpus", &self.corpus)
            .field("exact", &self.exact)
            .field("status", &self.status.as_ref().map(|_| Redacted))
            .field("explanation", &self.explanation)
            .finish()
    }
}

impl fmt::Debug for HitExplanation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HitExplanation")
            .field("dense_rank", &self.dense_rank)
            .field("bm25_rank", &self.bm25_rank)
            .field("rrf_score", &self.rrf_score)
            .finish()
    }
}

impl fmt::Debug for GateEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GateEntry")
            .field("path", &Redacted)
            .field("score", &self.score)
            .finish()
    }
}

impl fmt::Debug for GateVerdict {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("GateVerdict")
            .field("baseline", &self.baseline)
            .field("min_score", &self.min_score)
            .field("max_gap", &self.max_gap)
            .field("no_hits", &self.no_hits)
            .field("floor_fired", &self.floor_fired)
            .field("top", &self.top)
            .field("floor_dropped", &self.floor_dropped)
            .field("gap_dropped", &self.gap_dropped)
            .field("rescued", &self.rescued)
            .finish()
    }
}

impl fmt::Debug for SearchExplanation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchExplanation")
            .field("query", &Redacted)
            .field("applied", &self.applied)
            .field("gate", &self.gate)
            .finish()
    }
}

impl fmt::Debug for SearchResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SearchResponse")
            .field("hits", &self.hits)
            .field("explanation", &self.explanation)
            .finish()
    }
}

impl fmt::Debug for ProjectSearchResponse {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ProjectSearchResponse")
            .field("project", &Redacted)
            .field("hits", &self.hits)
            .finish()
    }
}

impl fmt::Debug for RenderError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonFinite { field } => formatter
                .debug_struct("NonFinite")
                .field("field", field)
                .finish(),
            Self::Json(_) => formatter.debug_tuple("Json").field(&Redacted).finish(),
            Self::MissingHitExplanation { .. } => formatter
                .debug_struct("MissingHitExplanation")
                .field("path", &Redacted)
                .finish(),
            Self::InvalidRank { field } => formatter
                .debug_struct("InvalidRank")
                .field("field", field)
                .finish(),
            Self::OutputLimit => formatter.write_str("OutputLimit"),
        }
    }
}

/// Render a bare hit array or, when requested, an explain envelope.
pub fn render_json(response: &SearchResponse) -> Result<String, RenderError> {
    validate_response(response)?;
    let value = if let Some(explanation) = &response.explanation {
        explain_json(&response.hits, explanation)?
    } else {
        let hits = response
            .hits
            .iter()
            .map(|hit| hit_json(hit, false, None))
            .collect::<Result<Vec<_>, _>>()?;
        Value::Array(hits)
    };
    json_line(&value)
}

/// Flatten independently ranked project groups into the `--all` JSON array.
pub fn render_all_json(groups: &[ProjectSearchResponse]) -> Result<String, RenderError> {
    for group in groups {
        for hit in &group.hits {
            validate_hit(hit)?;
        }
    }
    let hits = groups
        .iter()
        .flat_map(|group| {
            group
                .hits
                .iter()
                .map(|hit| hit_json(hit, false, Some(&group.project)))
        })
        .collect::<Result<Vec<_>, _>>()?;
    json_line(&Value::Array(hits))
}

/// Render project headings and their independently ranked, indented hits.
pub fn render_all_text(
    groups: &[ProjectSearchResponse],
    full: bool,
    styling: Styling,
) -> Result<String, RenderError> {
    for group in groups {
        for hit in &group.hits {
            validate_hit(hit)?;
        }
    }
    if groups.is_empty() {
        return Ok("no results\n".to_owned());
    }

    let mut output = BoundedBuffer::new(BoundedBuffer::DIRECT_STREAM_CAP);
    let (underline, reset) = match styling {
        Styling::Plain => ("", ""),
        Styling::Ansi => ("\u{1b}[4m", "\u{1b}[0m"),
    };
    for group in groups {
        if group.hits.is_empty() {
            continue;
        }
        let project = sanitize_terminal(&group.project, TerminalText::Inline);
        writeln!(output, "\n{underline}{project}{reset}")?;
        render_text_inner(
            &mut output,
            &group.hits,
            None,
            TextOptions::grouped(full, styling),
        )?;
    }
    if output.is_empty() {
        return Ok("no results\n".to_owned());
    }
    output.write_char('\n')?;
    Ok(finish_text_output(output))
}

/// Render one untrusted diagnostic as a single terminal-safe line.
pub fn render_diagnostic(message: &str) -> String {
    let message = sanitize_terminal(message, TerminalText::Inline);
    format!("{message}\n")
}

/// Render one project's result as full text or query-relevant snippets.
pub fn render_text(response: &SearchResponse, options: TextOptions) -> Result<String, RenderError> {
    validate_response(response)?;
    let mut output = BoundedBuffer::new(BoundedBuffer::DIRECT_STREAM_CAP);
    render_text_inner(
        &mut output,
        &response.hits,
        response.explanation.as_ref(),
        options,
    )?;
    Ok(finish_text_output(output))
}

fn finish_text_output(output: BoundedBuffer) -> String {
    let output = output.into_string();
    sanitize_terminal(&output, TerminalText::RenderedFrame).into_owned()
}

fn render_text_inner(
    output: &mut BoundedBuffer,
    hits: &[SearchHit],
    explanation: Option<&SearchExplanation>,
    options: TextOptions,
) -> Result<(), RenderError> {
    if hits.is_empty() {
        output.write_str(
            if explanation.is_some_and(|explanation| explanation.gate.is_some()) {
                "no confident match\n"
            } else {
                "no results\n"
            },
        )?;
        if let Some(explanation) = explanation {
            push_explanation_text(output, explanation, options.indent)?;
        }
        return Ok(());
    }

    let seen = hits
        .iter()
        .fold(BTreeMap::<String, usize>::new(), |mut counts, hit| {
            let title = sanitize_terminal(&hit.name, TerminalText::Inline).into_owned();
            *counts.entry(title).or_default() += 1;
            counts
        });
    let indent = " ".repeat(options.indent);
    let width = 280usize.saturating_sub(2 * options.indent);
    let (bold, reset) = match options.styling {
        Styling::Plain => ("", ""),
        Styling::Ansi => ("\u{1b}[1m", "\u{1b}[0m"),
    };

    if let Some(explanation) = explanation {
        push_explanation_text(output, explanation, options.indent)?;
    }

    for hit in hits {
        let score = text_score("score", hit.score)?;
        let title = sanitize_terminal(&hit.name, TerminalText::Inline);
        let path = sanitize_terminal(&hit.path, TerminalText::Inline);
        let labels = text_labels(hit);
        let prefix = if options.indent == 0 { "\n" } else { "" };
        writeln!(
            output,
            "{prefix}{indent}{bold}{title}{reset}  ({score}){labels}"
        )?;
        if options.show_path || seen[title.as_ref()] > 1 {
            writeln!(output, "{indent}  {path}")?;
        }

        if options.full {
            let body = sanitize_terminal(&hit.text, TerminalText::Multiline);
            push_indented_multiline(output, &indent, &body)?;
        } else {
            let (body, narrowed) = take_chars(&hit.snippet, width);
            let body = sanitize_terminal(&body, TerminalText::Inline);
            let lead = if hit.clipped_start { "..." } else { "" };
            let trail = if hit.clipped_end || narrowed {
                "..."
            } else {
                ""
            };
            writeln!(output, "{indent}  {lead}{body}{trail}")?;
        }
        if explanation.is_some() {
            let explanation =
                hit.explanation
                    .as_ref()
                    .ok_or_else(|| RenderError::MissingHitExplanation {
                        path: hit.path.clone(),
                    })?;
            let dense = rank_text(explanation.dense_rank);
            let bm25 = rank_text(explanation.bm25_rank);
            let rrf = explanation
                .rrf_score
                .map(|score| {
                    if !score.is_finite() {
                        Err(RenderError::NonFinite { field: "rrf_score" })
                    } else {
                        Ok(format!("{score:.5}"))
                    }
                })
                .transpose()?
                .unwrap_or_else(|| "—".to_owned());
            writeln!(
                output,
                "{indent}    dense {dense}   bm25 {bm25}   rrf {rrf}"
            )?;
        }
    }
    if options.indent == 0 {
        output.write_char('\n')?;
    }

    Ok(())
}

fn validate_response(response: &SearchResponse) -> Result<(), RenderError> {
    for hit in &response.hits {
        validate_hit(hit)?;
    }
    if let Some(gate) = response
        .explanation
        .as_ref()
        .and_then(|explanation| explanation.gate.as_ref())
    {
        validate_gate(gate)?;
    }
    Ok(())
}

fn validate_hit(hit: &SearchHit) -> Result<(), RenderError> {
    require_finite("score", hit.score)?;
    if let Some(explanation) = &hit.explanation {
        validate_rank("dense_rank", explanation.dense_rank)?;
        validate_rank("bm25_rank", explanation.bm25_rank)?;
        if let Some(score) = explanation.rrf_score {
            require_finite("rrf_score", score)?;
        }
    }
    Ok(())
}

fn validate_rank(field: &'static str, rank: Option<usize>) -> Result<(), RenderError> {
    if rank == Some(0) {
        Err(RenderError::InvalidRank { field })
    } else {
        Ok(())
    }
}

fn validate_gate(gate: &GateVerdict) -> Result<(), RenderError> {
    require_finite("baseline", gate.baseline)?;
    require_finite("min_score", gate.min_score)?;
    require_finite("max_gap", gate.max_gap)?;
    require_finite("margin", gate.baseline - gate.min_score)?;
    if let Some(top) = gate.top {
        require_finite("top", top)?;
    }
    for entry in &gate.floor_dropped {
        require_finite("floor_dropped.score", entry.score)?;
    }
    for entry in &gate.gap_dropped {
        require_finite("gap_dropped.score", entry.score)?;
    }
    for entry in &gate.rescued {
        require_finite("rescued.score", entry.score)?;
    }
    Ok(())
}

fn require_finite(field: &'static str, value: f64) -> Result<(), RenderError> {
    if value.is_finite() {
        Ok(())
    } else {
        Err(RenderError::NonFinite { field })
    }
}

fn push_explanation_text(
    output: &mut BoundedBuffer,
    explanation: &SearchExplanation,
    indentation: usize,
) -> Result<(), RenderError> {
    let indent = " ".repeat(indentation);
    let Some(gate) = &explanation.gate else {
        writeln!(
            output,
            "{indent}gate: not consulted — this corpus holds no vectors"
        )?;
        return Ok(());
    };

    let baseline = text_score("baseline", gate.baseline)?;
    let min_score = text_number("min_score", gate.min_score, 2)?;
    let max_gap = text_number("max_gap", gate.max_gap, 2)?;
    let margin_value = gate.baseline - gate.min_score;
    if !margin_value.is_finite() {
        return Err(RenderError::NonFinite { field: "margin" });
    }
    let note = if explanation.applied {
        ""
    } else {
        "   (shown anyway: --no-gate)"
    };
    writeln!(
        output,
        "{indent}baseline {baseline}   floor {min_score}   gap {max_gap}   margin {margin_value:+.4}{note}"
    )?;

    let hypothetical = !explanation.applied;
    if gate.no_hits {
        writeln!(
            output,
            "{indent}  no hits reached the gate — nothing to decide on"
        )?;
    } else if gate.floor_fired {
        let action = if hypothetical { "would drop" } else { "drops" };
        writeln!(
            output,
            "{indent}  floor {action} everything unmarked: the corpus-wide best is under it"
        )?;
    } else if !gate.gap_dropped.is_empty() || !gate.rescued.is_empty() {
        let action = if hypothetical {
            "would have decided"
        } else {
            "decided"
        };
        writeln!(output, "{indent}  floor cleared; the gap {action} the rest")?;
    } else {
        let action = if hypothetical { "would pass" } else { "pass" };
        writeln!(output, "{indent}  both gates {action} everything")?;
    }

    let dropped = if hypothetical {
        "would drop"
    } else {
        "dropped"
    };
    for entry in &gate.floor_dropped {
        push_gate_entry(output, &indent, "floor", dropped, entry, "")?;
    }
    for entry in &gate.gap_dropped {
        push_gate_entry(output, &indent, "gap  ", dropped, entry, "")?;
    }
    let kept = if hypothetical { "would keep" } else { "kept" };
    for entry in &gate.rescued {
        push_gate_entry(
            output,
            &indent,
            "rescue",
            kept,
            entry,
            "  (exact identifier)",
        )?;
    }
    Ok(())
}

fn push_gate_entry(
    output: &mut BoundedBuffer,
    indent: &str,
    gate: &str,
    action: &str,
    entry: &GateEntry,
    suffix: &str,
) -> Result<(), RenderError> {
    let score = text_score("gate_entry.score", entry.score)?;
    let path = sanitize_terminal(&entry.path, TerminalText::Inline);
    writeln!(output, "{indent}  {gate} {action}  {score}  {path}{suffix}")?;
    Ok(())
}

fn rank_text(rank: Option<usize>) -> String {
    rank.map_or_else(|| "—".to_owned(), |rank| format!("#{rank}"))
}

fn text_labels(hit: &SearchHit) -> String {
    let mut labels = Vec::new();
    if hit.corpus != CorpusKind::Shared {
        labels.push(corpus_name(hit.corpus).to_owned());
    }
    if let Some(status) = hit
        .status
        .as_deref()
        .filter(|status| !status.is_empty() && *status != "current")
    {
        labels.push(sanitize_terminal(status, TerminalText::Inline).into_owned());
    }
    if labels.is_empty() {
        String::new()
    } else {
        format!("  [{}]", labels.join(", "))
    }
}

fn push_indented_multiline(
    output: &mut BoundedBuffer,
    indent: &str,
    body: &str,
) -> Result<(), RenderError> {
    if body.is_empty() {
        writeln!(output, "{indent}  ")?;
        return Ok(());
    }
    for line in body.split_inclusive('\n') {
        write!(output, "{indent}  {line}")?;
        if !line.ends_with('\n') {
            output.write_char('\n')?;
        }
    }
    Ok(())
}

fn take_chars(input: &str, width: usize) -> (String, bool) {
    let mut characters = input.chars();
    let body = characters.by_ref().take(width).collect::<String>();
    (body, characters.next().is_some())
}

fn text_score(field: &'static str, value: f64) -> Result<String, RenderError> {
    if !value.is_finite() {
        return Err(RenderError::NonFinite { field });
    }
    Ok(format!("{value:.4}"))
}

fn text_number(field: &'static str, value: f64, precision: usize) -> Result<String, RenderError> {
    if !value.is_finite() {
        return Err(RenderError::NonFinite { field });
    }
    Ok(format!("{value:.precision$}"))
}

fn hit_json(
    hit: &SearchHit,
    include_explanation: bool,
    project: Option<&str>,
) -> Result<Value, RenderError> {
    let mut object = Map::new();
    object.insert("name".to_owned(), Value::String(hit.name.clone()));
    object.insert("path".to_owned(), Value::String(hit.path.clone()));
    object.insert("score".to_owned(), finite_four("score", hit.score)?);
    object.insert("text".to_owned(), Value::String(hit.text.clone()));
    object.insert("snippet".to_owned(), Value::String(hit.snippet.clone()));
    object.insert("clipped_start".to_owned(), hit.clipped_start.into());
    object.insert("clipped_end".to_owned(), hit.clipped_end.into());
    object.insert(
        "corpus".to_owned(),
        Value::String(corpus_name(hit.corpus).to_owned()),
    );
    object.insert("exact".to_owned(), hit.exact.into());
    if let Some(status) = &hit.status {
        object.insert("status".to_owned(), Value::String(status.clone()));
    }
    if let Some(project) = project {
        object.insert("project".to_owned(), Value::String(project.to_owned()));
    }
    if include_explanation {
        let explanation =
            hit.explanation
                .as_ref()
                .ok_or_else(|| RenderError::MissingHitExplanation {
                    path: hit.path.clone(),
                })?;
        object.insert("explain".to_owned(), hit_explanation_json(explanation)?);
    }
    Ok(Value::Object(object))
}

fn explain_json(hits: &[SearchHit], explanation: &SearchExplanation) -> Result<Value, RenderError> {
    let mut object = Map::new();
    object.insert("query".to_owned(), Value::String(explanation.query.clone()));
    object.insert("applied".to_owned(), explanation.applied.into());
    object.insert(
        "gate".to_owned(),
        explanation
            .gate
            .as_ref()
            .map(gate_json)
            .transpose()?
            .unwrap_or(Value::Null),
    );
    object.insert(
        "hits".to_owned(),
        Value::Array(
            hits.iter()
                .map(|hit| hit_json(hit, true, None))
                .collect::<Result<Vec<_>, _>>()?,
        ),
    );
    Ok(Value::Object(object))
}

fn hit_explanation_json(explanation: &HitExplanation) -> Result<Value, RenderError> {
    let mut object = Map::new();
    object.insert(
        "dense_rank".to_owned(),
        explanation.dense_rank.map_or(Value::Null, Value::from),
    );
    object.insert(
        "bm25_rank".to_owned(),
        explanation.bm25_rank.map_or(Value::Null, Value::from),
    );
    object.insert(
        "rrf_score".to_owned(),
        explanation
            .rrf_score
            .map(|score| finite_number("rrf_score", score))
            .transpose()?
            .unwrap_or(Value::Null),
    );
    Ok(Value::Object(object))
}

fn gate_json(gate: &GateVerdict) -> Result<Value, RenderError> {
    let mut object = Map::new();
    object.insert(
        "baseline".to_owned(),
        finite_four("baseline", gate.baseline)?,
    );
    object.insert(
        "min_score".to_owned(),
        finite_number("min_score", gate.min_score)?,
    );
    object.insert(
        "max_gap".to_owned(),
        finite_number("max_gap", gate.max_gap)?,
    );
    object.insert(
        "margin".to_owned(),
        finite_four("margin", gate.baseline - gate.min_score)?,
    );
    object.insert("no_hits".to_owned(), gate.no_hits.into());
    object.insert("floor_fired".to_owned(), gate.floor_fired.into());
    object.insert(
        "top".to_owned(),
        gate.top
            .map(|score| finite_four("top", score))
            .transpose()?
            .unwrap_or(Value::Null),
    );
    object.insert(
        "floor_dropped".to_owned(),
        gate_entries_json("floor_dropped.score", &gate.floor_dropped)?,
    );
    object.insert(
        "gap_dropped".to_owned(),
        gate_entries_json("gap_dropped.score", &gate.gap_dropped)?,
    );
    object.insert(
        "rescued".to_owned(),
        gate_entries_json("rescued.score", &gate.rescued)?,
    );
    Ok(Value::Object(object))
}

fn gate_entries_json(field: &'static str, entries: &[GateEntry]) -> Result<Value, RenderError> {
    entries
        .iter()
        .map(|entry| {
            let mut object = Map::new();
            object.insert("path".to_owned(), Value::String(entry.path.clone()));
            object.insert("score".to_owned(), finite_four(field, entry.score)?);
            Ok(Value::Object(object))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Value::Array)
}

fn finite_four(field: &'static str, value: f64) -> Result<Value, RenderError> {
    if !value.is_finite() {
        return Err(RenderError::NonFinite { field });
    }
    let rounded = format!("{value:.4}")
        .parse::<f64>()
        .expect("finite decimal float formatting always parses");
    Ok(Value::Number(
        Number::from_f64(rounded).expect("a rounded finite value remains finite"),
    ))
}

fn finite_number(field: &'static str, value: f64) -> Result<Value, RenderError> {
    if !value.is_finite() {
        return Err(RenderError::NonFinite { field });
    }
    Ok(Value::Number(Number::from_f64(value).expect(
        "a validated finite value has a JSON representation",
    )))
}

fn corpus_name(corpus: CorpusKind) -> &'static str {
    match corpus {
        CorpusKind::Shared => "shared",
        CorpusKind::Private => "private",
        CorpusKind::Override => "override",
    }
}

fn json_line(value: &Value) -> Result<String, RenderError> {
    let serialized = serde_json::to_string_pretty(value)
        .map_err(|error| RenderError::Json(error.to_string()))?;
    let mut output = String::with_capacity(serialized.len());
    for character in serialized.chars() {
        if matches!(character, '\u{007f}'..='\u{009f}') {
            write!(output, "\\u{:04x}", character as u32)
                .expect("writing terminal-control escapes into a string cannot fail");
        } else {
            output.push(character);
        }
    }
    output.push('\n');
    Ok(output)
}
