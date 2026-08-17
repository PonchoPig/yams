use sha2::{Digest, Sha256};

use crate::{Embedder, Embedding, EmbeddingError};

const FAKE_DIMENSIONS: usize = 32;
const FAKE_SIGNATURE: &str = "fake-token-v1";

/// Deterministic, model-free embeddings for tests and development harnesses.
#[derive(Clone, Copy, Debug, Default)]
pub struct FakeEmbedder;

impl FakeEmbedder {
    pub const fn new() -> Self {
        Self
    }

    fn embed(text: &str) -> Result<Embedding, EmbeddingError> {
        let mut values = vec![0.0; FAKE_DIMENSIONS];
        let mut fallback: Option<(String, usize, f32)> = None;
        for token in text.split_whitespace() {
            let token = token.to_lowercase();
            let digest = Sha256::digest(token.as_bytes());
            let bucket = usize::from(digest[0]) % FAKE_DIMENSIONS;
            let sign = if digest[1] & 1 == 0 { 1.0 } else { -1.0 };
            values[bucket] += sign;
            if fallback
                .as_ref()
                .is_none_or(|(candidate, _, _)| token.as_str() < candidate.as_str())
            {
                fallback = Some((token, bucket, sign));
            }
        }
        if values.iter().all(|value| *value == 0.0)
            && let Some((_, bucket, sign)) = fallback
        {
            values[bucket] = sign;
        }
        Embedding::new(values)
    }
}

impl Embedder for FakeEmbedder {
    fn signature(&self) -> &str {
        FAKE_SIGNATURE
    }

    fn dimensions(&self) -> usize {
        FAKE_DIMENSIONS
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        texts.iter().map(|text| Self::embed(text)).collect()
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        Self::embed(text)
    }
}

#[cfg(test)]
mod tests {
    use super::{Embedder, Embedding, EmbeddingError, FakeEmbedder};

    fn dot(left: &Embedding, right: &Embedding) -> f32 {
        left.values()
            .iter()
            .zip(right.values())
            .map(|(left, right)| left * right)
            .sum()
    }

    #[test]
    fn signed_bag_mapping_is_frozen_without_enabling_a_feature() {
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
        assert_eq!(fake.embed_query("Alpha beta alpha").unwrap(), passages[0]);
    }

    #[test]
    fn a_nonempty_cancelling_bag_has_a_pinned_order_independent_fallback() {
        let mut fake = FakeEmbedder::new();

        let fallback = fake.embed_query("fictional0 fictional12").unwrap();
        let reversed = fake.embed_query("fictional12 fictional0").unwrap();

        assert_eq!(fallback, reversed);
        assert_eq!(fallback.values()[8], 1.0);
        assert_eq!(
            fallback
                .values()
                .iter()
                .filter(|value| **value != 0.0)
                .count(),
            1
        );

        let shared = fake.embed_query("fictional0").unwrap();
        let unrelated = fake.embed_query("alpha").unwrap();
        assert!(dot(&fallback, &shared) > dot(&fallback, &unrelated));
        assert_eq!(
            fake.embed_query(" \n\t"),
            Err(EmbeddingError::ZeroNorm),
            "whitespace-only input does not use the nonempty-token fallback"
        );
    }
}
