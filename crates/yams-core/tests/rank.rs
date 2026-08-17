use std::path::Path;

use yams_core::{
    CANDIDATES, ChunkId, DenseCandidate, LEXICAL_WEIGHT, LexicalCandidate, PageId, RRF_K,
    RankError, VectorSide, cosine, dense_rank, hybrid_rank, normalize_score, ranked_ids,
    rrf_explained, rrf_scores,
};

fn page(path: &str) -> PageId {
    PageId::from_canonical_path(path).expect("fictional test paths are canonical")
}

#[test]
fn stable_ids_are_the_canonical_path_and_zero_based_ordinal() {
    let alpha = page("/fictional/shared/alpha.md");
    let beta = page("/fictional/shared/beta.md");
    let first = ChunkId::new(alpha.clone(), 0);
    let second = ChunkId::new(alpha.clone(), 1);

    assert_eq!(alpha.as_path(), Path::new("/fictional/shared/alpha.md"));
    assert_eq!(alpha.as_str(), "/fictional/shared/alpha.md");
    assert_eq!(first.page(), &alpha);
    assert_eq!(first.ordinal(), 0);
    assert!(first < second);
    assert!(second < ChunkId::new(beta, 0));
    assert_eq!(RRF_K, 60);
    assert_eq!(CANDIDATES, 25);
    assert_eq!(LEXICAL_WEIGHT, 0.2);
}

#[test]
fn page_ids_refuse_paths_that_are_not_canonical_utf8_absolutes() {
    assert!(matches!(
        PageId::from_canonical_path("relative/page.md"),
        Err(RankError::NonCanonicalPagePath { .. })
    ));
    assert!(matches!(
        PageId::from_canonical_path("/fictional/../page.md"),
        Err(RankError::NonCanonicalPagePath { .. })
    ));
    for redundant_spelling in [
        "/fictional/./page.md",
        "/fictional//page.md",
        "/fictional/page.md/",
    ] {
        assert!(matches!(
            PageId::from_canonical_path(redundant_spelling),
            Err(RankError::NonCanonicalPagePath { .. })
        ));
    }
}

#[test]
fn cosine_uses_f64_accumulation_and_returns_the_mathematical_similarity() {
    let score = cosine(&[3.0, 4.0], &[4.0, 3.0]).unwrap();
    assert!((score - 0.96).abs() < 1e-12);

    let cancellation = cosine(&[1.0e20, 1.0, -1.0e20], &[1.0e20, 1.0, 1.0e20]).unwrap();
    assert!(cancellation.is_finite());
}

#[test]
fn cosine_reports_typed_shape_and_component_failures() {
    assert_eq!(
        cosine(&[1.0, 0.0], &[1.0]),
        Err(RankError::DimensionMismatch { left: 2, right: 1 })
    );
    assert_eq!(
        cosine(&[], &[]),
        Err(RankError::EmptyVector {
            side: VectorSide::Left,
        })
    );
    assert_eq!(
        cosine(&[f32::NAN], &[1.0]),
        Err(RankError::NonFiniteComponent {
            side: VectorSide::Left,
            index: 0,
        })
    );
    assert_eq!(
        cosine(&[1.0], &[f32::INFINITY]),
        Err(RankError::NonFiniteComponent {
            side: VectorSide::Right,
            index: 0,
        })
    );
    assert_eq!(
        cosine(&[1.0], &[0.0]),
        Err(RankError::ZeroNorm {
            side: VectorSide::Right,
        })
    );
}

#[test]
fn public_scores_match_python_four_decimal_rounding() {
    assert_eq!(normalize_score(0.12345).unwrap().get(), 0.1235);
    assert_eq!(normalize_score(0.12355).unwrap().get(), 0.1235);
    assert_eq!(normalize_score(0.90155).unwrap().get(), 0.9015);
    assert_eq!(normalize_score(0.90145).unwrap().get(), 0.9014);
    assert_eq!(normalize_score(-0.00005).unwrap().get(), -0.0001);
}

#[test]
fn public_scores_refuse_nonfinite_and_out_of_cosine_range_values() {
    assert_eq!(normalize_score(f64::NAN), Err(RankError::NonFiniteScore));
    assert_eq!(
        normalize_score(1.0001),
        Err(RankError::ScoreOutsideCosineRange)
    );
}

#[test]
fn dense_rank_sorts_chunks_then_collapses_each_canonical_page() {
    let alpha = page("/fictional/private/alpha.md");
    let beta = page("/fictional/private/beta.md");
    let gamma = page("/fictional/private/gamma.md");
    let weak = [0.8, 0.6];
    let strong = [1.0, 0.0];
    let orthogonal = [0.0, 1.0];
    let candidates = vec![
        DenseCandidate::new(ChunkId::new(beta.clone(), 0), &strong),
        DenseCandidate::new(ChunkId::new(alpha.clone(), 0), &weak),
        DenseCandidate::new(ChunkId::new(gamma.clone(), 0), &orthogonal),
        DenseCandidate::new(ChunkId::new(alpha.clone(), 1), &strong),
    ];

    assert_eq!(candidates[0].id(), &ChunkId::new(beta.clone(), 0));
    assert_eq!(candidates[0].vector(), strong);

    let ranked = dense_rank(&[1.0, 0.0], &candidates).unwrap();

    assert_eq!(
        ranked.iter().map(|hit| hit.page()).collect::<Vec<_>>(),
        [&alpha, &beta, &gamma]
    );
    assert_eq!(ranked[0].chunk(), &ChunkId::new(alpha, 1));
    assert_eq!(ranked[0].score().get(), 1.0);
    assert_eq!(ranked[2].score().get(), 0.0);
}

#[test]
fn dense_rank_validates_every_candidate_before_returning_results() {
    let valid = [1.0, 0.0];
    let corrupt = [f32::NAN, 0.0];
    let candidates = vec![
        DenseCandidate::new(
            ChunkId::new(page("/fictional/private/answer.md"), 0),
            &valid,
        ),
        DenseCandidate::new(
            ChunkId::new(page("/fictional/private/corrupt.md"), 0),
            &corrupt,
        ),
    ];

    assert_eq!(
        dense_rank(&valid, &candidates),
        Err(RankError::NonFiniteComponent {
            side: VectorSide::Right,
            index: 0,
        })
    );
}

#[test]
fn dense_order_uses_raw_cosine_before_exposing_rounded_scores() {
    let lower = 0.900_03_f32;
    let higher = 0.900_04_f32;
    let lower_vector = [lower, (1.0 - lower * lower).sqrt()];
    let higher_vector = [higher, (1.0 - higher * higher).sqrt()];
    let candidates = vec![
        DenseCandidate::new(
            ChunkId::new(page("/fictional/private/alpha-lower.md"), 0),
            &lower_vector,
        ),
        DenseCandidate::new(
            ChunkId::new(page("/fictional/private/zeta-higher.md"), 0),
            &higher_vector,
        ),
    ];

    let ranked = dense_rank(&[1.0, 0.0], &candidates).unwrap();

    assert_eq!(ranked[0].page(), &page("/fictional/private/zeta-higher.md"));
    assert_eq!(ranked[0].score(), ranked[1].score());
}

#[test]
fn weighted_rrf_matches_the_contract_and_preserves_source_contributions() {
    let alpha = page("/fictional/private/alpha.md");
    let beta = page("/fictional/private/beta.md");
    let gamma = page("/fictional/private/gamma.md");
    let rankings = vec![vec![alpha.clone(), beta.clone()], vec![beta.clone(), gamma]];
    let weights = [1.0, LEXICAL_WEIGHT];

    let scores = rrf_scores(&rankings, &weights, RRF_K).unwrap();
    assert!((scores[&alpha] - 1.0 / 61.0).abs() < 1e-12);
    assert!((scores[&beta] - (1.0 / 62.0 + 0.2 / 61.0)).abs() < 1e-12);
    assert_eq!(ranked_ids(&scores, 3)[0], beta);

    let explained = rrf_explained(&rankings, &weights, RRF_K).unwrap();
    let beta_score = &explained[&beta];
    assert_eq!(beta_score.contributions().len(), 2);
    assert_eq!(beta_score.contributions()[0].source(), 0);
    assert_eq!(beta_score.contributions()[0].rank(), 2);
    assert_eq!(beta_score.contributions()[1].source(), 1);
    assert_eq!(beta_score.contributions()[1].rank(), 1);
    assert_eq!(beta_score.contributions()[1].weight(), LEXICAL_WEIGHT);
    assert_eq!(beta_score.contributions()[1].score(), LEXICAL_WEIGHT / 61.0);
    assert_eq!(beta_score.total(), scores[&beta]);
}

#[test]
fn rrf_ties_break_by_page_id_not_input_order() {
    let alpha = page("/fictional/private/alpha.md");
    let beta = page("/fictional/private/beta.md");
    let rankings = vec![
        vec![beta.clone(), alpha.clone()],
        vec![alpha.clone(), beta.clone()],
    ];

    let scores = rrf_scores(&rankings, &[1.0, 1.0], RRF_K).unwrap();

    assert_eq!(ranked_ids(&scores, 2), [alpha, beta]);
}

#[test]
fn rrf_refuses_weight_shape_and_nonfinite_values() {
    let rankings = vec![vec![page("/fictional/private/alpha.md")]];

    assert_eq!(
        rrf_scores(&rankings, &[], RRF_K),
        Err(RankError::RankingWeightMismatch {
            rankings: 1,
            weights: 0,
        })
    );
    assert_eq!(
        rrf_scores(&rankings, &[f64::NAN], RRF_K),
        Err(RankError::NonFiniteWeight { source_index: 0 })
    );
    assert_eq!(
        rrf_scores(&rankings, &[-0.1], RRF_K),
        Err(RankError::NegativeWeight { source_index: 0 })
    );
}

#[test]
fn rrf_deduplicates_dense_and_lexical_sources_before_explanation_ranks() {
    let alpha = page("/fictional/private/alpha.md");
    let beta = page("/fictional/private/beta.md");
    let rankings = vec![
        vec![alpha.clone(), alpha.clone(), beta.clone()],
        vec![beta.clone(), beta.clone(), alpha.clone()],
    ];

    let scores = rrf_explained(&rankings, &[1.0, LEXICAL_WEIGHT], RRF_K).unwrap();

    assert_eq!(scores[&alpha].contributions().len(), 2);
    assert_eq!(scores[&alpha].contributions()[0].rank(), 1);
    assert_eq!(scores[&alpha].contributions()[1].rank(), 2);
    assert_eq!(scores[&beta].contributions().len(), 2);
    assert_eq!(scores[&beta].contributions()[0].rank(), 2);
    assert_eq!(scores[&beta].contributions()[1].rank(), 1);
    assert_eq!(scores[&alpha].total(), 1.0 / 61.0 + LEXICAL_WEIGHT / 62.0);
}

#[test]
fn rrf_refuses_a_nonfinite_total_from_finite_source_weights() {
    let alpha = page("/fictional/private/alpha.md");
    let rankings = vec![vec![alpha.clone()], vec![alpha.clone()]];

    assert_eq!(
        rrf_explained(&rankings, &[f64::MAX, f64::MAX], 0),
        Err(RankError::NonFiniteFusedTotal { page: alpha })
    );
}

#[test]
fn lexical_chunks_collapse_by_page_before_ranking_and_select_the_first_chunk() {
    let alpha = page("/fictional/private/alpha.md");
    let beta = page("/fictional/private/beta.md");
    let alpha_vector = [1.0, 0.0];
    let beta_vector = [0.8, 0.6];
    let candidates = vec![
        DenseCandidate::new(ChunkId::new(alpha.clone(), 0), &alpha_vector),
        DenseCandidate::new(ChunkId::new(beta.clone(), 0), &beta_vector),
    ];
    let dense = dense_rank(&alpha_vector, &candidates).unwrap();
    let lexical = vec![
        LexicalCandidate::new(ChunkId::new(alpha.clone(), 2)),
        LexicalCandidate::new(ChunkId::new(alpha.clone(), 1)),
        LexicalCandidate::new(ChunkId::new(beta.clone(), 3)),
    ];

    let hits = hybrid_rank(&dense, &lexical, 2).unwrap();
    let alpha_hit = hits.iter().find(|hit| hit.page() == &alpha).unwrap();
    let beta_hit = hits.iter().find(|hit| hit.page() == &beta).unwrap();

    assert_eq!(alpha_hit.lexical_rank(), Some(1));
    assert_eq!(alpha_hit.lexical_chunk(), Some(&ChunkId::new(alpha, 2)));
    assert_eq!(
        alpha_hit.selected_chunk(),
        &ChunkId::new(page("/fictional/private/alpha.md"), 2)
    );
    assert_eq!(beta_hit.lexical_rank(), Some(2));
    assert_eq!(
        beta_hit.fusion().unwrap().contributions()[1].rank(),
        2,
        "duplicate chunks of alpha do not consume another page rank",
    );
}

#[test]
fn hybrid_rank_collapses_before_fusion_and_uses_the_python_candidate_weights() {
    let mut vectors = Vec::new();
    let mut ids = Vec::new();
    for index in 0..11_u32 {
        let cosine = 0.99_f32 - index as f32 * 0.01;
        vectors.push(vec![cosine, (1.0 - cosine * cosine).sqrt()]);
        let name = if index == 10 {
            "z-buried".to_owned()
        } else {
            format!("dense-{index:02}")
        };
        ids.push(ChunkId::new(
            page(&format!("/fictional/private/{name}.md")),
            0,
        ));
    }
    let candidates = ids
        .into_iter()
        .zip(&vectors)
        .map(|(id, vector)| DenseCandidate::new(id, vector))
        .collect::<Vec<_>>();
    let dense = dense_rank(&[1.0, 0.0], &candidates).unwrap();
    let vectorless = page("/fictional/private/vectorless.md");
    let buried = page("/fictional/private/z-buried.md");

    let lexical = [
        LexicalCandidate::new(ChunkId::new(vectorless, 4)),
        LexicalCandidate::new(ChunkId::new(buried.clone(), 7)),
    ];
    let hits = hybrid_rank(&dense, &lexical, 3).unwrap();

    assert_eq!(hits[0].page(), &buried);
    assert_eq!(hits[0].dense_chunk(), &ChunkId::new(buried.clone(), 0));
    assert_eq!(
        hits[0].lexical_chunk(),
        Some(&ChunkId::new(buried.clone(), 7))
    );
    assert_eq!(hits[0].selected_chunk(), &ChunkId::new(buried.clone(), 7));
    assert_eq!(hits[0].dense_rank(), 11);
    assert_eq!(hits[0].lexical_rank(), Some(2));
    let fusion = hits[0].fusion().expect("a usable lexical rank enables RRF");
    assert_eq!(fusion.contributions()[0].rank(), 11);
    assert_eq!(
        fusion.contributions()[1].rank(),
        1,
        "vectorless lexical pages are removed before RRF, matching Python",
    );
    assert_eq!(hits[0].score().get(), 0.89);
}

#[test]
fn hybrid_rank_falls_back_to_dense_when_no_lexical_page_has_a_vector() {
    let first = [1.0, 0.0];
    let second = [0.8, 0.6];
    let candidates = vec![
        DenseCandidate::new(ChunkId::new(page("/fictional/private/first.md"), 0), &first),
        DenseCandidate::new(
            ChunkId::new(page("/fictional/private/second.md"), 0),
            &second,
        ),
    ];
    let dense = dense_rank(&first, &candidates).unwrap();

    let hits = hybrid_rank(
        &dense,
        &[LexicalCandidate::new(ChunkId::new(
            page("/fictional/private/vectorless.md"),
            9,
        ))],
        1,
    )
    .unwrap();

    assert_eq!(hits[0].page(), &page("/fictional/private/first.md"));
    assert_eq!(hits[0].dense_rank(), 1);
    assert_eq!(hits[0].lexical_rank(), None);
    assert_eq!(hits[0].lexical_chunk(), None);
    assert_eq!(hits[0].selected_chunk(), hits[0].dense_chunk());
    assert_eq!(hits[0].fusion(), None);
}
