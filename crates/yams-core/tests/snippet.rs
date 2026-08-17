use yams_core::{
    SNIPPET_GAIN, SNIPPET_WIDTH, SnippetError, SnippetStatistics, TermFrequency, WeightedTerm,
    query_terms, snippet, term_weights,
};

fn term(name: &str, weight: f64) -> WeightedTerm {
    WeightedTerm {
        term: name.to_owned(),
        weight,
    }
}

fn neutral_filler(repetitions: usize) -> String {
    "ordinary prose keeps this synthetic passage neutral. ".repeat(repetitions)
}

#[test]
fn constants_match_the_query_relevant_snippet_contract() {
    assert_eq!(SNIPPET_WIDTH, 280);
    assert_eq!(SNIPPET_GAIN, 0.75);
}

#[test]
fn query_terms_match_the_lexical_ascii_and_stopword_rules() {
    assert_eq!(
        query_terms("A blue_widget, x and blue-widget; WHAT blue_widget?"),
        ["blue_widget", "blue", "widget"]
    );
    assert_eq!(query_terms("where is this"), ["where", "is", "this"]);
    assert!(query_terms("x - !").is_empty());
    assert_eq!(query_terms("naïve café Δelta"), ["naïve", "café", "δelta"]);
    assert_eq!(query_terms("İstanbul"), ["istanbul"]);
}

#[test]
fn rarity_weights_use_matching_chunks_and_the_normalized_formula() {
    let stats = SnippetStatistics {
        total_chunks: 9,
        frequencies: vec![
            TermFrequency {
                term: "orchidneedle".to_owned(),
                matching_chunks: 0,
            },
            TermFrequency {
                term: "sharedword".to_owned(),
                matching_chunks: 9,
            },
        ],
    };

    let weights = term_weights(&stats).unwrap();
    assert_eq!(weights[0].term, "orchidneedle");
    assert!((weights[0].weight - 1.0).abs() < 1e-12);
    let expected = (1.0_f64 + 9.0 / 10.0).ln() / 10.0_f64.ln();
    assert!((weights[1].weight - expected).abs() < 1e-12);
    assert!(weights[0].weight > weights[1].weight);
}

#[test]
fn an_empty_index_has_no_weights_and_impossible_frequencies_are_refused() {
    let empty = SnippetStatistics {
        total_chunks: 0,
        frequencies: vec![TermFrequency {
            term: "unused".to_owned(),
            matching_chunks: 0,
        }],
    };
    assert!(term_weights(&empty).unwrap().is_empty());

    let invalid = SnippetStatistics {
        total_chunks: 2,
        frequencies: vec![TermFrequency {
            term: "impossible".to_owned(),
            matching_chunks: 3,
        }],
    };
    assert_eq!(
        term_weights(&invalid),
        Err(SnippetError::FrequencyExceedsTotal {
            term: "impossible".to_owned(),
            matching_chunks: 3,
            total_chunks: 2,
        })
    );
}

#[test]
fn direct_statistics_reject_terms_outside_the_lexical_token_grammar() {
    for invalid_term in ["x", "two-words"] {
        let stats = SnippetStatistics {
            total_chunks: 1,
            frequencies: vec![TermFrequency {
                term: invalid_term.to_owned(),
                matching_chunks: 0,
            }],
        };
        assert_eq!(
            term_weights(&stats),
            Err(SnippetError::InvalidTerm {
                term: invalid_term.to_owned(),
            })
        );
    }
}

#[test]
fn a_short_passage_is_collapsed_and_returned_without_clipping() {
    let result = snippet(
        "  compact\nsynthetic\tpassage  ",
        &[term("compact", 1.0)],
        280,
        0.75,
    )
    .unwrap();

    assert_eq!(result.text, "compact synthetic passage");
    assert!(!result.clipped_start);
    assert!(!result.clipped_end);
}

#[test]
fn python_information_separators_collapse_to_visible_word_boundaries() {
    for separator in ['\u{001c}', '\u{001d}', '\u{001e}', '\u{001f}'] {
        let text = format!("left{separator}right");
        let result = snippet(&text, &[], 280, 0.75).unwrap();
        assert_eq!(
            result.text, "left right",
            "separator U+{:04X}",
            separator as u32
        );
    }
}

#[test]
fn a_rare_term_at_the_end_displaces_the_head_and_sets_directional_flags() {
    let ending = " orchidneedle closes";
    let suffix = format!("{}{ending}", "z".repeat(80 - ending.len()));
    assert_eq!(suffix.chars().count(), 80, "premise: exact tail window");
    let text = format!("{}{suffix}", neutral_filler(10));
    let result = snippet(&text, &[term("orchidneedle", 1.0)], 80, 0.75).unwrap();

    assert!(result.text.contains("orchidneedle"));
    assert!(result.clipped_start);
    assert!(!result.clipped_end);
    assert!(result.text.chars().count() <= 80);
}

#[test]
fn no_literal_span_keeps_the_head_even_with_maximum_rarity() {
    let text = format!("opening words. {}", neutral_filler(8));
    let result = snippet(&text, &[term("absenttoken", 1.0)], 70, 0.0).unwrap();

    assert!(result.text.starts_with("opening words."));
    assert!(!result.clipped_start);
    assert!(result.clipped_end);
}

#[test]
fn weighted_distinct_coverage_beats_repeated_occurrences() {
    let repeated = "cobalt ".repeat(16);
    let text = format!(
        "{}{} {}cobalt amber together {}",
        neutral_filler(3),
        repeated,
        neutral_filler(3),
        neutral_filler(3)
    );
    let result = snippet(&text, &[term("cobalt", 0.5), term("amber", 0.5)], 70, 0.1).unwrap();

    assert!(result.text.contains("amber"));
}

#[test]
fn the_best_qualifying_window_wins_independently_of_query_order() {
    let text = format!(
        "{}cobalt alone here. {}cobalt amber together. {}",
        neutral_filler(3),
        neutral_filler(4),
        neutral_filler(3)
    );
    let forward = snippet(&text, &[term("cobalt", 0.8), term("amber", 0.7)], 70, 0.75).unwrap();
    let reverse = snippet(&text, &[term("amber", 0.7), term("cobalt", 0.8)], 70, 0.75).unwrap();

    assert!(forward.text.contains("cobalt amber"));
    assert_eq!(forward, reverse);
}

#[test]
fn equal_scoring_candidates_choose_the_earlier_source_position() {
    let text = format!(
        "{}cinnabar appears here. {}malachite appears there. {}",
        neutral_filler(3),
        neutral_filler(4),
        neutral_filler(3)
    );
    let result = snippet(
        &text,
        &[term("cinnabar", 1.0), term("malachite", 1.0)],
        70,
        0.5,
    )
    .unwrap();

    assert!(result.text.contains("cinnabar"));
    assert!(!result.text.contains("malachite"));
}

#[test]
fn the_gain_bar_is_strict_and_fixed_to_the_head() {
    let text = format!(
        "headmark starts here. {}tailmark appears later. {}",
        neutral_filler(5),
        neutral_filler(3)
    );
    let weights = [term("headmark", 0.5), term("tailmark", 0.7)];

    let tied = snippet(&text, &weights, 70, 0.2).unwrap();
    assert!(tied.text.starts_with("headmark"));

    let moved = snippet(&text, &weights, 70, 0.19).unwrap();
    assert!(moved.text.contains("tailmark"));
}

#[test]
fn literal_matching_is_case_insensitive_and_uses_ascii_identifier_boundaries() {
    let embedded = format!(
        "{}prefixorchidneedlesuffix {}",
        neutral_filler(5),
        neutral_filler(3)
    );
    let embedded_result = snippet(&embedded, &[term("orchidneedle", 1.0)], 70, 0.5).unwrap();
    assert!(!embedded_result.clipped_start);

    let standalone = format!(
        "{}ORCHIDNEEDLE is standalone {}",
        neutral_filler(5),
        neutral_filler(3)
    );
    let standalone_result = snippet(&standalone, &[term("orchidneedle", 1.0)], 70, 0.5).unwrap();
    assert!(standalone_result.text.contains("ORCHIDNEEDLE"));
    assert!(standalone_result.clipped_start);
}

#[test]
fn windows_snap_to_spaces_and_never_exceed_unicode_scalar_width() {
    let text = format!(
        "{}family 👩‍🔬 é orchidneedle closes {}",
        neutral_filler(6),
        neutral_filler(4)
    );
    let result = snippet(&text, &[term("orchidneedle", 1.0)], 67, 0.5).unwrap();

    assert!(result.text.contains("orchidneedle"));
    assert!(result.text.chars().count() <= 67);
    assert!(!result.text.starts_with(' '));
    assert!(!result.text.ends_with(' '));
    // Width is Unicode-scalar based. Grapheme/ZWJ preservation is deliberately
    // not asserted: that documented compatibility gap remains out of scope.
    assert!(std::str::from_utf8(result.text.as_bytes()).is_ok());
}

#[test]
fn invalid_window_configuration_and_weights_are_refused() {
    assert_eq!(snippet("text", &[], 0, 0.5), Err(SnippetError::ZeroWidth));
    assert_eq!(
        snippet("text", &[], 20, f64::NAN),
        Err(SnippetError::InvalidGain)
    );
    assert_eq!(
        snippet("text", &[], 20, 1.01),
        Err(SnippetError::InvalidGain)
    );
    assert_eq!(
        snippet("text", &[term("unsafe", f64::INFINITY)], 20, 0.5),
        Err(SnippetError::InvalidWeight {
            term: "unsafe".to_owned(),
        })
    );
}
