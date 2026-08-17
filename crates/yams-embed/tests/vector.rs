use yams_embed::{Embedding, EmbeddingError, EmbeddingRole};

#[cfg(feature = "test-support")]
use yams_embed::Embedder;

#[test]
fn embeddings_are_finite_nonzero_and_normalized() {
    let vector = Embedding::new(vec![3.0, 4.0]).unwrap();

    assert!((vector.values()[0] - 0.6).abs() < 1e-6);
    assert!((vector.values()[1] - 0.8).abs() < 1e-6);
    assert_eq!(vector.dimensions(), 2);
    assert_eq!(Embedding::new(Vec::new()), Err(EmbeddingError::Empty));
    assert_eq!(
        Embedding::new(vec![f32::NAN]),
        Err(EmbeddingError::NonFinite)
    );
    assert_eq!(
        Embedding::new(vec![f32::INFINITY]),
        Err(EmbeddingError::NonFinite)
    );
    assert_eq!(
        Embedding::new(vec![0.0, -0.0]),
        Err(EmbeddingError::ZeroNorm)
    );
}

#[test]
fn little_endian_encoding_round_trips_with_an_expected_dimension() {
    let vector = Embedding::new(vec![3.0, 4.0]).unwrap();
    let bytes = vector.to_le_bytes();

    assert_eq!(bytes.len(), 8);
    assert_eq!(
        &bytes[..4],
        &vector.values()[0].to_le_bytes(),
        "components use little-endian f32 encoding"
    );
    assert_eq!(Embedding::from_le_bytes(&bytes, 2).unwrap(), vector);
}

#[test]
fn decoding_an_already_normalized_vector_preserves_its_exact_f32_bytes() {
    let vector = Embedding::new(vec![377.816_47, 209.987_84]).unwrap();
    let bytes = vector.to_le_bytes();

    let decoded = Embedding::from_le_bytes(&bytes, vector.dimensions()).unwrap();

    assert_eq!(decoded, vector);
    assert_eq!(decoded.to_le_bytes(), bytes);
}

#[test]
fn decoding_rejects_finite_nonzero_vectors_that_are_not_normalized() {
    let bytes = [3.0_f32.to_le_bytes(), 4.0_f32.to_le_bytes()].concat();

    assert_eq!(
        Embedding::from_le_bytes(&bytes, 2),
        Err(EmbeddingError::NotNormalized)
    );
}

#[test]
fn decoding_tolerates_only_the_rounding_error_of_normalized_f32_components() {
    let within_tolerance = 1.0_f32 + f32::EPSILON;
    let within_bytes = within_tolerance.to_le_bytes();
    assert_eq!(
        Embedding::from_le_bytes(&within_bytes, 1)
            .unwrap()
            .to_le_bytes(),
        within_bytes
    );

    let outside_tolerance = 1.0_f32 + 3.0 * f32::EPSILON;
    assert_eq!(
        Embedding::from_le_bytes(&outside_tolerance.to_le_bytes(), 1),
        Err(EmbeddingError::NotNormalized)
    );
}

#[test]
fn decoding_rejects_bad_lengths_dimensions_values_and_size_overflow() {
    let bytes = Embedding::new(vec![3.0, 4.0]).unwrap().to_le_bytes();
    assert_eq!(
        Embedding::from_le_bytes(&bytes, 3),
        Err(EmbeddingError::DimensionMismatch {
            expected: 3,
            actual: 2,
        })
    );

    let mut trailing = 1.0_f32.to_le_bytes().to_vec();
    trailing.push(0);
    assert_eq!(
        Embedding::from_le_bytes(&trailing, 1),
        Err(EmbeddingError::InvalidByteLength {
            expected: 4,
            actual: 5,
        })
    );
    assert_eq!(
        Embedding::from_le_bytes(&f32::NAN.to_le_bytes(), 1),
        Err(EmbeddingError::NonFinite)
    );
    assert_eq!(
        Embedding::from_le_bytes(&0.0_f32.to_le_bytes(), 1),
        Err(EmbeddingError::ZeroNorm)
    );
    assert_eq!(
        Embedding::from_le_bytes(&[], usize::MAX),
        Err(EmbeddingError::DimensionOverflow)
    );
}

#[test]
fn roles_have_stable_cache_labels() {
    assert_eq!(EmbeddingRole::Passage.as_str(), "passage");
    assert_eq!(EmbeddingRole::Query.as_str(), "query");
    assert_ne!(EmbeddingRole::Passage, EmbeddingRole::Query);
}

#[test]
fn cross_boundary_failures_are_typed() {
    assert_eq!(
        EmbeddingError::CardinalityMismatch {
            expected: 2,
            actual: 1,
        }
        .to_string(),
        "embedding cardinality mismatch: expected 2, got 1"
    );
    assert_eq!(
        EmbeddingError::Backend("inference failed".to_owned()).to_string(),
        "embedding backend failed: inference failed"
    );
}

#[cfg(feature = "test-support")]
fn assert_embedder_contract<T: Embedder>() {}

#[cfg(feature = "test-support")]
#[test]
fn fake_embedder_is_public_test_support_and_implements_the_boundary() {
    use yams_embed::FakeEmbedder;

    assert_embedder_contract::<FakeEmbedder>();
    let mut fake = FakeEmbedder::new();

    assert_eq!(fake.signature(), "fake-token-v1");
    assert_eq!(fake.dimensions(), 32);

    let passages = fake
        .embed_passages(&["Alpha\tBETA alpha".to_owned(), "beta".to_owned()])
        .unwrap();
    assert_eq!(passages.len(), 2);

    let first = passages[0].values();
    assert!((first[14] - (-2.0_f32 / 5.0_f32.sqrt())).abs() < 1e-6);
    assert!((first[20] - (1.0_f32 / 5.0_f32.sqrt())).abs() < 1e-6);
    assert_eq!(first.iter().filter(|value| **value != 0.0).count(), 2);
    assert_eq!(
        fake.embed_query("Alpha beta alpha").unwrap(),
        passages[0],
        "query and passage fakes deliberately share behavior"
    );
    assert_eq!(
        fake.embed_passages(&[" \n\t".to_owned()]),
        Err(EmbeddingError::ZeroNorm)
    );
}
