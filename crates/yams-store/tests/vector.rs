use std::collections::{BTreeMap, BTreeSet};
use std::str::FromStr;

use rusqlite::Connection;
use tempfile::tempdir;
use yams_embed::{Embedding, EmbeddingRole};
use yams_store::{
    StoreHome, VectorCache, VectorError, VectorInsert, VectorKey, VectorMutationLease, vector_key,
};

const MODEL: &str = "fixture-model-v1";

fn embedding(values: &[f32]) -> Embedding {
    Embedding::new(values.to_vec()).unwrap()
}

fn insert<'a>(
    key: VectorKey,
    signature: &'a str,
    role: EmbeddingRole,
    text: &'a str,
    embedding: &'a Embedding,
) -> VectorInsert<'a> {
    VectorInsert::new(key, signature, role, text, embedding)
}

fn keys(values: impl IntoIterator<Item = VectorKey>) -> BTreeSet<VectorKey> {
    values.into_iter().collect()
}

fn raw_connection(home: &StoreHome) -> Connection {
    Connection::open(home.vectors_path()).unwrap()
}

#[test]
fn vector_key_covers_field_boundaries_model_role_and_exact_text() {
    let passage = vector_key("ab", EmbeddingRole::Passage, "c").unwrap();

    assert_ne!(
        passage,
        vector_key("a", EmbeddingRole::Passage, "bc").unwrap()
    );
    assert_ne!(
        passage,
        vector_key("other", EmbeddingRole::Passage, "c").unwrap()
    );
    assert_ne!(
        passage,
        vector_key("ab", EmbeddingRole::Query, "c").unwrap()
    );
    assert_ne!(
        passage,
        vector_key("ab", EmbeddingRole::Passage, "C").unwrap()
    );
    assert_eq!(
        passage.to_string(),
        "e7ea7b793a6066358f160207b857c72cc1f447659518dc9648b22bd7ddc4c03f"
    );
    assert_eq!(passage.to_string().len(), 64);
    assert_eq!(VectorKey::from_str(&passage.to_string()).unwrap(), passage);
    assert!(matches!(
        vector_key("", EmbeddingRole::Passage, "c"),
        Err(VectorError::EmptyModelSignature)
    ));
}

#[test]
fn cache_reopens_with_byte_identical_embedding_and_metadata() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let value = embedding(&[3.0, 4.0]);
    let original_bytes = value.to_le_bytes();
    let key = vector_key(MODEL, EmbeddingRole::Passage, "Alpha").unwrap();

    {
        let mut cache = VectorCache::open(&home).unwrap();
        cache
            .insert_batch(&[insert(key, MODEL, EmbeddingRole::Passage, "Alpha", &value)])
            .unwrap();
    }

    let cache = VectorCache::open(&home).unwrap();
    let found = cache.get_many(&keys([key])).unwrap();
    assert_eq!(found.len(), 1);
    let found = &found[&key];
    assert_eq!(found.key(), key);
    assert_eq!(found.model_signature(), MODEL);
    assert_eq!(found.dimensions(), 2);
    assert_eq!(found.embedding().to_le_bytes(), original_bytes);
}

#[test]
fn insertion_rejects_bad_keys_and_signatures_before_writing() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let value = embedding(&[1.0, 0.0]);
    let alpha = vector_key(MODEL, EmbeddingRole::Passage, "alpha").unwrap();
    let beta = vector_key(MODEL, EmbeddingRole::Passage, "beta").unwrap();
    let mut cache = VectorCache::open(&home).unwrap();

    let error = cache
        .insert_batch(&[insert(beta, MODEL, EmbeddingRole::Passage, "alpha", &value)])
        .unwrap_err();
    assert!(matches!(
        error,
        VectorError::KeyMismatch {
            supplied,
            expected
        } if supplied == beta && expected == alpha
    ));

    let error = cache
        .insert_batch(&[insert(alpha, "", EmbeddingRole::Passage, "alpha", &value)])
        .unwrap_err();
    assert!(matches!(error, VectorError::EmptyModelSignature));
    assert_eq!(
        cache.missing(&keys([alpha, beta])).unwrap(),
        keys([alpha, beta])
    );
}

#[test]
fn identical_duplicates_are_idempotent_but_collisions_never_overwrite() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let original = embedding(&[1.0, 0.0]);
    let different = embedding(&[0.0, 1.0]);
    let key = vector_key(MODEL, EmbeddingRole::Passage, "alpha").unwrap();
    let mut cache = VectorCache::open(&home).unwrap();

    cache
        .insert_batch(&[
            insert(key, MODEL, EmbeddingRole::Passage, "alpha", &original),
            insert(key, MODEL, EmbeddingRole::Passage, "alpha", &original),
        ])
        .unwrap();

    let error = cache
        .insert_batch(&[insert(
            key,
            MODEL,
            EmbeddingRole::Passage,
            "alpha",
            &different,
        )])
        .unwrap_err();
    assert!(matches!(error, VectorError::VectorCollision { key: found } if found == key));
    assert_eq!(
        cache.get_many(&keys([key])).unwrap()[&key]
            .embedding()
            .to_le_bytes(),
        original.to_le_bytes()
    );
}

#[test]
fn different_persisted_metadata_is_a_collision_and_is_not_overwritten() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let value = embedding(&[1.0, 0.0]);
    let key = vector_key(MODEL, EmbeddingRole::Passage, "alpha").unwrap();
    let mut cache = VectorCache::open(&home).unwrap();
    {
        let raw = raw_connection(&home);
        raw.execute(
            "INSERT INTO vectors(hash, model_signature, dimensions, bytes) \
             VALUES (?1, 'different-model', 2, ?2)",
            (key.to_string(), value.to_le_bytes()),
        )
        .unwrap();
    }

    let error = cache
        .insert_batch(&[insert(key, MODEL, EmbeddingRole::Passage, "alpha", &value)])
        .unwrap_err();

    assert!(matches!(error, VectorError::VectorCollision { key: found } if found == key));
    let found = cache.get_many(&keys([key])).unwrap();
    assert_eq!(found[&key].model_signature(), "different-model");
}

#[test]
fn a_collision_rolls_back_every_new_vector_in_the_batch() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let original = embedding(&[1.0, 0.0]);
    let different = embedding(&[0.0, 1.0]);
    let existing = vector_key(MODEL, EmbeddingRole::Passage, "existing").unwrap();
    let fresh = vector_key(MODEL, EmbeddingRole::Passage, "fresh").unwrap();
    let mut cache = VectorCache::open(&home).unwrap();
    cache
        .insert_batch(&[insert(
            existing,
            MODEL,
            EmbeddingRole::Passage,
            "existing",
            &original,
        )])
        .unwrap();

    let error = cache
        .insert_batch(&[
            insert(fresh, MODEL, EmbeddingRole::Passage, "fresh", &original),
            insert(
                existing,
                MODEL,
                EmbeddingRole::Passage,
                "existing",
                &different,
            ),
        ])
        .unwrap_err();

    assert!(matches!(error, VectorError::VectorCollision { key } if key == existing));
    assert_eq!(cache.missing(&keys([fresh])).unwrap(), keys([fresh]));
}

#[test]
fn missing_and_get_many_are_key_ordered_and_omit_absent_rows() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let alpha_value = embedding(&[1.0, 0.0]);
    let beta_value = embedding(&[0.0, 1.0]);
    let alpha = vector_key(MODEL, EmbeddingRole::Passage, "alpha").unwrap();
    let beta = vector_key(MODEL, EmbeddingRole::Passage, "beta").unwrap();
    let absent = vector_key(MODEL, EmbeddingRole::Passage, "absent").unwrap();
    let mut cache = VectorCache::open(&home).unwrap();
    cache
        .insert_batch(&[
            insert(beta, MODEL, EmbeddingRole::Passage, "beta", &beta_value),
            insert(alpha, MODEL, EmbeddingRole::Passage, "alpha", &alpha_value),
        ])
        .unwrap();

    let requested = keys([beta, absent, alpha]);
    assert_eq!(cache.missing(&requested).unwrap(), keys([absent]));

    let found = cache.get_many(&requested).unwrap();
    assert_eq!(
        found.keys().copied().collect::<Vec<_>>(),
        keys([alpha, beta]).into_iter().collect::<Vec<_>>()
    );
    assert_eq!(
        found[&alpha].embedding().to_le_bytes(),
        alpha_value.to_le_bytes()
    );
    assert_eq!(
        found[&beta].embedding().to_le_bytes(),
        beta_value.to_le_bytes()
    );
    let expected: BTreeMap<_, _> = [(alpha, MODEL), (beta, MODEL)].into_iter().collect();
    assert_eq!(
        found
            .iter()
            .map(|(key, value)| (*key, value.model_signature()))
            .collect::<BTreeMap<_, _>>(),
        expected
    );
}

#[test]
fn malformed_persisted_dimensions_and_blobs_are_refused() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let value = embedding(&[1.0, 0.0]);
    let key = vector_key(MODEL, EmbeddingRole::Passage, "alpha").unwrap();
    {
        let mut cache = VectorCache::open(&home).unwrap();
        cache
            .insert_batch(&[insert(key, MODEL, EmbeddingRole::Passage, "alpha", &value)])
            .unwrap();
    }
    {
        let raw = raw_connection(&home);
        raw.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        raw.execute(
            "UPDATE vectors SET dimensions = 3 WHERE hash = ?1",
            [key.to_string()],
        )
        .unwrap();
    }
    {
        let cache = VectorCache::open(&home).unwrap();
        let error = cache.get_many(&keys([key])).unwrap_err();
        assert!(matches!(
            error,
            VectorError::InvalidStoredEmbedding { key: found, .. } if found == key
        ));
    }
    {
        let raw = raw_connection(&home);
        raw.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        raw.execute(
            "UPDATE vectors SET dimensions = 2, bytes = ?2 WHERE hash = ?1",
            (key.to_string(), f32::NAN.to_le_bytes().repeat(2)),
        )
        .unwrap();
    }
    let cache = VectorCache::open(&home).unwrap();
    let error = cache.get_many(&keys([key])).unwrap_err();
    assert!(matches!(
        error,
        VectorError::InvalidStoredEmbedding { key: found, .. } if found == key
    ));
}

#[test]
fn missing_ignores_an_unrelated_malformed_hash_outside_the_requested_keys() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let value = embedding(&[1.0]);
    let stored = vector_key(MODEL, EmbeddingRole::Passage, "stored").unwrap();
    let requested = vector_key(MODEL, EmbeddingRole::Passage, "requested").unwrap();
    {
        let mut cache = VectorCache::open(&home).unwrap();
        cache
            .insert_batch(&[insert(
                stored,
                MODEL,
                EmbeddingRole::Passage,
                "stored",
                &value,
            )])
            .unwrap();
    }
    {
        let raw = raw_connection(&home);
        raw.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        raw.execute("UPDATE vectors SET hash = 'not-a-key'", [])
            .unwrap();
    }

    let cache = VectorCache::open(&home).unwrap();
    assert_eq!(
        cache.missing(&keys([requested])).unwrap(),
        keys([requested])
    );
    assert!(cache.missing(&BTreeSet::new()).unwrap().is_empty());
}

#[test]
fn sqlite_constraint_failures_remain_typed() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let value = embedding(&[1.0]);
    let key = vector_key(MODEL, EmbeddingRole::Passage, "alpha").unwrap();
    let mut cache = VectorCache::open(&home).unwrap();
    {
        let raw = raw_connection(&home);
        raw.execute_batch(
            "CREATE TRIGGER reject_vector_insert \
             BEFORE INSERT ON vectors \
             BEGIN SELECT RAISE(ABORT, 'fixture constraint'); END",
        )
        .unwrap();
    }

    let error = cache
        .insert_batch(&[insert(key, MODEL, EmbeddingRole::Passage, "alpha", &value)])
        .unwrap_err();

    assert!(matches!(error, VectorError::Constraint { .. }));
    assert_eq!(cache.missing(&keys([key])).unwrap(), keys([key]));
}

#[test]
fn sweep_keeps_exact_references_and_reports_deterministic_counts() {
    let directory = tempdir().unwrap();
    let home = StoreHome::new(directory.path());
    let value = embedding(&[1.0]);
    let alpha = vector_key(MODEL, EmbeddingRole::Passage, "alpha").unwrap();
    let beta = vector_key(MODEL, EmbeddingRole::Passage, "beta").unwrap();
    let gamma = vector_key(MODEL, EmbeddingRole::Passage, "gamma").unwrap();
    let unknown = vector_key(MODEL, EmbeddingRole::Passage, "unknown").unwrap();
    let mut cache = VectorCache::open(&home).unwrap();
    cache
        .insert_batch(&[
            insert(alpha, MODEL, EmbeddingRole::Passage, "alpha", &value),
            insert(beta, MODEL, EmbeddingRole::Passage, "beta", &value),
            insert(gamma, MODEL, EmbeddingRole::Passage, "gamma", &value),
        ])
        .unwrap();
    let lease = VectorMutationLease::acquire(&home).unwrap();

    let report = cache
        .sweep_except(&lease, &keys([gamma, alpha, unknown]))
        .unwrap();

    assert_eq!(report.kept, 2);
    assert_eq!(report.removed, 1);
    assert_eq!(
        cache.missing(&keys([alpha, beta, gamma])).unwrap(),
        keys([beta])
    );
}

#[test]
fn sweep_refuses_a_lease_from_a_different_store_home() {
    let directory = tempdir().unwrap();
    let first_home = StoreHome::new(directory.path().join("first"));
    let second_home = StoreHome::new(directory.path().join("second"));
    let lease = VectorMutationLease::acquire(&first_home).unwrap();
    let mut second_cache = VectorCache::open(&second_home).unwrap();

    let error = second_cache
        .sweep_except(&lease, &BTreeSet::new())
        .unwrap_err();

    assert!(matches!(error, VectorError::WrongMutationLease { .. }));
}
