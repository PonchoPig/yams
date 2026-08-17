use yams_cli::{
    BoundedBuffer, GateEntry, GateVerdict, HitExplanation, ProjectSearchResponse, RenderError,
    SearchExplanation, SearchHit, SearchResponse, Styling, TextOptions, render_all_json,
    render_all_text, render_diagnostic, render_json, render_text,
};
use yams_core::CorpusKind;

fn hit() -> SearchHit {
    SearchHit {
        name: "aerostat".to_owned(),
        path: "/fictional/.agents/memory/aerostat.md".to_owned(),
        score: 0.812_34,
        text: "The complete aerostat maintenance record.".to_owned(),
        snippet: "aerostat maintenance".to_owned(),
        clipped_start: false,
        clipped_end: true,
        corpus: CorpusKind::Shared,
        exact: false,
        status: None,
        explanation: None,
    }
}

fn assert_no_literal_terminal_controls(frame: &str) {
    assert!(!frame.chars().any(|character| {
        character != '\n'
            && matches!(
                character,
                '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}'
            )
    }));
}

#[test]
fn bounded_buffer_stops_growing_at_the_cap() {
    use std::fmt::Write as _;

    let mut buffer = BoundedBuffer::new(8);
    assert!(write!(buffer, "12345678").is_ok());
    assert!(write!(buffer, "9").is_err());
    assert!(buffer.overflowed());
    assert!(
        buffer.into_string().len() <= 8,
        "never retains past the cap"
    );
}

#[test]
fn bounded_buffer_counts_utf8_bytes_and_refuses_a_whole_multibyte_write() {
    use std::fmt::Write as _;

    let mut exact = BoundedBuffer::new(4);
    assert!(write!(exact, "éé").is_ok());
    assert_eq!(exact.into_string(), "éé");

    let mut partial = BoundedBuffer::new(3);
    assert!(write!(partial, "xé").is_ok());
    assert!(write!(partial, "é").is_err());
    assert_eq!(partial.into_string(), "xé");
}

#[test]
fn direct_text_renderer_reports_typed_overflow_without_truncating_utf8() {
    let mut oversized = hit();
    oversized.text = "é".repeat(BoundedBuffer::DIRECT_STREAM_CAP / 2);

    assert_eq!(
        render_text(
            &SearchResponse {
                hits: vec![oversized],
                explanation: None,
            },
            TextOptions::single(true, Styling::Plain),
        ),
        Err(RenderError::OutputLimit)
    );
}

#[test]
fn json_encoding_expansion_is_subject_to_the_direct_stream_cap() {
    let mut encoded = hit();
    encoded.name = "\u{007f}".repeat(BoundedBuffer::DIRECT_STREAM_CAP / 6);
    assert!(encoded.name.len() < BoundedBuffer::DIRECT_STREAM_CAP);
    let rendered = render_json(&SearchResponse {
        hits: vec![encoded],
        explanation: None,
    })
    .unwrap();
    assert!(rendered.len() > BoundedBuffer::DIRECT_STREAM_CAP);
}

#[test]
fn plain_json_is_a_bare_array_with_the_stable_hit_shape() {
    let output = render_json(&SearchResponse {
        hits: vec![hit()],
        explanation: None,
    })
    .unwrap();

    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&output).unwrap(),
        serde_json::json!([{
            "name": "aerostat",
            "path": "/fictional/.agents/memory/aerostat.md",
            "score": 0.8123,
            "text": "The complete aerostat maintenance record.",
            "snippet": "aerostat maintenance",
            "clipped_start": false,
            "clipped_end": true,
            "corpus": "shared",
            "exact": false
        }])
    );
    assert!(output.ends_with('\n'));
    assert!(!output.ends_with("\n\n"));
}

#[test]
fn json_preserves_hostile_controls_unicode_and_an_asserted_status_as_data() {
    let mut hostile = hit();
    hostile.name = "Aérostat\u{1b}[2J\n偽 heading".to_owned();
    hostile.snippet = "naïve\u{009b}2J\t記録".to_owned();
    hostile.status = Some("historical\r\u{1b}]0;title\u{7}".to_owned());

    let output = render_json(&SearchResponse {
        hits: vec![hostile],
        explanation: None,
    })
    .unwrap();
    let hits: serde_json::Value = serde_json::from_str(&output).unwrap();

    assert_eq!(hits[0]["name"], "Aérostat\u{1b}[2J\n偽 heading");
    assert_eq!(hits[0]["snippet"], "naïve\u{009b}2J\t記録");
    assert_eq!(hits[0]["status"], "historical\r\u{1b}]0;title\u{7}");
    assert!(output.contains("\\u001b"));
}

#[test]
fn json_escapes_del_and_c1_without_changing_parsed_values() {
    let del_and_c1 = (0x7f..=0x9f)
        .map(|value| char::from_u32(value).unwrap())
        .collect::<String>();
    let mut hostile = hit();
    hostile.name = format!("name{del_and_c1}");
    hostile.path = "path\u{1b}\u{0080}".to_owned();
    hostile.text = "text\u{7}\u{0081}".to_owned();
    hostile.snippet = "snippet\t\u{0082}".to_owned();
    hostile.status = Some("status\r\u{0083}".to_owned());
    hostile.explanation = Some(HitExplanation {
        dense_rank: Some(1),
        bm25_rank: None,
        rrf_score: Some(0.01),
    });
    let response = SearchResponse {
        hits: vec![hostile.clone()],
        explanation: Some(SearchExplanation {
            query: "query\n\u{0084}".to_owned(),
            applied: true,
            gate: Some(GateVerdict {
                baseline: 0.8,
                min_score: 0.72,
                max_gap: 0.05,
                no_hits: false,
                floor_fired: false,
                top: Some(0.8),
                floor_dropped: vec![GateEntry {
                    path: "gate\u{001b}\u{0085}".to_owned(),
                    score: 0.7,
                }],
                gap_dropped: vec![],
                rescued: vec![],
            }),
        }),
    };

    let output = render_json(&response).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
    let all_output = render_all_json(&[ProjectSearchResponse {
        project: "project\u{0007}\u{0086}".to_owned(),
        hits: vec![hostile],
    }])
    .unwrap();
    let all_parsed: serde_json::Value = serde_json::from_str(&all_output).unwrap();

    for value in 0x7f..=0x9f {
        assert!(output.contains(&format!("\\u{value:04x}")));
    }
    assert_no_literal_terminal_controls(&output);
    assert_no_literal_terminal_controls(&all_output);
    assert_eq!(parsed["hits"][0]["name"], format!("name{del_and_c1}"));
    assert_eq!(parsed["hits"][0]["path"], "path\u{1b}\u{0080}");
    assert_eq!(parsed["hits"][0]["text"], "text\u{7}\u{0081}");
    assert_eq!(parsed["hits"][0]["snippet"], "snippet\t\u{0082}");
    assert_eq!(parsed["hits"][0]["status"], "status\r\u{0083}");
    assert_eq!(parsed["query"], "query\n\u{0084}");
    assert_eq!(
        parsed["gate"]["floor_dropped"][0]["path"],
        "gate\u{001b}\u{0085}"
    );
    assert_eq!(all_parsed[0]["project"], "project\u{0007}\u{0086}");
}

#[test]
fn explain_json_is_an_envelope_with_gate_and_per_hit_rank_signals() {
    let mut explained = hit();
    explained.explanation = Some(HitExplanation {
        dense_rank: Some(2),
        bm25_rank: None,
        rrf_score: Some(0.016_393_442_6),
    });
    let response = SearchResponse {
        hits: vec![explained],
        explanation: Some(SearchExplanation {
            query: "where is the aerostat?".to_owned(),
            applied: true,
            gate: Some(GateVerdict {
                baseline: 0.6841,
                min_score: 0.72,
                max_gap: 0.05,
                no_hits: false,
                floor_fired: true,
                top: None,
                floor_dropped: vec![GateEntry {
                    path: "/fictional/.agents/memory/aerostat.md".to_owned(),
                    score: 0.6841,
                }],
                gap_dropped: vec![],
                rescued: vec![],
            }),
        }),
    };

    let payload: serde_json::Value =
        serde_json::from_str(&render_json(&response).unwrap()).unwrap();

    assert_eq!(payload["query"], "where is the aerostat?");
    assert_eq!(payload["applied"], true);
    assert_eq!(payload["gate"]["baseline"], 0.6841);
    assert_eq!(payload["gate"]["min_score"], 0.72);
    assert_eq!(payload["gate"]["max_gap"], 0.05);
    assert_eq!(payload["gate"]["margin"], -0.0359);
    assert_eq!(payload["gate"]["top"], serde_json::Value::Null);
    assert_eq!(
        payload["gate"]["floor_dropped"][0],
        serde_json::json!({
            "path": "/fictional/.agents/memory/aerostat.md",
            "score": 0.6841
        })
    );
    assert_eq!(payload["hits"][0]["explain"]["dense_rank"], 2);
    assert_eq!(
        payload["hits"][0]["explain"]["bm25_rank"],
        serde_json::Value::Null
    );
    assert_eq!(payload["hits"][0]["explain"]["rrf_score"], 0.016_393_442_6);
}

#[test]
fn non_finite_values_are_refused_instead_of_becoming_json_null() {
    let mut invalid = hit();
    invalid.score = f64::NAN;

    assert_eq!(
        render_json(&SearchResponse {
            hits: vec![invalid],
            explanation: None,
        }),
        Err(RenderError::NonFinite { field: "score" })
    );
}

#[test]
fn text_renders_an_honestly_clipped_snippet_with_injected_styling() {
    let response = SearchResponse {
        hits: vec![hit()],
        explanation: None,
    };

    let plain = render_text(&response, TextOptions::single(false, Styling::Plain)).unwrap();
    let styled = render_text(&response, TextOptions::single(false, Styling::Ansi)).unwrap();

    assert_eq!(
        plain,
        "\naerostat  (0.8123)\n  /fictional/.agents/memory/aerostat.md\n  aerostat maintenance...\n\n"
    );
    assert_eq!(
        styled,
        "\n\u{1b}[1maerostat\u{1b}[0m  (0.8123)\n  /fictional/.agents/memory/aerostat.md\n  aerostat maintenance...\n\n"
    );
}

#[test]
fn explain_text_tenses_a_hypothetical_gate_and_annotates_the_hit() {
    let mut explained = hit();
    explained.explanation = Some(HitExplanation {
        dense_rank: Some(3),
        bm25_rank: Some(17),
        rrf_score: Some(0.019_876),
    });
    let response = SearchResponse {
        hits: vec![explained],
        explanation: Some(SearchExplanation {
            query: "aerostat".to_owned(),
            applied: false,
            gate: Some(GateVerdict {
                baseline: 0.6841,
                min_score: 0.72,
                max_gap: 0.05,
                no_hits: false,
                floor_fired: true,
                top: None,
                floor_dropped: vec![GateEntry {
                    path: "/fictional/aero\u{1b}[2J\nstat.md".to_owned(),
                    score: 0.6841,
                }],
                gap_dropped: vec![],
                rescued: vec![],
            }),
        }),
    };

    let output = render_text(&response, TextOptions::single(false, Styling::Plain)).unwrap();

    assert!(output.contains("baseline 0.6841   floor 0.72   gap 0.05   margin -0.0359"));
    assert!(output.contains("shown anyway: --no-gate"));
    assert!(output.contains("floor would drop everything unmarked"));
    assert!(output.contains("floor would drop  0.6841  /fictional/aero[2Jstat.md"));
    assert!(!output.contains('\u{1b}'));
    assert!(output.contains("dense #3   bm25 #17   rrf 0.01988"));
}

#[test]
fn all_json_is_flat_while_text_groups_and_disambiguates_duplicate_names() {
    let first = hit();
    let mut twin = hit();
    twin.path = "/fictional/private/aerostat.md".to_owned();
    twin.corpus = CorpusKind::Private;
    twin.status = Some("historical".to_owned());
    twin.snippet = "x".repeat(280);
    twin.clipped_end = false;
    let groups = vec![ProjectSearchResponse {
        project: "/fictional/proj\u{1b}[2J\nroot".to_owned(),
        hits: vec![first, twin],
    }];

    let json: serde_json::Value = serde_json::from_str(&render_all_json(&groups).unwrap()).unwrap();
    let text = render_all_text(&groups, false, Styling::Plain).unwrap();

    assert_eq!(json.as_array().unwrap().len(), 2);
    assert_eq!(json[0]["project"], "/fictional/proj\u{1b}[2J\nroot");
    assert_eq!(json[1]["project"], "/fictional/proj\u{1b}[2J\nroot");
    assert_eq!(json[1]["status"], "historical");
    assert_eq!(text.lines().nth(1), Some("/fictional/proj[2Jroot"));
    assert!(text.contains("  aerostat  (0.8123)"));
    assert!(text.contains("    /fictional/.agents/memory/aerostat.md"));
    assert!(text.contains("    /fictional/private/aerostat.md"));
    assert!(text.contains("[private, historical]"));
    assert!(text.contains(&format!("    {}...", "x".repeat(276))));
    assert!(!text.contains('\u{1b}'));
}

#[test]
fn grouped_text_disambiguates_titles_that_collide_after_sanitizing() {
    let mut first = hit();
    first.name = "aero\nstat".to_owned();
    let mut second = hit();
    second.name = "aerostat".to_owned();
    second.path = "/fictional/private/aerostat.md".to_owned();
    let groups = vec![ProjectSearchResponse {
        project: "/fictional/project".to_owned(),
        hits: vec![first, second],
    }];

    let output = render_all_text(&groups, false, Styling::Plain).unwrap();

    assert_eq!(output.matches("  aerostat  (0.8123)").count(), 2);
    assert!(output.contains("    /fictional/.agents/memory/aerostat.md"));
    assert!(output.contains("    /fictional/private/aerostat.md"));
}

#[test]
fn hostile_text_fields_and_diagnostics_cannot_control_the_terminal() {
    let mut hostile = hit();
    hostile.name = "Aéro\u{1b}[2J\nstat".to_owned();
    hostile.path = "/fictional/\u{009b}2J\rpage.md".to_owned();
    hostile.snippet = "naïve\t\n記録\u{1b}]52;c;payload\u{7}".to_owned();
    hostile.status = Some("hist\u{1b}[4m\norical".to_owned());

    let styled = render_text(
        &SearchResponse {
            hits: vec![hostile],
            explanation: None,
        },
        TextOptions::single(false, Styling::Ansi),
    )
    .unwrap();
    let del_and_c1 = (0x7f..=0x9f)
        .map(|value| char::from_u32(value).unwrap())
        .collect::<String>();
    let diagnostic = render_diagnostic(&format!(
        "failed\nforged header\u{1b}[2J\u{009b}2J{del_and_c1}"
    ));
    let unstyled = styled.replace("\u{1b}[1m", "").replace("\u{1b}[0m", "");

    assert!(unstyled.contains("Aéro[2Jstat"));
    assert!(unstyled.contains("/fictional/2Jpage.md"));
    assert!(unstyled.contains("naïve記録]52;c;payload"));
    assert!(unstyled.contains("[hist[4morical]"));
    assert!(!unstyled.chars().any(|character| {
        character != '\n' && matches!(character, '\u{0000}'..='\u{001f}' | '\u{007f}'..='\u{009f}')
    }));
    assert_no_literal_terminal_controls(&unstyled);
    assert_no_literal_terminal_controls(&diagnostic);
    assert_eq!(diagnostic, "failedforged header[2J2J\n");
}

#[test]
fn full_text_preserves_unicode_layout_without_snippet_ellipses() {
    let mut full = hit();
    full.text = "first line\n\tsecond naïve line\u{1b}[2J\n記録".to_owned();
    full.snippet = "unused".to_owned();
    full.clipped_start = true;
    full.clipped_end = true;

    let output = render_text(
        &SearchResponse {
            hits: vec![full],
            explanation: None,
        },
        TextOptions::single(true, Styling::Plain),
    )
    .unwrap();

    assert!(output.contains("  first line\n  \tsecond naïve line[2J\n  記録\n"));
    assert!(!output.contains("..."));
    assert!(!output.contains('\u{1b}'));
}

#[test]
fn no_hit_explanations_distinguish_an_unconsulted_gate_from_a_rejection() {
    let rejected = SearchResponse {
        hits: vec![],
        explanation: Some(SearchExplanation {
            query: "aerostat".to_owned(),
            applied: true,
            gate: Some(GateVerdict {
                baseline: 0.40,
                min_score: 0.72,
                max_gap: 0.05,
                no_hits: false,
                floor_fired: true,
                top: None,
                floor_dropped: vec![GateEntry {
                    path: "/fictional/weak.md".to_owned(),
                    score: 0.40,
                }],
                gap_dropped: vec![],
                rescued: vec![],
            }),
        }),
    };
    let unindexed = SearchResponse {
        hits: vec![],
        explanation: Some(SearchExplanation {
            query: "aerostat".to_owned(),
            applied: true,
            gate: None,
        }),
    };

    let rejected_text = render_text(&rejected, TextOptions::single(false, Styling::Plain)).unwrap();
    let unindexed_text =
        render_text(&unindexed, TextOptions::single(false, Styling::Plain)).unwrap();
    let unindexed_json: serde_json::Value =
        serde_json::from_str(&render_json(&unindexed).unwrap()).unwrap();

    assert!(rejected_text.starts_with("no confident match\n"));
    assert!(rejected_text.contains("floor drops everything unmarked"));
    assert!(rejected_text.contains("/fictional/weak.md"));
    assert!(unindexed_text.starts_with("no results\n"));
    assert!(unindexed_text.contains("gate: not consulted — this corpus holds no vectors"));
    assert_eq!(unindexed_json["gate"], serde_json::Value::Null);
    assert_eq!(unindexed_json["hits"], serde_json::json!([]));
}

#[test]
fn every_optional_explain_number_is_validated_before_serialization() {
    let mut explained = hit();
    explained.explanation = Some(HitExplanation {
        dense_rank: Some(1),
        bm25_rank: None,
        rrf_score: Some(f64::INFINITY),
    });
    let response = SearchResponse {
        hits: vec![explained],
        explanation: Some(SearchExplanation {
            query: "aerostat".to_owned(),
            applied: true,
            gate: None,
        }),
    };

    assert_eq!(
        render_json(&response),
        Err(RenderError::NonFinite { field: "rrf_score" })
    );
    assert_eq!(
        render_text(&response, TextOptions::single(false, Styling::Plain)),
        Err(RenderError::NonFinite { field: "rrf_score" })
    );
}

#[test]
fn text_rejects_a_non_finite_gate_value_even_when_that_field_is_json_only() {
    let response = SearchResponse {
        hits: vec![],
        explanation: Some(SearchExplanation {
            query: "aerostat".to_owned(),
            applied: true,
            gate: Some(GateVerdict {
                baseline: 0.80,
                min_score: 0.72,
                max_gap: 0.05,
                no_hits: true,
                floor_fired: false,
                top: Some(f64::NAN),
                floor_dropped: vec![],
                gap_dropped: vec![],
                rescued: vec![],
            }),
        }),
    };

    assert_eq!(
        render_text(&response, TextOptions::single(false, Styling::Plain)),
        Err(RenderError::NonFinite { field: "top" })
    );
}

#[test]
fn explain_ranks_are_one_based() {
    let mut invalid = hit();
    invalid.explanation = Some(HitExplanation {
        dense_rank: Some(0),
        bm25_rank: Some(1),
        rrf_score: None,
    });
    let response = SearchResponse {
        hits: vec![invalid],
        explanation: Some(SearchExplanation {
            query: "aerostat".to_owned(),
            applied: true,
            gate: None,
        }),
    };

    assert_eq!(
        render_json(&response),
        Err(RenderError::InvalidRank {
            field: "dense_rank"
        })
    );
}

#[test]
fn presentation_debug_redacts_dynamic_values_through_nested_results() {
    let mut private_hit = hit();
    private_hit.name = "PRIVATE_NAME_SENTINEL".to_owned();
    private_hit.path = "/fictional/PRIVATE_PATH_SENTINEL.md".to_owned();
    private_hit.text = "PRIVATE_TEXT_SENTINEL".to_owned();
    private_hit.snippet = "PRIVATE_SNIPPET_SENTINEL".to_owned();
    private_hit.status = Some("PRIVATE_STATUS_SENTINEL".to_owned());
    private_hit.explanation = Some(HitExplanation {
        dense_rank: Some(1),
        bm25_rank: Some(2),
        rrf_score: Some(0.01),
    });
    let response = SearchResponse {
        hits: vec![private_hit.clone()],
        explanation: Some(SearchExplanation {
            query: "PRIVATE_QUERY_SENTINEL".to_owned(),
            applied: true,
            gate: Some(GateVerdict {
                baseline: 0.8,
                min_score: 0.72,
                max_gap: 0.05,
                no_hits: false,
                floor_fired: false,
                top: Some(0.8),
                floor_dropped: vec![GateEntry {
                    path: "/fictional/PRIVATE_FLOOR_SENTINEL.md".to_owned(),
                    score: 0.7,
                }],
                gap_dropped: vec![GateEntry {
                    path: "/fictional/PRIVATE_GAP_SENTINEL.md".to_owned(),
                    score: 0.6,
                }],
                rescued: vec![GateEntry {
                    path: "/fictional/PRIVATE_RESCUE_SENTINEL.md".to_owned(),
                    score: 0.5,
                }],
            }),
        }),
    };
    let groups = vec![ProjectSearchResponse {
        project: "/fictional/PRIVATE_PROJECT_SENTINEL".to_owned(),
        hits: vec![private_hit],
    }];
    let result_debug = format!("{:?}", Ok::<_, RenderError>(response));
    let groups_debug = format!("{groups:?}");
    let error_debug = format!(
        "{:?}",
        Err::<SearchResponse, _>(RenderError::MissingHitExplanation {
            path: "/fictional/PRIVATE_ERROR_PATH_SENTINEL.md".to_owned(),
        })
    );
    let combined = format!("{result_debug}\n{groups_debug}\n{error_debug}");

    for sentinel in [
        "PRIVATE_NAME_SENTINEL",
        "PRIVATE_PATH_SENTINEL",
        "PRIVATE_TEXT_SENTINEL",
        "PRIVATE_SNIPPET_SENTINEL",
        "PRIVATE_STATUS_SENTINEL",
        "PRIVATE_QUERY_SENTINEL",
        "PRIVATE_FLOOR_SENTINEL",
        "PRIVATE_GAP_SENTINEL",
        "PRIVATE_RESCUE_SENTINEL",
        "PRIVATE_PROJECT_SENTINEL",
        "PRIVATE_ERROR_PATH_SENTINEL",
    ] {
        assert!(!combined.contains(sentinel), "debug leaked {sentinel}");
    }
    assert!(combined.contains("<redacted>"));
}

#[test]
fn render_error_display_redacts_the_untrusted_hit_path() {
    let error = RenderError::MissingHitExplanation {
        path: "/fictional/PRIVATE_ERROR_PATH_SENTINEL\n\u{1b}[2J\u{009b}2J.md".to_owned(),
    };

    let display = error.to_string();

    assert_eq!(
        display,
        "explained response is missing rank signals for a redacted hit"
    );
    assert_no_literal_terminal_controls(&display);
    assert!(!display.contains("PRIVATE_ERROR_PATH_SENTINEL"));
}
