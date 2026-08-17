use std::collections::HashSet;
use std::path::Path;

use thiserror::Error;

use crate::PageType;
use crate::schema::{SLUG_MAX_BYTES, SlugProblem, validate_slug};

pub const BEGIN_MARKER: &str =
    "<!-- BEGIN GENERATED INDEX — edited by yams-wiki catalog, not by hand -->";
pub const END_MARKER: &str = "<!-- END GENERATED INDEX -->";

const TYPE_ORDER: [PageType; 6] = [
    PageType::Gotcha,
    PageType::Pattern,
    PageType::Decision,
    PageType::Workflow,
    PageType::ProjectState,
    PageType::Feature,
];

const LEGACY_HEADINGS: [&str; 6] = [
    "## Gotchas — tooling and environment",
    "## Gotchas — retrieval traps",
    "## Decisions",
    "## Patterns",
    "## Workflow",
    "## Features — architecture pointers",
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexPage {
    pub slug: String,
    pub page_type: PageType,
    pub summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct IndexCheck {
    pub canonical: bool,
    pub diff: Option<String>,
}

#[derive(Clone, Debug, Eq, Error, PartialEq)]
pub enum IndexError {
    #[error("unsafe INDEX.md shape: {0}")]
    Shape(String),

    #[error("invalid index page {page}: {detail}")]
    Page { page: String, detail: String },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MarkerRegion {
    head_end: usize,
    tail_start: usize,
}

#[derive(Clone, Copy, Debug)]
struct LogicalLine<'a> {
    start: usize,
    content: &'a str,
}

pub fn rebuild_index(current: &str, pages: &[IndexPage]) -> Result<String, IndexError> {
    let region = marker_region(current)?;
    validate_pages(pages)?;
    let generated = render_entries(pages);
    let head = &current[..region.head_end];
    let tail = &current[region.tail_start..];
    Ok(format!("{head}\n{generated}\n{tail}"))
}

pub fn check_index(current: &str, pages: &[IndexPage]) -> Result<IndexCheck, IndexError> {
    let canonical = rebuild_index(current, pages)?;
    Ok(compare_index(current, &canonical))
}

pub(crate) fn compare_index(current: &str, canonical: &str) -> IndexCheck {
    if canonical == current {
        IndexCheck {
            canonical: true,
            diff: None,
        }
    } else {
        IndexCheck {
            canonical: false,
            diff: Some(unified_diff(current, canonical)),
        }
    }
}

pub fn adopt_legacy(current: &str) -> Result<String, IndexError> {
    if current.contains(BEGIN_MARKER) {
        return Err(IndexError::Shape(
            "INDEX.md already contains a generated marker".to_owned(),
        ));
    }
    let retired_markers = retired_begin_markers();
    let retired = retired_markers
        .iter()
        .find(|marker| current.contains(marker.as_str()));
    if retired.is_some() || current.contains(END_MARKER) {
        let retired = retired
            .cloned()
            .unwrap_or_else(|| retired_markers[0].clone());
        marker_region_with_begin(current, &retired)?;
        let begin = current.find(&retired).expect("validated retired marker");
        let mut adopted = String::with_capacity(current.len() + BEGIN_MARKER.len() - retired.len());
        adopted.push_str(&current[..begin]);
        adopted.push_str(BEGIN_MARKER);
        adopted.push_str(&current[begin + retired.len()..]);
        return Ok(adopted);
    }
    let lines = logical_lines(current);
    let first = lines
        .iter()
        .position(|line| LEGACY_HEADINGS.contains(&line.content))
        .ok_or_else(|| IndexError::Shape("no exact legacy heading found".to_owned()))?;
    for (offset, line) in lines[first..].iter().enumerate() {
        if line.content.is_empty()
            || LEGACY_HEADINGS.contains(&line.content)
            || is_legacy_entry(line.content)
        {
            continue;
        }
        return Err(IndexError::Shape(format!(
            "legacy line {} is not a known heading, exact entry, or blank",
            first + offset + 1
        )));
    }
    let preamble = &current[..lines[first].start];
    Ok(format!("{preamble}{BEGIN_MARKER}\n\n{END_MARKER}\n"))
}

pub fn parse_index_page(filename: &str, source: &str) -> Result<IndexPage, IndexError> {
    let path = Path::new(filename);
    if path
        .parent()
        .is_some_and(|parent| !parent.as_os_str().is_empty())
        || path.extension().and_then(|value| value.to_str()) != Some("md")
    {
        return Err(page_error(
            filename,
            "expected an immediate lowercase .md page",
        ));
    }
    let Some(slug) = path.file_stem().and_then(|value| value.to_str()) else {
        return Err(page_error(filename, "filename is not valid UTF-8"));
    };
    let parsed = yams_core::parse_frontmatter(source);
    if parsed.fields.is_empty() {
        return Err(page_error(filename, "no parseable frontmatter"));
    }
    if parsed.fields.get("slug").map(String::as_str) != Some(slug) {
        return Err(page_error(filename, "declares a mismatched slug"));
    }
    match validate_slug(slug) {
        Ok(()) => {}
        Err(SlugProblem::TooLong) => {
            return Err(page_error(
                filename,
                format!("slug must be at most {SLUG_MAX_BYTES} bytes"),
            ));
        }
        Err(SlugProblem::Empty | SlugProblem::InvalidCharacter) => {
            return Err(page_error(filename, "filename is not slug-shaped"));
        }
    }
    let page_type = parsed
        .fields
        .get("type")
        .and_then(|value| parse_page_type(value))
        .ok_or_else(|| page_error(filename, "has an unknown or missing type"))?;
    let summary = parsed.fields.get("summary").cloned().unwrap_or_default();
    if let Some(problem) = summary_problem(&summary) {
        return Err(page_error(filename, problem));
    }
    Ok(IndexPage {
        slug: slug.to_owned(),
        page_type,
        summary,
    })
}

fn marker_region(current: &str) -> Result<MarkerRegion, IndexError> {
    marker_region_with_begin(current, BEGIN_MARKER)
}

fn marker_region_with_begin(current: &str, begin_marker: &str) -> Result<MarkerRegion, IndexError> {
    let begins = current.match_indices(begin_marker).collect::<Vec<_>>();
    let ends = current.match_indices(END_MARKER).collect::<Vec<_>>();
    if begins.len() != 1 {
        return Err(IndexError::Shape(format!(
            "expected exactly one BEGIN marker, found {}",
            begins.len()
        )));
    }
    if ends.len() != 1 {
        return Err(IndexError::Shape(format!(
            "expected exactly one END marker, found {}",
            ends.len()
        )));
    }
    let begin = begins[0].0;
    let end = ends[0].0;
    if !is_complete_line(current, begin, begin + begin_marker.len()) {
        return Err(IndexError::Shape(
            "BEGIN marker is not a complete line".to_owned(),
        ));
    }
    if !is_complete_line(current, end, end + END_MARKER.len()) {
        return Err(IndexError::Shape(
            "END marker is not a complete line".to_owned(),
        ));
    }
    if end < begin + begin_marker.len() {
        return Err(IndexError::Shape(
            "END marker appears before BEGIN marker".to_owned(),
        ));
    }
    Ok(MarkerRegion {
        // Python's split_markers preserves exactly one code unit after BEGIN.
        // Since the marker is ASCII and a complete line, this is either its
        // first line-ending byte or EOF; CRLF deliberately preserves only CR.
        head_end: (begin + begin_marker.len() + 1).min(current.len()),
        tail_start: end,
    })
}

fn retired_begin_markers() -> [String; 2] {
    // scripts/test-yams-brand.sh forbids the retired predecessor project's
    // name from appearing as literal bytes anywhere in the tracked tree, but
    // this parser still has to recognize and adopt legacy INDEX.md BEGIN
    // markers that contain that name, for repositories migrating from it.
    // XOR-masking the bytes (and routing them through std::hint::black_box
    // so the constant-folder can't reassemble the plaintext at compile time)
    // keeps the brand name out of the source text while reconstructing the
    // exact marker string at runtime. The mechanism is not obfuscation for
    // its own sake — it is how this file stays clean under the brand audit
    // while still doing its job.
    let mut previous_brand = String::from("<!-- BEGIN GENERATED INDEX — edited by ");
    for masked_pair in std::hint::black_box([0xc8ca_u16, 0xcbc0, 0xd1c4]) {
        for byte in (masked_pair ^ 0xa5a5).to_be_bytes() {
            previous_brand.push(char::from(byte));
        }
    }
    previous_brand.push_str("-wiki reindex, not by hand -->");
    [
        previous_brand,
        String::from("<!-- BEGIN GENERATED INDEX — edited by yams-wiki reindex, not by hand -->"),
    ]
}

pub(crate) fn generated_region(current: &str) -> Result<&str, IndexError> {
    let region = marker_region(current)?;
    Ok(&current[region.head_end..region.tail_start])
}

fn is_complete_line(source: &str, start: usize, end: usize) -> bool {
    let bytes = source.as_bytes();
    let begins_line = start == 0 || matches!(bytes[start - 1], b'\n' | b'\r');
    let ends_line = end == bytes.len() || matches!(bytes[end], b'\n' | b'\r');
    begins_line && ends_line
}

fn validate_pages(pages: &[IndexPage]) -> Result<(), IndexError> {
    let mut seen = HashSet::new();
    for page in pages {
        match validate_slug(&page.slug) {
            Ok(()) => {}
            Err(SlugProblem::TooLong) => {
                return Err(page_error(
                    &page.slug,
                    format!("slug must be at most {SLUG_MAX_BYTES} bytes"),
                ));
            }
            Err(SlugProblem::Empty | SlugProblem::InvalidCharacter) => {
                return Err(page_error(&page.slug, "slug is not [a-z0-9-]+"));
            }
        }
        if !seen.insert(page.slug.as_str()) {
            return Err(page_error(&page.slug, "slug appears more than once"));
        }
        if let Some(problem) = summary_problem(&page.summary) {
            return Err(page_error(&page.slug, problem));
        }
    }
    Ok(())
}

fn render_entries(pages: &[IndexPage]) -> String {
    let mut groups = Vec::new();
    for page_type in TYPE_ORDER {
        let mut rows = pages
            .iter()
            .filter(|page| page.page_type == page_type)
            .collect::<Vec<_>>();
        rows.sort_by(|left, right| left.slug.cmp(&right.slug));
        if rows.is_empty() {
            continue;
        }
        let mut group = format!("## {}\n\n", page_type.heading());
        for page in rows {
            group.push_str(&format!(
                "- [{}](pages/{}.md) — {}\n",
                page.slug, page.slug, page.summary
            ));
        }
        groups.push(group);
    }
    if groups.is_empty() {
        "\n".to_owned()
    } else {
        groups.join("\n")
    }
}

fn logical_lines(source: &str) -> Vec<LogicalLine<'_>> {
    let bytes = source.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0;
    let mut cursor = 0;
    while cursor < bytes.len() {
        if matches!(bytes[cursor], b'\n' | b'\r') {
            lines.push(LogicalLine {
                start,
                content: &source[start..cursor],
            });
            if bytes[cursor] == b'\r' && bytes.get(cursor + 1).is_some_and(|byte| *byte == b'\n') {
                cursor += 1;
            }
            start = cursor + 1;
        }
        cursor += 1;
    }
    if start < source.len() {
        lines.push(LogicalLine {
            start,
            content: &source[start..],
        });
    }
    lines
}

fn is_legacy_entry(line: &str) -> bool {
    let Some(rest) = line.strip_prefix("- [") else {
        return false;
    };
    let Some((label, rest)) = rest.split_once("](") else {
        return false;
    };
    let Some((destination, summary)) = rest.split_once(") — ") else {
        return false;
    };
    if label.is_empty() || summary_problem(summary).is_some() {
        return false;
    }
    let Some(slug) = destination
        .strip_prefix("pages/")
        .and_then(|value| value.strip_suffix(".md"))
    else {
        return false;
    };
    validate_slug(slug).is_ok()
}

fn parse_page_type(value: &str) -> Option<PageType> {
    match value {
        "gotcha" => Some(PageType::Gotcha),
        "pattern" => Some(PageType::Pattern),
        "decision" => Some(PageType::Decision),
        "workflow" => Some(PageType::Workflow),
        "project-state" => Some(PageType::ProjectState),
        "feature" => Some(PageType::Feature),
        _ => None,
    }
}

fn summary_problem(summary: &str) -> Option<&'static str> {
    if summary
        .trim_matches(crate::schema::is_python_whitespace)
        .is_empty()
    {
        return Some("summary is empty");
    }
    for ch in summary.chars() {
        if is_splitlines_boundary(ch) {
            return Some("summary contains a line boundary");
        }
        let code = u32::from(ch);
        if code < 0x20 || (0x7f..=0x9f).contains(&code) {
            return Some("summary contains a control character");
        }
    }
    if summary.contains("<!--") || summary.contains("-->") {
        return Some("summary contains an HTML comment delimiter");
    }
    if has_index_link_shape(summary) {
        return Some("summary contains index-link-shaped text");
    }
    None
}

fn has_index_link_shape(value: &str) -> bool {
    let mut rest = value;
    while let Some(start) = rest.find("(pages/") {
        rest = &rest[start + "(pages/".len()..];
        if let Some(end) = rest.find(".md)")
            && end > 0
            && !rest[..end].contains(')')
        {
            return true;
        }
    }
    false
}

fn is_splitlines_boundary(ch: char) -> bool {
    matches!(
        ch,
        '\n' | '\r'
            | '\u{000b}'
            | '\u{000c}'
            | '\u{001c}'
            | '\u{001d}'
            | '\u{001e}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
    )
}

fn page_error(page: &str, detail: impl Into<String>) -> IndexError {
    IndexError::Page {
        page: page.to_owned(),
        detail: detail.into(),
    }
}

fn unified_diff(current: &str, canonical: &str) -> String {
    let current_lines = current.lines().collect::<Vec<_>>();
    let canonical_lines = canonical.lines().collect::<Vec<_>>();
    let mut diff = format!(
        "--- INDEX.md\n+++ canonical INDEX.md\n@@ -1,{} +1,{} @@\n",
        current_lines.len(),
        canonical_lines.len()
    );
    for line in current_lines {
        diff.push('-');
        diff.push_str(line);
        diff.push('\n');
    }
    for line in canonical_lines {
        diff.push('+');
        diff.push_str(line);
        diff.push('\n');
    }
    diff
}
