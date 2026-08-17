use std::path::Path;
use yams_core::{MAX_CHUNK, MIN_CHUNK, chunk, chunks_for_page, embed_text_for};

#[test]
fn oversized_lines_are_capped_without_losing_text() {
    let source = "x".repeat(5_000);
    let chunks = chunk(&source);

    assert!(chunks.iter().all(|part| part.chars().count() <= MAX_CHUNK));
    assert_eq!(chunks.concat(), source);
}

#[test]
fn short_paragraphs_merge_but_blank_input_stays_empty() {
    let source = std::iter::repeat_n("tiny paragraph.", 40)
        .collect::<Vec<_>>()
        .join("\n\n");
    let chunks = chunk(&source);

    assert!(
        chunks[..chunks.len() - 1]
            .iter()
            .all(|part| part.chars().count() >= MIN_CHUNK)
    );
    assert!(chunk("\n\n   \n").is_empty());
}

#[test]
fn logical_line_endings_produce_the_same_paragraph_chunks() {
    let first = "a".repeat(MIN_CHUNK);
    let second = "b".repeat(MIN_CHUNK);
    let expected = vec![first.clone(), format!("\n\n{second}")];

    for separator in ["\n \n", "\r\n \r\n", "\r \r"] {
        let source = format!("{first}{separator}{second}");
        assert_eq!(chunk(&source), expected);
    }
}

#[test]
fn title_is_prepended_only_to_embedding_input() {
    let text = embed_text_for(Path::new("alpha.md"), "body", Some("Alpha"));

    assert_eq!(text, "Alpha\n\nbody");
}

#[test]
fn a_short_document_stays_one_chunk() {
    assert_eq!(
        chunk("one short line about widgets."),
        ["one short line about widgets."]
    );
}

#[test]
fn multiline_oversized_paragraph_is_split_on_line_boundaries() {
    let source = (0..40)
        .map(|line| format!("line {line} {}", "x".repeat(100)))
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk(&source);

    assert!(chunks.len() > 1);
    assert!(chunks.iter().all(|part| part.chars().count() <= MAX_CHUNK));
    for line in source.lines() {
        assert!(chunks.iter().any(|part| part.contains(line)));
    }
}

#[test]
fn every_representative_shape_respects_the_maximum() {
    let cases = [
        "x".repeat(5_000),
        std::iter::repeat_n("short paragraph", 100)
            .collect::<Vec<_>>()
            .join("\n\n"),
        std::iter::repeat_n("w".repeat(2_500), 3)
            .collect::<Vec<_>>()
            .join("\n\n"),
        format!("short.\n\n{}", "q".repeat(4_000)),
    ];

    for source in cases {
        assert!(
            chunk(&source)
                .iter()
                .all(|part| part.chars().count() <= MAX_CHUNK)
        );
    }
}

#[test]
fn unicode_hard_wrap_uses_character_boundaries() {
    let source = "🍊".repeat(MAX_CHUNK + 37);
    let chunks = chunk(&source);

    assert!(chunks.iter().all(|part| part.chars().count() <= MAX_CHUNK));
    assert_eq!(chunks.concat(), source);
}

#[test]
fn exact_multibyte_minimum_and_maximum_boundaries_are_preserved() {
    let minimum = "🍊".repeat(MIN_CHUNK);
    let maximum = "🍊".repeat(MAX_CHUNK);

    assert_eq!(chunk(&minimum), [minimum]);
    assert_eq!(chunk(&maximum), [maximum]);
}

#[test]
fn a_short_prefix_is_extended_from_the_following_long_piece() {
    let source = format!("{}\n\n{}", "p".repeat(300), "q".repeat(1_300));
    let chunks = chunk(&source);

    assert!(
        chunks[..chunks.len() - 1]
            .iter()
            .all(|part| part.chars().count() >= 400)
    );
    assert_eq!(
        nonblank_characters(&chunks.concat()),
        nonblank_characters(&source)
    );
}

#[test]
fn oversized_line_flush_retains_its_single_line_separator() {
    let source = format!("{}\n{}", "a".repeat(1_000), "b".repeat(500));
    let chunks = chunk(&source);

    assert_eq!(
        chunks,
        ["a".repeat(1_000), format!("\n{}", "b".repeat(500))]
    );
    assert_eq!(chunks.concat(), source);
    assert!(chunks.iter().all(|part| part.chars().count() <= MAX_CHUNK));
}

#[test]
fn short_prefix_topping_up_from_a_line_preserves_one_separator() {
    let source = format!("{}\n{}", "a".repeat(300), "b".repeat(MAX_CHUNK));
    let chunks = chunk(&source);

    assert_eq!(
        chunks[0],
        format!("{}\n{}", "a".repeat(300), "b".repeat(899))
    );
    assert_eq!(chunks[1], "b".repeat(301));
    assert_eq!(chunks.concat(), source);
    assert!(chunks.iter().all(|part| part.chars().count() <= MAX_CHUNK));
}

#[test]
fn a_maximum_line_after_a_separator_is_split_without_underflow() {
    let source = format!("{}\n{}", "a".repeat(MIN_CHUNK), "b".repeat(MAX_CHUNK));
    let chunks = chunk(&source);

    assert_eq!(chunks.concat(), source);
    assert!(chunks.iter().all(|part| part.chars().count() <= MAX_CHUNK));
    assert_eq!(chunks[1].chars().count(), MAX_CHUNK);
}

#[test]
fn dense_short_lines_preserve_the_normalized_text_and_chunk_bounds() {
    let source = std::iter::repeat_n("x", MAX_CHUNK + MIN_CHUNK)
        .collect::<Vec<_>>()
        .join("\n");
    let chunks = chunk(&source);

    assert_eq!(chunks.concat(), source);
    assert!(chunks.iter().all(|part| part.chars().count() <= MAX_CHUNK));
    assert!(
        chunks[..chunks.len() - 1]
            .iter()
            .all(|part| part.chars().count() >= MIN_CHUNK)
    );
}

#[test]
fn chunks_for_page_removes_frontmatter_and_keeps_display_text_unmodified() {
    let source = "---\ntitle: Alpha heading\nstatus: current\n---\n\nbody with a distinct phrase";
    let chunks = chunks_for_page(Path::new("ignored-name.md"), source).unwrap();

    assert_eq!(chunks.len(), 1);
    assert_eq!(chunks[0].ordinal, 0);
    assert_eq!(chunks[0].text, "body with a distinct phrase");
    assert_eq!(
        chunks[0].embed_text,
        "Alpha heading\n\nbody with a distinct phrase"
    );
}

#[test]
fn page_chunks_have_stable_ordinals_and_title_in_every_embedding() {
    let body = std::iter::repeat_n("tiny paragraph.", 80)
        .collect::<Vec<_>>()
        .join("\n\n");
    let source = format!("---\ntitle: Stable title\n---\n\n{body}");
    let chunks = chunks_for_page(Path::new("fallback-name.md"), &source).unwrap();

    assert!(chunks.len() > 1);
    for (ordinal, chunk) in chunks.iter().enumerate() {
        assert_eq!(chunk.ordinal, ordinal as u32);
        assert_eq!(chunk.embed_text, format!("Stable title\n\n{}", chunk.text));
        assert!(!chunk.text.starts_with("Stable title\n\n"));
    }
}

#[test]
fn embedding_titles_use_a_unicode_safe_compatibility_cap() {
    let title = "🍊".repeat(201);
    let embed_text = embed_text_for(Path::new("fallback-name.md"), "body", Some(&title));

    let (head, body) = embed_text.split_once("\n\n").unwrap();
    assert_eq!(head.chars().count(), 200);
    assert_eq!(body, "body");
}

#[test]
fn embedding_uses_the_filename_when_no_title_is_provided() {
    assert_eq!(
        embed_text_for(Path::new("fallback-name.md"), "body", None),
        "fallback name\n\nbody"
    );
}

fn nonblank_characters(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}
