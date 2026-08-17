use std::collections::{BTreeMap, HashSet};

use thiserror::Error;

pub const SNIPPET_WIDTH: usize = 280;
pub const SNIPPET_GAIN: f64 = 0.75;

const STOPWORDS: &[&str] = &[
    "a", "an", "and", "are", "as", "at", "be", "but", "by", "for", "from", "has", "have", "how",
    "i", "if", "in", "into", "is", "it", "its", "of", "on", "or", "that", "the", "their", "then",
    "there", "these", "they", "this", "to", "was", "were", "what", "when", "where", "which", "who",
    "why", "will", "with", "you", "your", "do", "does", "did", "not", "no", "any", "all", "my",
    "me", "we", "our", "us", "can", "could", "should", "would", "about", "after", "before", "over",
    "under", "more", "most", "some", "such", "only", "own", "same", "so", "than", "too", "very",
    "just", "now", "yet", "still", "get", "got", "make", "made", "use", "used", "using",
];

#[derive(Clone, Debug, PartialEq)]
pub struct TermFrequency {
    pub term: String,
    pub matching_chunks: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct SnippetStatistics {
    pub total_chunks: u64,
    pub frequencies: Vec<TermFrequency>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct WeightedTerm {
    pub term: String,
    pub weight: f64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Snippet {
    pub text: String,
    pub clipped_start: bool,
    pub clipped_end: bool,
}

#[derive(Clone, Debug, Error, PartialEq)]
pub enum SnippetError {
    #[error("snippet width must be at least one Unicode scalar")]
    ZeroWidth,
    #[error("snippet gain must be finite and within 0..=1")]
    InvalidGain,
    #[error("snippet term `{term}` is not a lexical identifier token")]
    InvalidTerm { term: String },
    #[error("snippet weight for `{term}` must be finite and within 0..=1")]
    InvalidWeight { term: String },
    #[error(
        "snippet frequency for `{term}` exceeds the index total: {matching_chunks} > {total_chunks}"
    )]
    FrequencyExceedsTotal {
        term: String,
        matching_chunks: u64,
        total_chunks: u64,
    },
    #[error("snippet term `{term}` was supplied with conflicting weights")]
    ConflictingWeight { term: String },
}

/// Extract the exact lexical term set shared by FTS and snippet weighting.
///
/// Tokens are Unicode alphanumeric identifiers of at least two characters,
/// plus `_`. Stopwords are removed only when a content term remains; an
/// all-stopword query retains its weak lexical signal. Repeated terms keep
/// their first position.
pub fn query_terms(query: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();

    for character in query.chars().chain(std::iter::once(' ')) {
        if character.is_alphanumeric() || character == '_' {
            for folded in character.to_lowercase() {
                if folded.is_alphanumeric() || folded == '_' {
                    current.push(folded);
                }
            }
        } else {
            if current.chars().count() > 1 {
                tokens.push(std::mem::take(&mut current));
            }
            current.clear();
        }
    }

    let content = tokens
        .iter()
        .filter(|term| !STOPWORDS.contains(&term.as_str()))
        .cloned()
        .collect::<Vec<_>>();
    let selected = if content.is_empty() { tokens } else { content };
    let mut seen = HashSet::new();
    selected
        .into_iter()
        .filter(|term| seen.insert(term.clone()))
        .collect()
}

/// Convert store-supplied chunk frequencies into normalized rarity weights.
///
/// `matching_chunks` deliberately counts FTS rows, not pages. The store owns
/// those observations; this calculation remains deterministic and database
/// independent.
pub fn term_weights(statistics: &SnippetStatistics) -> Result<Vec<WeightedTerm>, SnippetError> {
    if statistics.total_chunks == 0 {
        return Ok(Vec::new());
    }

    let total = statistics.total_chunks as f64;
    let ceiling = total.ln_1p();
    statistics
        .frequencies
        .iter()
        .map(|frequency| {
            validate_term(&frequency.term)?;
            if frequency.matching_chunks > statistics.total_chunks {
                return Err(SnippetError::FrequencyExceedsTotal {
                    term: frequency.term.clone(),
                    matching_chunks: frequency.matching_chunks,
                    total_chunks: statistics.total_chunks,
                });
            }
            let matching = frequency.matching_chunks as f64;
            Ok(WeightedTerm {
                term: frequency
                    .term
                    .chars()
                    .flat_map(char::to_lowercase)
                    .collect(),
                weight: (total / (1.0 + matching)).ln_1p() / ceiling,
            })
        })
        .collect()
}

/// Select a query-relevant display window without consulting a model or store.
///
/// Width is measured in Unicode scalar values. The implementation never slices
/// UTF-8 at a byte offset, but deliberately does not promise grapheme-cluster or
/// terminal-cell boundaries.
pub fn snippet(
    text: &str,
    terms: &[WeightedTerm],
    width: usize,
    gain: f64,
) -> Result<Snippet, SnippetError> {
    if width == 0 {
        return Err(SnippetError::ZeroWidth);
    }
    if !gain.is_finite() || !(0.0..=1.0).contains(&gain) {
        return Err(SnippetError::InvalidGain);
    }
    let terms = canonical_terms(terms)?;
    let collapsed = collapse_python_whitespace(text);
    let characters = collapsed.chars().collect::<Vec<_>>();
    if characters.len() <= width {
        return Ok(Snippet {
            text: collapsed,
            clipped_start: false,
            clipped_end: false,
        });
    }

    let mut spans = term_spans(&characters, &terms);
    spans.sort_by(|left, right| {
        (left.start, left.end, terms[left.term].term.as_str()).cmp(&(
            right.start,
            right.end,
            terms[right.term].term.as_str(),
        ))
    });

    let (head_start, head_end) = snap(&characters, 0, width);
    let head = make_snippet(&characters, head_start, head_end);
    if spans.is_empty() {
        return Ok(head);
    }

    let head_score = window_score(head_start, head_end, &terms, &spans);
    let bar = head_score + gain;
    let mut chosen = None;
    let mut chosen_score = head_score;
    for span in &spans {
        let span_width = span.end - span.start;
        let centered = span
            .start
            .saturating_sub(width.saturating_sub(span_width) / 2)
            .min(characters.len() - width);
        let (start, end) = snap(&characters, centered, width);
        let candidate_score = window_score(start, end, &terms, &spans);
        if candidate_score > bar && candidate_score > chosen_score {
            chosen_score = candidate_score;
            chosen = Some(make_snippet(&characters, start, end));
        }
    }

    Ok(chosen.unwrap_or(head))
}

fn validate_term(term: &str) -> Result<(), SnippetError> {
    if term.chars().count() < 2
        || !term
            .chars()
            .all(|character| character.is_alphanumeric() || character == '_')
    {
        return Err(SnippetError::InvalidTerm {
            term: term.to_owned(),
        });
    }
    Ok(())
}

fn collapse_python_whitespace(text: &str) -> String {
    let mut collapsed = String::with_capacity(text.len());
    let mut pending_space = false;
    for character in text.chars() {
        if is_python_whitespace(character) {
            pending_space = !collapsed.is_empty();
        } else {
            if pending_space {
                collapsed.push(' ');
                pending_space = false;
            }
            collapsed.push(character);
        }
    }
    collapsed
}

fn is_python_whitespace(character: char) -> bool {
    character.is_whitespace() || matches!(character, '\u{001c}'..='\u{001f}')
}

fn canonical_terms(terms: &[WeightedTerm]) -> Result<Vec<WeightedTerm>, SnippetError> {
    let mut canonical = BTreeMap::<String, f64>::new();
    for term in terms {
        validate_term(&term.term)?;
        if !term.weight.is_finite() || !(0.0..=1.0).contains(&term.weight) {
            return Err(SnippetError::InvalidWeight {
                term: term.term.clone(),
            });
        }
        let name: String = term.term.chars().flat_map(char::to_lowercase).collect();
        if let Some(existing) = canonical.insert(name.clone(), term.weight)
            && existing.to_bits() != term.weight.to_bits()
        {
            return Err(SnippetError::ConflictingWeight { term: name });
        }
    }
    Ok(canonical
        .into_iter()
        .map(|(term, weight)| WeightedTerm { term, weight })
        .collect())
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
struct Span {
    start: usize,
    end: usize,
    term: usize,
}

fn term_spans(text: &[char], terms: &[WeightedTerm]) -> Vec<Span> {
    let mut spans = Vec::new();
    for (term_index, weighted) in terms.iter().enumerate() {
        if weighted.weight <= 0.0 {
            continue;
        }
        let needle = weighted.term.chars().collect::<Vec<_>>();
        if needle.len() > text.len() {
            continue;
        }
        for start in 0..=text.len() - needle.len() {
            let end = start + needle.len();
            if start > 0 && is_identifier(text[start - 1]) {
                continue;
            }
            if end < text.len() && is_identifier(text[end]) {
                continue;
            }
            if text[start..end]
                .iter()
                .zip(&needle)
                .all(|(actual, expected)| actual.to_lowercase().eq(expected.to_lowercase()))
            {
                spans.push(Span {
                    start,
                    end,
                    term: term_index,
                });
            }
        }
    }
    spans
}

fn is_identifier(character: char) -> bool {
    character.is_alphanumeric() || character == '_'
}

fn snap(text: &[char], mut start: usize, width: usize) -> (usize, usize) {
    let mut end = text.len().min(start + width);
    if start > 0 {
        start = text[..start]
            .iter()
            .rposition(|character| *character == ' ')
            .map_or(0, |space| space + 1);
        end = text.len().min(start + width);
    }
    if end < text.len()
        && let Some(relative_space) = text[start..=end]
            .iter()
            .rposition(|character| *character == ' ')
    {
        let space = start + relative_space;
        if space > start {
            end = space;
        }
    }
    (start, end)
}

fn window_score(start: usize, end: usize, terms: &[WeightedTerm], spans: &[Span]) -> f64 {
    terms
        .iter()
        .enumerate()
        .filter(|(term, _)| {
            spans
                .iter()
                .any(|span| span.term == *term && span.start >= start && span.end <= end)
        })
        .map(|(_, term)| term.weight)
        .sum()
}

fn make_snippet(text: &[char], start: usize, end: usize) -> Snippet {
    Snippet {
        text: text[start..end].iter().collect(),
        clipped_start: start > 0,
        clipped_end: end < text.len(),
    }
}
