use yams_core::{
    DEFAULT_K, ExactMatch, ExitCode, GateMode, GateReason, HitExplanation, LiteralChunk, MAX_GAP,
    MIN_SCORE, NormalizedScore, PageLabels, RankContribution, RankError, RankedHit, SelectedChunk,
    SelectionConfig, SelectionError, SelectionOutcome, exact_identifier_match, normalize_score,
    select as select_normalized,
};

fn normalized(score: f64) -> NormalizedScore {
    normalize_score(score).expect("fictional score is a finite cosine")
}

fn select(
    query: &str,
    baseline: Option<f64>,
    ranked: &[RankedHit],
    literal_chunks: &[LiteralChunk<'_>],
    lexical_leader: Option<&str>,
    config: SelectionConfig,
) -> Result<SelectionOutcome, SelectionError> {
    select_normalized(
        query,
        baseline.map(normalized),
        ranked,
        literal_chunks,
        lexical_leader,
        config,
    )
}

fn chunk(ordinal: u32, text: &str) -> SelectedChunk {
    SelectedChunk::new(ordinal, text)
}

fn hit(name: &str, rank: usize, score: f64) -> RankedHit {
    RankedHit::new(
        name,
        format!("/fictional/shared/{name}.md"),
        chunk(0, &format!("dense text for {name}")),
        None,
        normalized(score),
        rank,
        HitExplanation::new(
            Some(rank),
            None,
            None,
            vec![RankContribution::new(
                0,
                rank,
                1.0,
                1.0 / (60.0 + rank as f64),
            )],
        ),
        PageLabels::new(Some("shared"), Some("current"), None),
    )
}

fn config() -> SelectionConfig {
    SelectionConfig::default()
}

#[allow(clippy::too_many_arguments)]
fn custom_hit(
    name: &str,
    path: &str,
    dense_chunk: SelectedChunk,
    lexical_chunk: Option<SelectedChunk>,
    score: f64,
    rank: usize,
    explanation: HitExplanation,
    labels: PageLabels,
) -> RankedHit {
    RankedHit::new(
        name,
        path,
        dense_chunk,
        lexical_chunk,
        normalized(score),
        rank,
        explanation,
        labels,
    )
}

#[test]
fn shipped_defaults_and_invalid_configurations_are_typed() {
    assert_eq!(DEFAULT_K, 5);
    assert_eq!(MIN_SCORE, 0.72);
    assert_eq!(MAX_GAP, 0.05);
    assert_eq!(config().limit(), DEFAULT_K);
    assert_eq!(config().min_score(), MIN_SCORE);
    assert_eq!(config().max_gap(), MAX_GAP);
    assert_eq!(config().gate_mode(), GateMode::Apply);

    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            SelectionConfig::new(DEFAULT_K, value, MAX_GAP, GateMode::Apply),
            Err(SelectionError::NonFiniteMinScore)
        );
        assert_eq!(
            SelectionConfig::new(DEFAULT_K, MIN_SCORE, value, GateMode::Apply),
            Err(SelectionError::NonFiniteMaxGap)
        );
    }
    for value in [-1.0001, 1.0001] {
        assert_eq!(
            SelectionConfig::new(DEFAULT_K, value, MAX_GAP, GateMode::Apply),
            Err(SelectionError::MinScoreOutsideCosineRange)
        );
    }
    assert_eq!(
        SelectionConfig::new(DEFAULT_K, MIN_SCORE, -0.0001, GateMode::Apply),
        Err(SelectionError::NegativeMaxGap)
    );
}

#[test]
fn zero_limit_reaches_the_gate_empty_and_disables_exact_rescue() {
    let zero = SelectionConfig::new(0, MIN_SCORE, MAX_GAP, GateMode::Apply).unwrap();
    let literal = [LiteralChunk::new(
        "/fictional/shared/answer.md",
        0,
        "the exact ZERO_LIMIT_ID_4 answer",
    )];
    let outcome = select(
        "ZERO_LIMIT_ID_4",
        Some(0.90),
        &[hit("answer", 1, 0.90)],
        &literal,
        Some("/fictional/shared/answer.md"),
        zero,
    )
    .unwrap();

    assert!(outcome.hits().is_empty());
    assert_eq!(outcome.exit_code(), ExitCode::Unsure);
    assert_eq!(outcome.gate().unwrap().reason(), GateReason::NoHits);
    assert!(outcome.gate().unwrap().no_hits());
    assert!(!outcome.gate().unwrap().floor_fired());
    assert!(outcome.gate().unwrap().rescued().is_empty());
}

#[test]
fn exact_identifier_grammar_boundaries_uniqueness_and_lexical_leader_match_python() {
    let pages = [
        LiteralChunk::new("/fictional/shared/alpha.md", 1, "later ID_42 mention"),
        LiteralChunk::new("/fictional/shared/alpha.md", 0, "first ID_42 mention"),
        LiteralChunk::new(
            "/fictional/shared/lookalike.md",
            0,
            "OTHER_ID_42_SUFFIX is not an exact occurrence",
        ),
    ];

    assert_eq!(
        exact_identifier_match("ID_42", Some("/fictional/shared/alpha.md"), &pages,),
        Some(ExactMatch::new("/fictional/shared/alpha.md", 0))
    );
    let matched =
        exact_identifier_match("ID_42", Some("/fictional/shared/alpha.md"), &pages).unwrap();
    assert_eq!(matched.path(), "/fictional/shared/alpha.md");
    assert_eq!(matched.ordinal(), 0);
    assert_eq!(
        exact_identifier_match("ID_42", Some("/fictional/shared/lookalike.md"), &pages,),
        None,
        "the unique page must also lead the filtered lexical ranking"
    );
    assert_eq!(
        exact_identifier_match("id_42", Some("/fictional/shared/alpha.md"), &pages,),
        None,
        "literal matching is case-sensitive"
    );

    for prose in ["platypus", "Platypus", "two ID_42", "!!!"] {
        assert_eq!(
            exact_identifier_match(prose, Some("/fictional/shared/alpha.md"), &pages),
            None,
            "ordinary or multi-token queries are not identifiers: {prose}"
        );
    }

    let camel = [LiteralChunk::new(
        "/fictional/shared/lifecycle.md",
        0,
        "call calibrateMoonValve atomically",
    )];
    assert_eq!(
        exact_identifier_match(
            "calibrateMoonValve",
            Some("/fictional/shared/lifecycle.md"),
            &camel,
        ),
        Some(ExactMatch::new("/fictional/shared/lifecycle.md", 0))
    );

    let duplicate = [
        LiteralChunk::new("/fictional/shared/alpha.md", 0, "ID_42"),
        LiteralChunk::new("/fictional/private/beta.md", 0, "ID_42"),
    ];
    assert_eq!(
        exact_identifier_match("ID_42", Some("/fictional/shared/alpha.md"), &duplicate,),
        None,
        "an exact occurrence on a second page defeats the rescue"
    );
}

#[test]
fn page_collapse_is_by_full_path_and_selection_prefers_the_lexical_chunk() {
    let later_duplicate = custom_hit(
        "long",
        "/fictional/shared/long.md",
        chunk(9, "later duplicate chunk"),
        None,
        0.81,
        3,
        HitExplanation::new(Some(3), None, None, vec![]),
        PageLabels::default(),
    );
    let lexical = custom_hit(
        "long",
        "/fictional/shared/long.md",
        chunk(0, "dense text for long"),
        Some(chunk(4, "the lexical answer chunk")),
        0.91,
        1,
        HitExplanation::new(
            Some(2),
            Some(7),
            Some(0.019_25),
            vec![
                RankContribution::new(0, 2, 1.0, 1.0 / 62.0),
                RankContribution::new(1, 7, 0.2, 0.2 / 67.0),
            ],
        ),
        PageLabels::new(
            Some("private"),
            Some("historical"),
            Some("/fictional/project"),
        ),
    );
    let twin = custom_hit(
        "long",
        "/fictional/private/long.md",
        chunk(0, "same filename, different page"),
        None,
        0.88,
        2,
        HitExplanation::new(Some(2), None, None, vec![]),
        PageLabels::default(),
    );

    let outcome = select(
        "rename lifecycle",
        Some(0.91),
        &[later_duplicate, twin, lexical],
        &[],
        None,
        config(),
    )
    .unwrap();

    assert_eq!(outcome.query(), "rename lifecycle");
    assert_eq!(outcome.exit_code(), ExitCode::Ok);
    assert_eq!(outcome.hits().len(), 2, "one row per canonical page path");
    assert_eq!(outcome.hits()[0].path(), "/fictional/shared/long.md");
    assert_eq!(outcome.hits()[0].selected_chunk().ordinal(), 4);
    assert_eq!(outcome.hits()[0].text(), "the lexical answer chunk");
    assert_eq!(
        outcome.hits()[0].score(),
        0.91,
        "display selection cannot change score"
    );
    assert_eq!(outcome.hits()[0].explanation().dense_rank(), Some(2));
    assert_eq!(outcome.hits()[0].explanation().bm25_rank(), Some(7));
    assert_eq!(outcome.hits()[0].explanation().rrf_score(), Some(0.019_25));
    assert_eq!(outcome.hits()[0].explanation().contributions().len(), 2);
    let contribution = outcome.hits()[0].explanation().contributions()[1];
    assert_eq!(contribution.source(), 1);
    assert_eq!(contribution.rank(), 7);
    assert_eq!(contribution.weight(), 0.2);
    assert_eq!(contribution.score(), 0.2 / 67.0);
    assert_eq!(outcome.hits()[0].labels().corpus(), Some("private"));
    assert_eq!(outcome.hits()[0].labels().status(), Some("historical"));
    assert_eq!(
        outcome.hits()[0].labels().project(),
        Some("/fictional/project")
    );
    assert_eq!(outcome.hits()[1].path(), "/fictional/private/long.md");
}

#[test]
fn exact_identifier_rescue_uses_the_last_slot_and_the_exact_chunk_for_display() {
    let mut ranked = (1..=7)
        .map(|rank| hit(&format!("page-{rank}"), rank, (95 - rank) as f64 / 100.0))
        .collect::<Vec<_>>();
    ranked[6] = custom_hit(
        "literal",
        "/fictional/shared/literal.md",
        chunk(0, "early ID_42 mention"),
        Some(chunk(3, "best lexical ID_42 answer")),
        0.88,
        7,
        HitExplanation::new(
            Some(7),
            Some(1),
            Some(0.01),
            vec![RankContribution::new(1, 1, 0.2, 0.2 / 61.0)],
        ),
        PageLabels::default(),
    );
    let literal_chunks = [
        LiteralChunk::new(
            "/fictional/shared/literal.md",
            3,
            "best lexical ID_42 answer",
        ),
        LiteralChunk::new("/fictional/shared/literal.md", 0, "early ID_42 mention"),
    ];

    let outcome = select(
        "ID_42",
        Some(0.94),
        &ranked,
        &literal_chunks,
        Some("/fictional/shared/literal.md"),
        config(),
    )
    .unwrap();

    assert_eq!(
        outcome
            .hits()
            .iter()
            .map(|hit| hit.path())
            .collect::<Vec<_>>(),
        [
            "/fictional/shared/page-1.md",
            "/fictional/shared/page-2.md",
            "/fictional/shared/page-3.md",
            "/fictional/shared/page-4.md",
            "/fictional/shared/literal.md",
        ]
    );
    let rescued = outcome.hits().last().unwrap();
    assert!(rescued.exact());
    assert_eq!(rescued.selected_chunk().ordinal(), 0);
    assert_eq!(rescued.text(), "early ID_42 mention");
    assert_eq!(
        rescued.score(),
        0.88,
        "the dense baseline score remains public"
    );
    assert_eq!(outcome.gate().unwrap().rescued()[0].path(), rescued.path());
    assert_eq!(outcome.gate().unwrap().rescued()[0].score(), 0.88);
}

#[test]
fn floor_gap_boundaries_and_exact_exemptions_match_the_oracle() {
    let confident = vec![
        hit("top", 1, 0.87),
        hit("boundary", 2, 0.82),
        hit("tail", 3, 0.8199),
    ];
    let outcome = select("query", Some(0.87), &confident, &[], None, config()).unwrap();
    assert_eq!(
        outcome
            .hits()
            .iter()
            .map(|hit| hit.name())
            .collect::<Vec<_>>(),
        ["top", "boundary"]
    );
    let gate = outcome.gate().unwrap();
    assert_eq!(gate.reason(), GateReason::Gap);
    assert_eq!(gate.baseline(), 0.87);
    assert!(!gate.no_hits());
    assert!(!gate.floor_fired());
    assert_eq!(gate.gap_dropped()[0].path(), "/fictional/shared/tail.md");
    assert_eq!(gate.top(), Some(0.87));

    let weak = vec![hit("weak", 1, 0.60), hit("exact", 2, 0.59)];
    let exact_chunks = [LiteralChunk::new(
        "/fictional/shared/exact.md",
        0,
        "the unique WEAK_ID_9 answer",
    )];
    let outcome = select(
        "WEAK_ID_9",
        Some(0.60),
        &weak,
        &exact_chunks,
        Some("/fictional/shared/exact.md"),
        config(),
    )
    .unwrap();
    assert_eq!(outcome.exit_code(), ExitCode::Ok);
    assert_eq!(outcome.hits().len(), 1);
    assert_eq!(outcome.hits()[0].name(), "exact");
    assert!(outcome.hits()[0].exact());
    let gate = outcome.gate().unwrap();
    assert_eq!(gate.reason(), GateReason::Floor);
    assert!(gate.floor_fired());
    assert_eq!(gate.margin(), -0.12);
    assert_eq!(gate.top(), Some(0.59));
    assert_eq!(gate.floor_dropped()[0].path(), "/fictional/shared/weak.md");
    assert_eq!(gate.rescued()[0].path(), "/fictional/shared/exact.md");
}

#[test]
fn floor_uses_the_corpus_baseline_but_gap_uses_the_best_visible_hit() {
    let ranked = [hit("fused-first", 1, 0.70), hit("visible-anchor", 2, 0.71)];
    let outcome = select("query", Some(0.90), &ranked, &[], None, config()).unwrap();

    assert_eq!(outcome.hits().len(), 2);
    assert_eq!(outcome.gate().unwrap().top(), Some(0.71));
    assert_eq!(outcome.gate().unwrap().reason(), GateReason::Passed);

    let ranked = [hit("impossible-shape", 1, 0.90)];
    let outcome = select("query", Some(0.71), &ranked, &[], None, config()).unwrap();

    assert_eq!(outcome.exit_code(), ExitCode::Unsure);
    assert!(outcome.hits().is_empty());
    assert_eq!(outcome.gate().unwrap().reason(), GateReason::Floor);
}

#[test]
fn floor_consumes_python_four_decimal_scores_before_comparison() {
    let outcome = select(
        "query",
        Some(0.71996),
        &[hit("rounded-to-floor", 1, 0.71996)],
        &[],
        None,
        config(),
    )
    .unwrap();

    assert_eq!(outcome.exit_code(), ExitCode::Ok);
    assert_eq!(outcome.hits()[0].score(), 0.72);
    assert_eq!(outcome.gate().unwrap().baseline(), 0.72);
}

#[test]
fn visible_gap_uses_python_rounding_including_fifth_decimal_ties() {
    let outcome = select(
        "query",
        Some(0.80005),
        &[hit("rounds-up", 1, 0.80005), hit("rounds-down", 2, 0.75005)],
        &[],
        None,
        config(),
    )
    .unwrap();

    assert_eq!(outcome.hits().len(), 1);
    assert_eq!(outcome.hits()[0].score(), 0.8001);
    assert_eq!(outcome.gate().unwrap().baseline(), 0.8001);
    assert_eq!(outcome.gate().unwrap().gap_dropped()[0].score(), 0.75);

    let rounded_boundary = select(
        "query",
        Some(0.87),
        &[hit("top", 1, 0.87), hit("rounds-to-boundary", 2, 0.81996)],
        &[],
        None,
        config(),
    )
    .unwrap();
    assert_eq!(rounded_boundary.hits().len(), 2);
    assert_eq!(rounded_boundary.hits()[1].score(), 0.82);
}

#[test]
fn empty_ranked_order_in_a_nonempty_index_is_a_no_hits_unsure_verdict() {
    let outcome = select("query", Some(0.90), &[], &[], None, config()).unwrap();

    assert_eq!(outcome.exit_code(), ExitCode::Unsure);
    assert!(outcome.hits().is_empty());
    assert_eq!(outcome.gate().unwrap().reason(), GateReason::NoHits);
    assert!(outcome.gate().unwrap().no_hits());
    assert!(!outcome.gate().unwrap().floor_fired());
}

#[test]
fn exact_marker_is_not_reported_as_a_rescue_when_the_score_passes() {
    let ranked = [hit("answer", 1, 0.90)];
    let literal = [LiteralChunk::new(
        "/fictional/shared/answer.md",
        0,
        "the ANSWER_ID_7 contract",
    )];
    let outcome = select(
        "ANSWER_ID_7",
        Some(0.90),
        &ranked,
        &literal,
        Some("/fictional/shared/answer.md"),
        config(),
    )
    .unwrap();

    assert!(outcome.hits()[0].exact());
    assert!(outcome.gate().unwrap().rescued().is_empty());
    assert_eq!(outcome.gate().unwrap().reason(), GateReason::Passed);
}

#[test]
fn equal_rank_inputs_break_ties_by_full_path_and_then_chunk_ordinal() {
    let zeta = custom_hit(
        "zeta",
        "/fictional/shared/zeta.md",
        chunk(0, "zeta"),
        None,
        0.80,
        1,
        HitExplanation::new(Some(2), None, None, vec![]),
        PageLabels::default(),
    );
    let alpha_later = custom_hit(
        "alpha",
        "/fictional/shared/alpha.md",
        chunk(3, "later duplicate"),
        None,
        0.80,
        1,
        HitExplanation::new(Some(1), None, None, vec![]),
        PageLabels::default(),
    );
    let alpha_first = custom_hit(
        "alpha",
        "/fictional/shared/alpha.md",
        chunk(1, "first duplicate"),
        None,
        0.80,
        1,
        HitExplanation::new(Some(1), None, None, vec![]),
        PageLabels::default(),
    );

    let outcome = select(
        "query",
        Some(0.80),
        &[zeta, alpha_later, alpha_first],
        &[],
        None,
        config(),
    )
    .unwrap();

    assert_eq!(
        outcome
            .hits()
            .iter()
            .map(|hit| hit.name())
            .collect::<Vec<_>>(),
        ["alpha", "zeta"]
    );
    assert_eq!(outcome.hits()[0].selected_chunk().ordinal(), 1);
}

#[test]
fn overrides_and_no_gate_report_the_hypothetical_verdict_without_filtering() {
    let config = SelectionConfig::new(5, 0.99, 0.5, GateMode::Bypass).unwrap();
    let outcome = select(
        "alpha",
        Some(0.90),
        &[hit("alpha", 1, 0.90), hit("beta", 2, 0.88)],
        &[],
        None,
        config,
    )
    .unwrap();

    assert_eq!(outcome.exit_code(), ExitCode::Ok);
    assert_eq!(outcome.hits().len(), 2, "--no-gate returns the ungated set");
    assert!(!outcome.applied());
    let gate = outcome.gate().unwrap();
    assert_eq!((gate.min_score(), gate.max_gap()), (0.99, 0.5));
    assert!(gate.floor_fired());
    assert_eq!(gate.floor_dropped().len(), 2);
}

#[test]
fn empty_index_and_gated_away_candidates_map_to_distinct_exit_codes() {
    let empty = select("query", None, &[], &[], None, config()).unwrap();
    assert_eq!(empty.exit_code(), ExitCode::Empty);
    assert_eq!(i32::from(empty.exit_code()), 1);
    assert_eq!(empty.gate(), None);

    let unsure = select(
        "query",
        Some(0.60),
        &[hit("weak", 1, 0.60)],
        &[],
        None,
        config(),
    )
    .unwrap();
    assert_eq!(unsure.exit_code(), ExitCode::Unsure);
    assert_eq!(i32::from(unsure.exit_code()), 3);
    assert!(unsure.hits().is_empty());
    assert_eq!(unsure.gate().unwrap().reason(), GateReason::Floor);
}

#[test]
fn invalid_raw_scores_cannot_cross_the_normalized_boundary_and_rank_signals_are_typed() {
    for raw in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(normalize_score(raw), Err(RankError::NonFiniteScore));
    }
    for raw in [-1.0001, 1.0001] {
        assert_eq!(
            normalize_score(raw),
            Err(RankError::ScoreOutsideCosineRange)
        );
    }

    let invalid = custom_hit(
        "bad",
        "/fictional/shared/bad.md",
        chunk(0, "bad"),
        None,
        0.8,
        0,
        HitExplanation::new(Some(0), Some(0), Some(f64::NAN), vec![]),
        PageLabels::default(),
    );
    assert!(matches!(
        select("query", Some(0.8), &[invalid], &[], None, config()),
        Err(SelectionError::InvalidRankSignal { .. })
    ));
}
