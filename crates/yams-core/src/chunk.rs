use std::path::Path;

use thiserror::Error;

use crate::{
    frontmatter::{MAX_TITLE_CHARS, filename_title, truncate_chars},
    parse_frontmatter, title_for,
};

/// Small chunks do not carry enough retrieval context on their own.
pub const MIN_CHUNK: usize = 400;
/// Large chunks blend together unrelated retrieval topics.
pub const MAX_CHUNK: usize = 1_200;

/// One display chunk and the text used to embed it.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Chunk {
    pub ordinal: u32,
    pub text: String,
    pub embed_text: String,
}

/// Errors returned while turning one parsed page into chunks.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum ChunkError {
    #[error("page contains more chunks than the index can represent")]
    TooManyChunks,
}

/// Splits text into bounded paragraphs suitable for retrieval.
pub fn chunk(text: &str) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut pending = String::new();

    for (paragraph_index, paragraph) in paragraphs(text).iter().enumerate() {
        for (line_index, line) in paragraph.split('\n').enumerate() {
            let mut first_part = true;
            for_each_hard_wrap(line, |part| {
                let boundary = if first_part && line_index > 0 {
                    Boundary::Line
                } else if first_part && paragraph_index > 0 {
                    Boundary::Paragraph
                } else {
                    Boundary::None
                };
                append_piece(&mut chunks, &mut pending, boundary, part);
                first_part = false;
            });
        }
    }

    if !pending.is_empty() {
        chunks.push(pending);
    }

    chunks
}

#[derive(Clone, Copy)]
enum Boundary {
    None,
    Line,
    Paragraph,
}

impl Boundary {
    fn text(self) -> &'static str {
        match self {
            Self::None => "",
            Self::Line => "\n",
            Self::Paragraph => "\n\n",
        }
    }
}

fn append_piece(chunks: &mut Vec<String>, pending: &mut String, boundary: Boundary, text: &str) {
    let mut boundary = boundary.text();
    let mut remaining = text;

    while !remaining.is_empty() {
        let available = MAX_CHUNK - char_count(pending);
        let boundary_len = char_count(boundary);

        if boundary_len > available {
            chunks.push(std::mem::take(pending));
            continue;
        }

        pending.push_str(boundary);
        boundary = "";
        let available_for_text = available - boundary_len;
        let (prefix, suffix) = split_prefix_chars(remaining, available_for_text);
        pending.push_str(prefix);
        remaining = suffix;

        if char_count(pending) >= MIN_CHUNK {
            chunks.push(std::mem::take(pending));
        }
    }
}

/// Constructs the exact input sent to the embedding model for a display chunk.
pub fn embed_text_for(path: &Path, chunk_text: &str, title: Option<&str>) -> String {
    let head = title
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| filename_title(path));
    let head = truncate_chars(&head, MAX_TITLE_CHARS);

    format!("{head}\n\n{chunk_text}")
}

/// Parses a page and assigns stable, zero-based chunk ordinals.
pub fn chunks_for_page(path: &Path, source: &str) -> Result<Vec<Chunk>, ChunkError> {
    let page = parse_frontmatter(source);
    let title = title_for(path, &page.fields);

    chunk(&page.body)
        .into_iter()
        .enumerate()
        .map(|(ordinal, text)| {
            let ordinal = u32::try_from(ordinal).map_err(|_| ChunkError::TooManyChunks)?;
            let embed_text = embed_text_for(path, &text, Some(&title));
            Ok(Chunk {
                ordinal,
                text,
                embed_text,
            })
        })
        .collect()
}

fn paragraphs(text: &str) -> Vec<String> {
    let mut paragraphs = Vec::new();
    let mut lines = Vec::new();

    for line in logical_lines(text) {
        if line.trim().is_empty() {
            push_paragraph(&mut paragraphs, &mut lines);
        } else {
            lines.push(line);
        }
    }
    push_paragraph(&mut paragraphs, &mut lines);

    paragraphs
}

fn logical_lines(text: &str) -> Vec<&str> {
    let mut lines = Vec::new();
    let mut start = 0;
    let mut characters = text.char_indices().peekable();

    while let Some((offset, character)) = characters.next() {
        if matches!(character, '\n' | '\r') {
            lines.push(&text[start..offset]);
            start = if character == '\r' && matches!(characters.peek(), Some((_, '\n'))) {
                let (newline_offset, _) = characters.next().expect("peeked a newline");
                newline_offset + 1
            } else {
                offset + 1
            };
        }
    }

    if start < text.len() {
        lines.push(&text[start..]);
    }

    lines
}

fn push_paragraph(paragraphs: &mut Vec<String>, lines: &mut Vec<&str>) {
    if lines.is_empty() {
        return;
    }

    let paragraph = lines.join("\n");
    let trimmed = paragraph.trim();
    if !trimmed.is_empty() {
        paragraphs.push(trimmed.to_owned());
    }
    lines.clear();
}

fn for_each_hard_wrap(line: &str, mut visit: impl FnMut(&str)) {
    if char_count(line) <= MAX_CHUNK {
        visit(line);
        return;
    }

    let mut start = 0;
    for (index, (offset, _)) in line.char_indices().enumerate() {
        if index > 0 && index % MAX_CHUNK == 0 {
            visit(&line[start..offset]);
            start = offset;
        }
    }
    visit(&line[start..]);
}

fn char_count(text: &str) -> usize {
    text.chars().count()
}

fn split_prefix_chars(text: &str, count: usize) -> (&str, &str) {
    let end = text
        .char_indices()
        .nth(count)
        .map_or(text.len(), |(offset, _)| offset);
    text.split_at(end)
}
