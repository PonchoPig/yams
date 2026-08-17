use std::path::{Path, PathBuf};

#[cfg(unix)]
use std::ffi::OsString;
#[cfg(unix)]
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use tempfile::TempDir;
use yams_core::{Corpus, CorpusKind, scan_corpora};
use yams_embed::{Embedding, EmbeddingRole, FakeEmbedder};
use yams_store::{
    ManagementError, StoreHome, SweepReport, SyncMode, VectorCache, VectorInsert, inventory,
    open_index, project_inventory, quarantine_vectors, reindex, stats, synchronize, vector_key,
};

struct Fixture {
    _directory: TempDir,
    root: PathBuf,
    home: StoreHome,
}

impl Fixture {
    fn new() -> Self {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("fictional-project");
        std::fs::create_dir_all(root.join(".agents/memory")).unwrap();
        std::fs::write(
            root.join(".agents/memory/alpha.md"),
            "---\ntitle: Alpha\n---\n\nalpha token\n",
        )
        .unwrap();
        Self {
            home: StoreHome::new(directory.path().join("state")),
            root,
            _directory: directory,
        }
    }

    fn index(&self) -> PathBuf {
        self.home.project_path(&self.root).unwrap()
    }
}

#[test]
fn inventory_is_read_only_and_does_not_create_missing_store_paths() {
    let fixture = Fixture::new();
    let before = fixture.home.vectors_path().parent().unwrap().to_path_buf();

    let result = inventory(&fixture.home);

    assert!(result.unwrap().is_empty());
    assert!(!before.exists());
}

#[test]
fn open_index_is_read_only_and_reports_metadata_without_moving_root_guess() {
    let fixture = Fixture::new();
    let corpus =
        Corpus::validated(&fixture.root.join(".agents/memory"), CorpusKind::Shared).unwrap();
    let scan = scan_corpora(&[corpus]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &scan,
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    let index = open_index(&fixture.index()).unwrap();
    assert_eq!(
        index.root(),
        Some(fixture.root.canonicalize().unwrap().as_path())
    );
    assert_eq!(index.page_count(), 1);
    assert_eq!(index.chunk_count(), 1);
    assert_eq!(index.generation(), 1);
}

#[test]
fn project_inventory_marks_current_and_keeps_unknown_or_invalid_files_visible() {
    let fixture = Fixture::new();
    let corpus =
        Corpus::validated(&fixture.root.join(".agents/memory"), CorpusKind::Shared).unwrap();
    let scan = scan_corpora(&[corpus]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &scan,
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    std::fs::write(
        fixture.home.indexes_dir().join("not-an-index.sqlite3"),
        b"junk",
    )
    .unwrap();

    let projects = project_inventory(&fixture.home, Some(&fixture.root)).unwrap();
    assert_eq!(projects.projects.len(), 1);
    assert!(projects.projects[0].current);
    assert_eq!(projects.unreadable.len(), 1);
}

#[test]
fn stats_allows_gc_and_reindex_to_reopen_the_vector_cache() {
    let fixture = Fixture::new();
    let corpus =
        Corpus::validated(&fixture.root.join(".agents/memory"), CorpusKind::Shared).unwrap();
    let scan = scan_corpora(&[corpus]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &scan,
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();
    let before = stats(&fixture.home, &fixture.root).unwrap();
    assert_eq!(before.index.page_count(), 1);
    assert_eq!(before.index.chunk_count(), 1);
    assert_eq!(yams_store::gc(&fixture.home).unwrap().removed, 0);
    let report = reindex(&fixture.home, &fixture.root, &scan, &mut embedder).unwrap();
    assert_eq!(report.generation, 2);
    assert_eq!(
        stats(&fixture.home, &fixture.root)
            .unwrap()
            .index
            .page_count(),
        1
    );
}

#[test]
fn gc_refuses_unknown_indexes_and_preserves_new_keys_after_initial_snapshot() {
    let fixture = Fixture::new();
    let corpus =
        Corpus::validated(&fixture.root.join(".agents/memory"), CorpusKind::Shared).unwrap();
    let scan = scan_corpora(&[corpus]);
    let mut embedder = FakeEmbedder::new();
    synchronize(
        &fixture.home,
        &fixture.root,
        &scan,
        &mut embedder,
        SyncMode::Incremental,
    )
    .unwrap();

    let key = vector_key("fake-token-v1", EmbeddingRole::Passage, "orphan").unwrap();
    let mut cache = VectorCache::open(&fixture.home).unwrap();
    let embedding = Embedding::new(vec![0.25; 384]).unwrap();
    cache
        .insert_batch(&[VectorInsert::new(
            key,
            "fake-token-v1",
            EmbeddingRole::Passage,
            "orphan",
            &embedding,
        )])
        .unwrap();
    drop(cache);

    let report = yams_store::gc(&fixture.home).unwrap();
    assert!(report.removed >= 1);

    std::fs::write(
        fixture.home.indexes_dir().join("unknown.sqlite3"),
        b"SQLite format 3\0",
    )
    .unwrap();
    assert!(matches!(
        yams_store::gc(&fixture.home),
        Err(ManagementError::IncompleteInventory { .. })
    ));
}

fn sync_project(home: &StoreHome, root: &Path) {
    let corpus = Corpus::validated(&root.join(".agents/memory"), CorpusKind::Shared).unwrap();
    let scan = scan_corpora(&[corpus]);
    synchronize(
        home,
        root,
        &scan,
        &mut FakeEmbedder::new(),
        SyncMode::Incremental,
    )
    .unwrap();
}

fn cache_sibling(home: &StoreHome, suffix: &str) -> PathBuf {
    PathBuf::from(format!("{}{}", home.vectors_path().display(), suffix))
}

#[cfg(unix)]
fn byte_exact_sibling(path: &Path, suffix: &[u8]) -> PathBuf {
    let mut bytes = path.as_os_str().as_bytes().to_vec();
    bytes.extend_from_slice(suffix);
    PathBuf::from(OsString::from_vec(bytes))
}

fn damage_vector_cache(home: &StoreHome) {
    for suffix in ["-wal", "-shm", "-journal"] {
        let _ = std::fs::remove_file(cache_sibling(home, suffix));
    }
    std::fs::write(home.vectors_path(), b"damaged cache").unwrap();
}

#[test]
fn zero_index_gc_sweeps_the_initial_orphan_snapshot() {
    let fixture = Fixture::new();
    assert!(inventory(&fixture.home).unwrap().is_empty());
    let embedding = Embedding::new(vec![0.25; 384]).unwrap();
    let orphans = ["orphan-one", "orphan-two", "orphan-three"];
    let mut cache = VectorCache::open(&fixture.home).unwrap();
    cache
        .insert_batch(
            &orphans
                .iter()
                .map(|text| {
                    VectorInsert::new(
                        vector_key("fake-token-v1", EmbeddingRole::Passage, text).unwrap(),
                        "fake-token-v1",
                        EmbeddingRole::Passage,
                        text,
                        &embedding,
                    )
                })
                .collect::<Vec<_>>(),
        )
        .unwrap();
    drop(cache);

    let report = yams_store::gc(&fixture.home).unwrap();

    assert_eq!(report.removed, orphans.len());
    assert_eq!(report.kept, 0);
    assert!(
        VectorCache::open(&fixture.home)
            .unwrap()
            .keys()
            .unwrap()
            .is_empty(),
        "an authoritative empty live set must leave no cached vector behind"
    );
}

#[test]
fn quarantine_moves_only_an_unreadable_vector_cache() {
    let fixture = Fixture::new();
    let path = fixture.home.vectors_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"not sqlite").unwrap();
    let quarantined = quarantine_vectors(&fixture.home).unwrap();
    assert!(!path.exists());
    assert_eq!(std::fs::read(quarantined).unwrap(), b"not sqlite");
}

#[test]
fn quarantine_does_not_overwrite_an_existing_corrupt_sibling() {
    let fixture = Fixture::new();
    let path = fixture.home.vectors_path();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"newer damage").unwrap();
    let first = cache_sibling(&fixture.home, ".corrupt");
    std::fs::write(&first, b"earlier quarantine").unwrap();

    let quarantined = quarantine_vectors(&fixture.home).unwrap();

    assert_eq!(std::fs::read(&first).unwrap(), b"earlier quarantine");
    assert_eq!(std::fs::read(&quarantined).unwrap(), b"newer damage");
    assert_ne!(quarantined, first);
    assert!(!path.exists());
}

#[cfg(unix)]
#[cfg_attr(
    target_vendor = "apple",
    ignore = "APFS requires valid UTF-8 filenames; exercised by the Linux CI job"
)]
#[test]
fn quarantine_preserves_non_utf8_cache_and_sidecar_path_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let home = StoreHome::new(
        directory
            .path()
            .join(OsString::from_vec(b"state-\xff".to_vec())),
    );
    let cache = home.vectors_path();
    std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
    std::fs::write(&cache, b"damaged cache").unwrap();
    for suffix in [b"-wal".as_slice(), b"-shm", b"-journal"] {
        std::fs::write(byte_exact_sibling(&cache, suffix), suffix).unwrap();
    }

    let quarantined = quarantine_vectors(&home).unwrap();
    let expected = byte_exact_sibling(&cache, b".corrupt");

    assert_eq!(
        quarantined.as_os_str().as_bytes(),
        expected.as_os_str().as_bytes()
    );
    assert!(!cache.exists());
    assert_eq!(std::fs::read(&quarantined).unwrap(), b"damaged cache");
    for suffix in [b"-wal".as_slice(), b"-shm", b"-journal"] {
        let source = byte_exact_sibling(&cache, suffix);
        let destination = byte_exact_sibling(&expected, suffix);
        assert_eq!(
            source.as_os_str().as_bytes(),
            [cache.as_os_str().as_bytes(), suffix].concat()
        );
        assert_eq!(
            destination.as_os_str().as_bytes(),
            [expected.as_os_str().as_bytes(), suffix].concat()
        );
        assert!(!source.exists());
        assert_eq!(std::fs::read(destination).unwrap(), suffix);
    }
}

#[cfg(unix)]
#[cfg_attr(
    target_vendor = "apple",
    ignore = "APFS requires valid UTF-8 filenames; exercised by the Linux CI job"
)]
#[test]
fn reject_sidecars_preserves_non_utf8_index_path_bytes() {
    let directory = tempfile::tempdir().unwrap();
    let home = StoreHome::new(
        directory
            .path()
            .join(OsString::from_vec(b"state-\xff".to_vec())),
    );
    let index = home.indexes_dir().join("index.sqlite3");
    std::fs::create_dir_all(index.parent().unwrap()).unwrap();
    std::fs::write(&index, b"SQLite format 3\0").unwrap();
    let sidecar = byte_exact_sibling(&index, b"-wal");
    std::fs::write(&sidecar, b"active wal").unwrap();

    let error = match open_index(&index) {
        Ok(_) => panic!("index with a sidecar was accepted"),
        Err(error) => error,
    };

    let ManagementError::UnsafeSidecar { path } = error else {
        panic!("expected UnsafeSidecar, got {error:?}");
    };
    assert_eq!(
        path.as_os_str().as_bytes(),
        [index.as_os_str().as_bytes(), b"-wal"].concat()
    );
    assert_eq!(path.as_os_str().as_bytes(), sidecar.as_os_str().as_bytes());
}

#[test]
fn gc_without_a_vector_cache_reports_nothing_and_creates_no_store_paths() {
    let fixture = Fixture::new();
    let private = fixture.home.vectors_path().parent().unwrap().to_path_buf();

    let report = yams_store::gc(&fixture.home).unwrap();

    assert_eq!(report, SweepReport::default());
    assert!(!private.exists());
}

#[test]
fn gc_surfaces_a_failed_quarantine_instead_of_the_read_that_triggered_it() {
    let fixture = Fixture::new();
    sync_project(&fixture.home, &fixture.root);
    damage_vector_cache(&fixture.home);
    // `.corrupt` plus `.corrupt-2`..`.corrupt-20` fills every destination
    // MAX_QUARANTINES allows, so preservation has nowhere left to go.
    for suffix in std::iter::once(".corrupt".to_owned())
        .chain((2..=20).map(|ordinal| format!(".corrupt-{ordinal}")))
    {
        std::fs::write(cache_sibling(&fixture.home, &suffix), b"earlier quarantine").unwrap();
    }

    let error = yams_store::gc(&fixture.home).unwrap_err();

    assert!(matches!(error, ManagementError::QuarantineLimit { .. }));
    assert_eq!(
        std::fs::read(fixture.home.vectors_path()).unwrap(),
        b"damaged cache"
    );
}

#[test]
fn gc_keeps_the_read_failure_of_a_cache_that_needs_no_quarantine() {
    let fixture = Fixture::new();
    sync_project(&fixture.home, &fixture.root);
    {
        let raw = rusqlite::Connection::open(fixture.home.vectors_path()).unwrap();
        raw.pragma_update(None, "ignore_check_constraints", "ON")
            .unwrap();
        raw.execute("UPDATE vectors SET hash = 'not-a-key'", [])
            .unwrap();
    }

    let error = yams_store::gc(&fixture.home).unwrap_err();

    assert!(matches!(error, ManagementError::InvalidVectorKey { .. }));
    assert!(!cache_sibling(&fixture.home, ".corrupt").exists());
}

#[test]
fn gc_quarantines_a_corrupt_cache_before_recreating_it() {
    let fixture = Fixture::new();
    sync_project(&fixture.home, &fixture.root);
    damage_vector_cache(&fixture.home);

    let report = yams_store::gc(&fixture.home).unwrap();
    assert_eq!(report.removed, 0);
    assert!(fixture.home.vectors_path().exists());
    assert!(
        std::fs::read_dir(fixture.home.vectors_path().parent().unwrap())
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with("vectors.sqlite3.corrupt"))
    );
}

#[test]
fn management_reports_missing_index_as_typed_error() {
    let fixture = Fixture::new();
    assert!(matches!(
        stats(&fixture.home, &fixture.root),
        Err(ManagementError::MissingIndex { .. })
    ));
}
