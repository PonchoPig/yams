use std::{collections::BTreeMap, path::Path};

pub(crate) const MAX_TITLE_CHARS: usize = 200;

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ParsedPage {
    pub fields: BTreeMap<String, String>,
    pub body: String,
}

#[derive(Clone, Copy)]
struct Line<'a> {
    content: &'a str,
    separator_end: usize,
}

impl<'a> Line<'a> {
    fn is_blank(self) -> bool {
        self.content.trim().is_empty()
    }
}

/// Splits text with Rust's native `str::lines` rules, then extends those rules
/// with the remaining separators that Python's `str.splitlines()` recognizes.
/// Every line keeps its source offsets so a valid parse can return the body
/// without normalizing its separators.
fn lines_with_separators(source: &str) -> Vec<Line<'_>> {
    let mut lines = Vec::new();
    let mut cursor = 0;

    for native_line in source.lines() {
        debug_assert!(source[cursor..].starts_with(native_line));
        let native_end = cursor + native_line.len();
        let native_separator_len = if source[native_end..].starts_with("\r\n") {
            2
        } else if source[native_end..].starts_with('\n') {
            1
        } else {
            0
        };

        append_explicitly_separated_lines(
            source,
            cursor,
            native_end,
            native_separator_len,
            &mut lines,
        );
        cursor = native_end + native_separator_len;
    }

    lines
}

fn append_explicitly_separated_lines<'a>(
    source: &'a str,
    start: usize,
    end: usize,
    native_separator_len: usize,
    lines: &mut Vec<Line<'a>>,
) {
    let mut line_start = start;
    let mut index = start;

    while index < end {
        let ch = source[index..]
            .chars()
            .next()
            .expect("index is within a UTF-8 string");
        if explicit_separator_len(ch).is_some() {
            let separator_end = index + ch.len_utf8();
            lines.push(Line {
                content: &source[line_start..index],
                separator_end,
            });
            line_start = separator_end;
            index = separator_end;
        } else {
            index += ch.len_utf8();
        }
    }

    if line_start < end || native_separator_len > 0 {
        lines.push(Line {
            content: &source[line_start..end],
            separator_end: end + native_separator_len,
        });
    }
}

fn explicit_separator_len(ch: char) -> Option<usize> {
    matches!(
        ch,
        '\r' | '\u{000b}'
            | '\u{000c}'
            | '\u{001c}'
            | '\u{001d}'
            | '\u{001e}'
            | '\u{0085}'
            | '\u{2028}'
            | '\u{2029}'
    )
    .then_some(ch.len_utf8())
}

fn field_line(line: &str) -> Option<(&str, &str)> {
    let (key, value) = line.split_once(':')?;
    let first = key.chars().next()?;
    let legal = first.is_ascii_alphabetic() || first == '_';
    let rest_legal = key
        .chars()
        .skip(1)
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '.' | '-'));
    (legal && rest_legal).then_some((key, value.trim()))
}

fn unquote(value: &str) -> &str {
    match value.as_bytes() {
        [b'\'', .., b'\''] | [b'"', .., b'"'] if value.len() >= 2 => &value[1..value.len() - 1],
        _ => value,
    }
}

/// Parses only a leading, unambiguous scalar frontmatter block.
pub fn parse_frontmatter(source: &str) -> ParsedPage {
    let parse_source = source.strip_prefix('\u{feff}').unwrap_or(source);
    let lines = lines_with_separators(parse_source);

    if lines.first().is_none_or(|line| line.content != "---") {
        return ParsedPage::default_with_source(source);
    }

    let Some(closing_index) = lines
        .iter()
        .enumerate()
        .skip(1)
        .find_map(|(index, line)| (line.content == "---").then_some(index))
    else {
        return ParsedPage::default_with_source(source);
    };

    let mut fields = BTreeMap::new();
    for line in &lines[1..closing_index] {
        if line.is_blank() {
            return ParsedPage::default_with_source(source);
        }
        if line.content.starts_with([' ', '\t']) {
            if fields.is_empty() {
                return ParsedPage::default_with_source(source);
            }
            continue;
        }
        let Some((key, value)) = field_line(line.content) else {
            return ParsedPage::default_with_source(source);
        };
        fields.insert(key.to_owned(), unquote(value).to_owned());
    }

    if fields.is_empty() {
        return ParsedPage::default_with_source(source);
    }

    let mut body_start = lines[closing_index].separator_end;
    for line in &lines[closing_index + 1..] {
        if !line.content.is_empty() {
            break;
        }
        body_start = line.separator_end;
    }

    ParsedPage {
        fields,
        body: parse_source[body_start..].to_owned(),
    }
}

impl ParsedPage {
    fn default_with_source(source: &str) -> Self {
        Self {
            fields: BTreeMap::new(),
            body: source.to_owned(),
        }
    }
}

/// Returns the most useful page title, capped without splitting UTF-8.
pub fn title_for(path: &Path, fields: &BTreeMap<String, String>) -> String {
    let title = fields
        .get("title")
        .filter(|value| !value.is_empty())
        .or_else(|| fields.get("name").filter(|value| !value.is_empty()))
        .cloned()
        .unwrap_or_else(|| filename_title(path));

    truncate_chars(&title, MAX_TITLE_CHARS)
}

pub(crate) fn filename_title(path: &Path) -> String {
    path.file_stem()
        .unwrap_or_default()
        .to_string_lossy()
        .replace(['-', '_'], " ")
}

pub(crate) fn truncate_chars(text: &str, maximum: usize) -> String {
    text.char_indices()
        .nth(maximum)
        .map_or_else(|| text.to_owned(), |(end, _)| text[..end].to_owned())
}
