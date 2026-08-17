use thiserror::Error;

const BYTES_PER_COMPONENT: usize = size_of::<f32>();
// A normalized component is rounded once to f32. Its relative error is at
// most half an f32 epsilon; squaring doubles that error. Four epsilons on the
// squared norm conservatively covers both directions and f64 accumulation
// without accepting materially scaled vectors.
const NORMALIZED_SQUARED_NORM_TOLERANCE: f64 = 4.0 * f32::EPSILON as f64;

mod construction_lock;
#[cfg(any(test, feature = "test-support"))]
mod fake;
mod jina;

pub use construction_lock::{
    ConstructionLease, ConstructionLockError, ConstructionNotice, ConstructionWait,
};
#[cfg(feature = "test-support")]
pub use fake::FakeEmbedder;
#[cfg(feature = "test-support")]
pub use jina::build_online_with_endpoint;
pub use jina::{
    JINA_ARTIFACTS_SHA256, JINA_DIMENSIONS, JINA_MAX_LENGTH, JINA_MODEL_ID, JINA_REVISION,
    JinaEmbedder, JinaError,
};

/// One finite, nonzero, L2-normalized embedding vector.
#[derive(Clone, Debug, PartialEq)]
pub struct Embedding {
    values: Vec<f32>,
}

impl Embedding {
    /// Validates and normalizes an embedding vector.
    pub fn new(values: Vec<f32>) -> Result<Self, EmbeddingError> {
        checked_encoded_len(values.len())?;
        let squared_norm = checked_squared_norm(&values)?;
        let norm = squared_norm.sqrt();
        let values = values
            .into_iter()
            .map(|value| (f64::from(value) / norm) as f32)
            .collect();

        Ok(Self { values })
    }

    /// Returns the normalized components.
    pub fn values(&self) -> &[f32] {
        &self.values
    }

    /// Returns the vector dimension.
    pub fn dimensions(&self) -> usize {
        self.values.len()
    }

    /// Encodes normalized components as consecutive little-endian `f32`s.
    pub fn to_le_bytes(&self) -> Vec<u8> {
        let capacity = checked_encoded_len(self.dimensions())
            .expect("Embedding construction checked its encoded size");
        let mut bytes = Vec::with_capacity(capacity);
        for value in &self.values {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    /// Decodes an exact number of consecutive little-endian `f32`s.
    pub fn from_le_bytes(bytes: &[u8], expected_dimensions: usize) -> Result<Self, EmbeddingError> {
        let expected_bytes = checked_encoded_len(expected_dimensions)?;
        if !bytes.len().is_multiple_of(BYTES_PER_COMPONENT) {
            return Err(EmbeddingError::InvalidByteLength {
                expected: expected_bytes,
                actual: bytes.len(),
            });
        }

        let actual_dimensions = bytes.len() / BYTES_PER_COMPONENT;
        if actual_dimensions != expected_dimensions {
            return Err(EmbeddingError::DimensionMismatch {
                expected: expected_dimensions,
                actual: actual_dimensions,
            });
        }

        let values: Vec<f32> = bytes
            .chunks_exact(BYTES_PER_COMPONENT)
            .map(|component| {
                f32::from_le_bytes(
                    component
                        .try_into()
                        .expect("chunks have exactly one f32 of bytes"),
                )
            })
            .collect();
        let squared_norm = checked_squared_norm(&values)?;
        if (squared_norm - 1.0).abs() > NORMALIZED_SQUARED_NORM_TOLERANCE {
            return Err(EmbeddingError::NotNormalized);
        }
        Ok(Self { values })
    }
}

fn checked_encoded_len(dimensions: usize) -> Result<usize, EmbeddingError> {
    dimensions
        .checked_mul(BYTES_PER_COMPONENT)
        .ok_or(EmbeddingError::DimensionOverflow)
}

fn checked_squared_norm(values: &[f32]) -> Result<f64, EmbeddingError> {
    if values.is_empty() {
        return Err(EmbeddingError::Empty);
    }
    if values.iter().any(|value| !value.is_finite()) {
        return Err(EmbeddingError::NonFinite);
    }

    let squared_norm = values
        .iter()
        .map(|value| {
            let value = f64::from(*value);
            value * value
        })
        .sum::<f64>();
    if squared_norm == 0.0 {
        return Err(EmbeddingError::ZeroNorm);
    }
    Ok(squared_norm)
}

/// The semantic role of an embedding input.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum EmbeddingRole {
    Passage,
    Query,
}

impl EmbeddingRole {
    /// Stable label used when deriving content-addressed vector keys.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passage => "passage",
            Self::Query => "query",
        }
    }
}

/// An injectable embedding model boundary.
///
/// Passage results preserve input order and contain exactly one embedding per
/// input. Every result has [`Embedder::dimensions`] components.
pub trait Embedder: Send {
    /// Stable identity for every setting that determines model output.
    fn signature(&self) -> &str;

    /// The exact dimension returned by this embedder.
    fn dimensions(&self) -> usize;

    /// Embeds passages, preserving input order and cardinality.
    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError>;

    /// Embeds one query.
    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError>;
}

/// Validation and inference failures at the embedding boundary.
#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum EmbeddingError {
    #[error("embedding is empty")]
    Empty,

    #[error("embedding contains a non-finite component")]
    NonFinite,

    #[error("embedding has zero norm")]
    ZeroNorm,

    #[error("persisted embedding is not unit normalized")]
    NotNormalized,

    #[error("embedding dimensions overflow the checked byte encoding")]
    DimensionOverflow,

    #[error("embedding dimension mismatch: expected {expected}, got {actual}")]
    DimensionMismatch { expected: usize, actual: usize },

    #[error("embedding byte length mismatch: expected {expected}, got {actual}")]
    InvalidByteLength { expected: usize, actual: usize },

    #[error("embedding cardinality mismatch: expected {expected}, got {actual}")]
    CardinalityMismatch { expected: usize, actual: usize },

    #[error("embedding backend failed: {0}")]
    Backend(String),
}
