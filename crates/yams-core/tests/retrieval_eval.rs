//! Frozen fictional retrieval cases for the shipped ranking constants.
//!
//! These pages and queries are invented. Change ranking only when this set is
//! updated in the same change.

use yams_core::{
    ChunkId, ChunkMetadata, DenseCandidate, ExitCode, GateMode, GateReason, LexicalScore, PageId,
    PageLabels, PageMetadata, SearchRequest, SelectionConfig, SnippetStatistics, TermFrequency,
    compose_search,
};

fn page(path: &str) -> PageId {
    PageId::from_canonical_path(path).expect("fictional eval paths are canonical")
}

fn lantern_corpus() -> (
    Vec<PageMetadata>,
    Vec<ChunkMetadata>,
    Vec<[f32; 2]>,
    SnippetStatistics,
) {
    let lantern = page("/fictional/shared/lantern.md");
    let moss = page("/fictional/shared/moss.md");
    let pages = vec![
        PageMetadata::new(
            lantern.clone(),
            "lantern",
            PageLabels::new(Some("shared"), Some("current"), None),
        ),
        PageMetadata::new(
            moss.clone(),
            "moss",
            PageLabels::new(Some("shared"), Some("historical"), None),
        ),
    ];
    let chunks = vec![
        ChunkMetadata::new(
            ChunkId::new(lantern, 0),
            "the lantern map marks the east quay",
        ),
        ChunkMetadata::new(ChunkId::new(moss, 0), "moss covers the unused cellar"),
    ];
    let vectors = vec![[1.0, 0.0], [0.2, 0.98]];
    let statistics = SnippetStatistics {
        total_chunks: 2,
        frequencies: vec![
            TermFrequency {
                term: "lantern".into(),
                matching_chunks: 1,
            },
            TermFrequency {
                term: "map".into(),
                matching_chunks: 1,
            },
        ],
    };
    (pages, chunks, vectors, statistics)
}

#[test]
fn eval_hit_returns_the_lantern_page() {
    let (pages, chunks, vectors, statistics) = lantern_corpus();
    let dense = [
        DenseCandidate::new(chunks[0].id().clone(), &vectors[0]),
        DenseCandidate::new(chunks[1].id().clone(), &vectors[1]),
    ];
    let lexical = [LexicalScore::new(chunks[0].id().clone(), -1.2, 1)];
    let response = compose_search(SearchRequest::new(
        "lantern map",
        &[1.0, 0.0],
        &pages,
        &chunks,
        &dense,
        &lexical,
        &statistics,
        SelectionConfig::new(1, 0.0, 1.0, GateMode::Apply).unwrap(),
    ))
    .unwrap();

    assert_eq!(response.exit_code(), ExitCode::Ok);
    assert_eq!(response.hits()[0].path(), "/fictional/shared/lantern.md");
    assert!(!response.hits()[0].exact());
}

#[test]
fn eval_miss_stays_empty_when_nothing_is_similar() {
    let miss_statistics = SnippetStatistics {
        total_chunks: 0,
        frequencies: vec![TermFrequency {
            term: "rivet".into(),
            matching_chunks: 0,
        }],
    };
    let response = compose_search(SearchRequest::new(
        "rivet",
        &[-1.0, 0.0],
        &[],
        &[],
        &[],
        &[],
        &miss_statistics,
        SelectionConfig::default(),
    ))
    .unwrap();

    assert_eq!(response.exit_code(), ExitCode::Empty);
    assert!(response.hits().is_empty());
}

#[test]
fn eval_gate_suppresses_a_weak_best_hit() {
    let (pages, chunks, _vectors, statistics) = lantern_corpus();
    let weak = [0.30_f32, 0.95];
    let other = [0.25_f32, 0.97];
    let dense = [
        DenseCandidate::new(chunks[0].id().clone(), &weak),
        DenseCandidate::new(chunks[1].id().clone(), &other),
    ];
    let response = compose_search(SearchRequest::new(
        "lantern map",
        &[1.0, 0.0],
        &pages,
        &chunks,
        &dense,
        &[],
        &statistics,
        SelectionConfig::default(),
    ))
    .unwrap();

    assert_eq!(response.exit_code(), ExitCode::Unsure);
    assert!(response.hits().is_empty());
    assert_eq!(
        response.explanation().gate().map(|gate| gate.reason()),
        Some(GateReason::Floor)
    );
}

#[test]
fn eval_exact_identifier_marks_the_rescued_page() {
    let identifier = page("/fictional/shared/ticket.md");
    let filler = page("/fictional/shared/filler.md");
    let pages = [
        PageMetadata::new(identifier.clone(), "ticket", PageLabels::default()),
        PageMetadata::new(filler.clone(), "filler", PageLabels::default()),
    ];
    let chunks = [
        ChunkMetadata::new(ChunkId::new(identifier.clone(), 0), "ticket ID_42 is open"),
        ChunkMetadata::new(ChunkId::new(filler, 0), "unrelated filler prose"),
    ];
    let ticket = [0.9_f32, 0.1];
    let other = [1.0_f32, 0.0];
    let dense = [
        DenseCandidate::new(chunks[1].id().clone(), &other),
        DenseCandidate::new(chunks[0].id().clone(), &ticket),
    ];
    let statistics = SnippetStatistics {
        total_chunks: 2,
        frequencies: vec![TermFrequency {
            term: "id_42".into(),
            matching_chunks: 1,
        }],
    };
    let response = compose_search(SearchRequest::new(
        "ID_42",
        &[1.0, 0.0],
        &pages,
        &chunks,
        &dense,
        &[LexicalScore::new(chunks[0].id().clone(), -0.5, 1)],
        &statistics,
        SelectionConfig::default(),
    ))
    .unwrap();

    assert_eq!(response.exit_code(), ExitCode::Ok);
    assert!(
        response
            .hits()
            .iter()
            .any(|hit| hit.path() == "/fictional/shared/ticket.md" && hit.exact()),
        "{response:?}"
    );
}
