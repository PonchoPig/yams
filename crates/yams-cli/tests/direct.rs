use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::thread;
use std::time::Duration;

use yams_cli::{
    BoundedBuffer, InvocationTime, Platform, RuntimeInputs, execute_direct_with_embedder,
};
use yams_core::{Corpus, CorpusKind, ExitCode, scan_corpora};
use yams_embed::{Embedder, Embedding, EmbeddingError};
use yams_store::{StoreHome, SyncMode, open_project, synchronize};

struct NeverEmbed;
impl yams_embed::Embedder for NeverEmbed {
    fn signature(&self) -> &str {
        "never"
    }
    fn dimensions(&self) -> usize {
        1
    }
    fn embed_passages(
        &mut self,
        _: &[String],
    ) -> Result<Vec<yams_embed::Embedding>, EmbeddingError> {
        unreachable!()
    }
    fn embed_query(&mut self, _: &str) -> Result<yams_embed::Embedding, EmbeddingError> {
        unreachable!()
    }
}

struct UnitEmbed;

impl Embedder for UnitEmbed {
    fn signature(&self) -> &str {
        "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"
    }

    fn dimensions(&self) -> usize {
        1
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        texts.iter().map(|_| Embedding::new(vec![1.0])).collect()
    }

    fn embed_query(&mut self, _: &str) -> Result<Embedding, EmbeddingError> {
        Embedding::new(vec![1.0])
    }
}

fn inputs(cwd: &std::path::Path) -> RuntimeInputs {
    RuntimeInputs {
        cwd: cwd.to_owned(),
        home: cwd.join("home"),
        temporary_directory: cwd.join("tmp"),
        uid: 42,
        platform: Platform::MacOs,
    }
}

fn when() -> InvocationTime {
    InvocationTime {
        civil_date: "2026-08-10".into(),
        utc_timestamp: "2026-08-10T00:00:00.000Z".into(),
    }
}

fn replace_chunk_text(home: &StoreHome, project: &std::path::Path, text: &str) {
    let mut connection = open_project(home, project).unwrap();
    let transaction = connection.transaction().unwrap();
    transaction
        .execute("UPDATE chunks SET text = ?1", [text])
        .unwrap();
    transaction.execute("DELETE FROM chunks_fts", []).unwrap();
    transaction
        .execute(
            "INSERT INTO chunks_fts(rowid, text) SELECT id, text FROM chunks",
            [],
        )
        .unwrap();
    transaction.commit().unwrap();
}

#[test]
fn projects_management_is_json_and_does_not_create_store_state() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    fs::create_dir(&project).unwrap();
    let state = temporary.path().join("state");
    let mut embedder = NeverEmbed;
    let completion = execute_direct_with_embedder(
        ["yams", "--projects", "--json"],
        [(OsString::from("YAMS_HOME"), state.clone().into_os_string())],
        &inputs(&project),
        &[],
        &when(),
        &mut embedder,
    );
    assert_eq!(completion.exit_code, ExitCode::Ok);
    let body: serde_json::Value = serde_json::from_str(completion.stdout.trim()).unwrap();
    assert_eq!(body["ok"], true);
    assert_eq!(body["projects"].as_array().unwrap().len(), 0);
    assert!(!state.exists());
}

#[test]
fn search_executes_against_existing_index_and_renders_json() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let corpus_path = project.join(".agents/memory");
    fs::create_dir_all(&corpus_path).unwrap();
    fs::write(
        corpus_path.join("alpha.md"),
        "---\ntitle: Alpha\nstatus: current\n---\n\nalpha search evidence\n",
    )
    .unwrap();
    let state = temporary.path().join("state");
    let home = StoreHome::new(&state);
    let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
    let scan = scan_corpora(&[corpus]);
    let mut indexer = UnitEmbed;
    synchronize(&home, &project, &scan, &mut indexer, SyncMode::Incremental).unwrap();

    let mut query_embedder = UnitEmbed;
    let completion = execute_direct_with_embedder(
        ["yams", "--json", "--no-gate", "alpha"],
        [(OsString::from("YAMS_HOME"), state.into_os_string())],
        &inputs(&project),
        &[],
        &when(),
        &mut query_embedder,
    );

    assert_eq!(completion.exit_code, ExitCode::Ok);
    assert_eq!(completion.stderr, "");
    let body: serde_json::Value = serde_json::from_str(completion.stdout.trim()).unwrap();
    assert_eq!(body.as_array().unwrap().len(), 1);
    assert_eq!(body[0]["name"], "Alpha");
    assert_eq!(
        body[0]["path"],
        corpus_path
            .join("alpha.md")
            .canonicalize()
            .unwrap()
            .to_str()
            .unwrap()
    );
}

#[test]
fn real_direct_text_and_json_overflow_use_the_exact_limit_completion() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let corpus_path = project.join(".agents/memory");
    fs::create_dir_all(&corpus_path).unwrap();
    fs::write(
        corpus_path.join("alpha.md"),
        "---\ntitle: Alpha\nstatus: current\n---\n\nalpha search evidence\n",
    )
    .unwrap();
    let state = temporary.path().join("state");
    let home = StoreHome::new(&state);
    let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
    let scan = scan_corpora(&[corpus]);
    synchronize(
        &home,
        &project,
        &scan,
        &mut UnitEmbed,
        SyncMode::Incremental,
    )
    .unwrap();

    let expected = yams_cli::DirectCompletion {
        exit_code: ExitCode::Operational,
        stdout: String::new(),
        stderr: "yams: output limit\n".to_owned(),
    };
    replace_chunk_text(
        &home,
        &project,
        &"é".repeat(BoundedBuffer::DIRECT_STREAM_CAP / 2),
    );
    let text = execute_direct_with_embedder(
        ["yams", "--full", "--no-gate", "alpha"],
        [(OsString::from("YAMS_HOME"), state.clone().into_os_string())],
        &inputs(&project),
        &[],
        &when(),
        &mut UnitEmbed,
    );
    assert_eq!(text, expected);

    let encoded_units = BoundedBuffer::DIRECT_STREAM_CAP / 6;
    let encoded = "\u{007f}".repeat(encoded_units);
    assert!(encoded.len() < BoundedBuffer::DIRECT_STREAM_CAP);
    replace_chunk_text(&home, &project, &encoded);
    let json = execute_direct_with_embedder(
        ["yams", "--json", "--no-gate", "alpha"],
        [(OsString::from("YAMS_HOME"), state.into_os_string())],
        &inputs(&project),
        &[],
        &when(),
        &mut UnitEmbed,
    );
    assert_eq!(json, expected);
}

#[test]
fn direct_search_appends_a_query_log_record_with_the_injected_timestamp() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let corpus_path = project.join(".agents/memory");
    fs::create_dir_all(&corpus_path).unwrap();
    fs::write(
        corpus_path.join("alpha.md"),
        "---\ntitle: Alpha\nstatus: current\n---\n\nalpha search evidence\n",
    )
    .unwrap();
    let state = temporary.path().join("state");
    let home = StoreHome::new(&state);
    let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
    let scan = scan_corpora(&[corpus]);
    let mut indexer = UnitEmbed;
    synchronize(&home, &project, &scan, &mut indexer, SyncMode::Incremental).unwrap();

    // Opt in to query logging: the log only appends when the file already
    // exists with the private permissions the writer requires.
    fs::create_dir_all(&state).unwrap();
    let log_path = state.join("queries.jsonl");
    fs::File::create(&log_path).unwrap();
    fs::set_permissions(&log_path, fs::Permissions::from_mode(0o600)).unwrap();

    let injected = InvocationTime {
        civil_date: "2026-08-10".into(),
        utc_timestamp: "2026-08-10T12:34:56.789Z".into(),
    };

    let mut query_embedder = UnitEmbed;
    let completion = execute_direct_with_embedder(
        ["yams", "--json", "--no-gate", "alpha"],
        [(OsString::from("YAMS_HOME"), state.clone().into_os_string())],
        &inputs(&project),
        &[],
        &injected,
        &mut query_embedder,
    );

    assert_eq!(completion.exit_code, ExitCode::Ok);
    let logged = fs::read_to_string(&log_path).unwrap();
    assert!(
        logged.contains("\"ts\":\"2026-08-10T12:34:56.789Z\""),
        "expected the injected utc_timestamp verbatim in the log, got: {logged}"
    );
}

#[test]
fn index_skips_private_memory_when_project_inventory_cannot_be_read() {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let shared = project.join(".agents/memory");
    fs::create_dir_all(&shared).unwrap();
    fs::write(
        shared.join("shared.md"),
        "---\ntitle: Shared\nstatus: current\n---\n\nshared-only evidence\n",
    )
    .unwrap();
    let state = temporary.path().join("state");
    let project = project.canonicalize().unwrap();
    let slug: String = project
        .to_str()
        .unwrap()
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect();
    let private = state.join(".claude/projects").join(slug).join("memory");
    fs::create_dir_all(&private).unwrap();
    fs::write(
        private.join("secret.md"),
        "---\ntitle: Secret\nstatus: current\n---\n\nprivate-only evidence\n",
    )
    .unwrap();

    let store = StoreHome::new(&state);
    let indexes = store.indexes_dir();
    fs::create_dir_all(&indexes).unwrap();
    fs::set_permissions(
        store.indexes_dir().parent().unwrap(),
        fs::Permissions::from_mode(0o700),
    )
    .unwrap();
    fs::set_permissions(&indexes, fs::Permissions::from_mode(0o311)).unwrap();

    let index = execute_direct_with_embedder(
        ["yams", "--index", "--json"],
        [(OsString::from("YAMS_HOME"), state.clone().into_os_string())],
        &inputs(&project),
        &[],
        &when(),
        &mut UnitEmbed,
    );
    fs::set_permissions(&indexes, fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(index.exit_code, ExitCode::Operational, "{index:?}");
    assert!(
        index.stdout.contains("index directory is not readable"),
        "inventory failure must fail closed before private discovery: {}",
        index.stdout
    );
}

fn journal_sidecar(path: &std::path::Path) -> std::path::PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-journal");
    std::path::PathBuf::from(name)
}

fn indexed_search_fixture() -> (
    tempfile::TempDir,
    std::path::PathBuf,
    std::path::PathBuf,
    StoreHome,
) {
    let temporary = tempfile::tempdir().unwrap();
    let project = temporary.path().join("project");
    let corpus_path = project.join(".agents/memory");
    fs::create_dir_all(&corpus_path).unwrap();
    fs::write(
        corpus_path.join("alpha.md"),
        "---\ntitle: Alpha\nstatus: current\n---\n\nalpha search evidence\n",
    )
    .unwrap();
    let state = temporary.path().join("state");
    let home = StoreHome::new(&state);
    let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
    synchronize(
        &home,
        &project,
        &scan_corpora(&[corpus]),
        &mut UnitEmbed,
        SyncMode::Incremental,
    )
    .unwrap();
    (temporary, project, state, home)
}

#[test]
fn search_succeeds_once_a_journal_sidecar_clears() {
    let (_temporary, project, state, home) = indexed_search_fixture();
    let journal = journal_sidecar(&home.project_path(&project).unwrap());
    fs::write(&journal, b"in-flight").unwrap();

    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(40));
            fs::remove_file(&journal).unwrap();
        });
        let completion = execute_direct_with_embedder(
            ["yams", "--json", "--no-gate", "alpha"],
            [(OsString::from("YAMS_HOME"), state.into_os_string())],
            &inputs(&project),
            &[],
            &when(),
            &mut UnitEmbed,
        );
        assert_eq!(
            completion.exit_code,
            ExitCode::Ok,
            "transient journal sidecar should be retried, got stderr={:?} stdout={:?}",
            completion.stderr,
            completion.stdout
        );
        let body: serde_json::Value = serde_json::from_str(completion.stdout.trim()).unwrap();
        assert_eq!(body.as_array().unwrap().len(), 1);
    });
}

#[test]
fn all_search_succeeds_once_a_journal_sidecar_clears() {
    let (_temporary, project, state, home) = indexed_search_fixture();
    let journal = journal_sidecar(&home.project_path(&project).unwrap());
    fs::write(&journal, b"in-flight").unwrap();

    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(40));
            fs::remove_file(&journal).unwrap();
        });
        let completion = execute_direct_with_embedder(
            ["yams", "--all", "--json", "--no-gate", "alpha"],
            [(OsString::from("YAMS_HOME"), state.into_os_string())],
            &inputs(&project),
            &[],
            &when(),
            &mut UnitEmbed,
        );
        assert_eq!(
            completion.exit_code,
            ExitCode::Ok,
            "transient journal sidecar should be retried for --all: {completion:?}"
        );
        let body: serde_json::Value = serde_json::from_str(completion.stdout.trim()).unwrap();
        assert_eq!(body.as_array().unwrap().len(), 1);
    });
}

#[test]
fn projects_rescan_once_a_journal_sidecar_clears() {
    let (_temporary, project, state, home) = indexed_search_fixture();
    let journal = journal_sidecar(&home.project_path(&project).unwrap());
    fs::write(&journal, b"in-flight").unwrap();

    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(40));
            fs::remove_file(&journal).unwrap();
        });
        let completion = execute_direct_with_embedder(
            ["yams", "--projects", "--json"],
            [(OsString::from("YAMS_HOME"), state.into_os_string())],
            &inputs(&project),
            &[],
            &when(),
            &mut UnitEmbed,
        );
        assert_eq!(completion.exit_code, ExitCode::Ok, "{completion:?}");
        let body: serde_json::Value = serde_json::from_str(completion.stdout.trim()).unwrap();
        assert!(body["unreadable"].as_array().unwrap().is_empty(), "{body}");
        assert_eq!(body["projects"].as_array().unwrap().len(), 1, "{body}");
    });
}

#[test]
fn all_search_keeps_exit_4_when_the_journal_sidecar_stays() {
    let (_temporary, project, state, home) = indexed_search_fixture();
    let journal = journal_sidecar(&home.project_path(&project).unwrap());
    fs::write(&journal, b"in-flight").unwrap();

    let completion = execute_direct_with_embedder(
        ["yams", "--all", "--json", "--no-gate", "alpha"],
        [(OsString::from("YAMS_HOME"), state.into_os_string())],
        &inputs(&project),
        &[],
        &when(),
        &mut UnitEmbed,
    );

    assert_eq!(completion.exit_code, ExitCode::Operational);
    assert!(journal.exists());
}

#[test]
fn search_keeps_exit_4_when_the_journal_sidecar_stays() {
    let (_temporary, project, state, home) = indexed_search_fixture();
    let journal = journal_sidecar(&home.project_path(&project).unwrap());
    fs::write(&journal, b"in-flight").unwrap();

    let completion = execute_direct_with_embedder(
        ["yams", "--json", "--no-gate", "alpha"],
        [(OsString::from("YAMS_HOME"), state.into_os_string())],
        &inputs(&project),
        &[],
        &when(),
        &mut UnitEmbed,
    );

    assert_eq!(completion.exit_code, ExitCode::Operational);
    assert_eq!(completion.stderr, "");
    let body: serde_json::Value = serde_json::from_str(completion.stdout.trim()).unwrap();
    assert_eq!(body["ok"], false);
    assert_eq!(body["exit"], 4);
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("refusing to inspect an in-flight database"),
        "expected sidecar refusal, got {}",
        body["error"]
    );
    assert_eq!(body["code"], "store_sidecar");
    assert_eq!(body["transient"], true);
}

#[test]
fn search_succeeds_once_a_vector_journal_sidecar_clears() {
    let (_temporary, project, state, home) = indexed_search_fixture();
    let journal = journal_sidecar(&home.vectors_path());
    fs::write(&journal, b"in-flight").unwrap();

    thread::scope(|scope| {
        scope.spawn(|| {
            thread::sleep(Duration::from_millis(40));
            fs::remove_file(&journal).unwrap();
        });
        let completion = execute_direct_with_embedder(
            ["yams", "--json", "--no-gate", "alpha"],
            [(OsString::from("YAMS_HOME"), state.into_os_string())],
            &inputs(&project),
            &[],
            &when(),
            &mut UnitEmbed,
        );
        assert_eq!(
            completion.exit_code,
            ExitCode::Ok,
            "transient vector journal sidecar should be retried, got stderr={:?} stdout={:?}",
            completion.stderr,
            completion.stdout
        );
        let body: serde_json::Value = serde_json::from_str(completion.stdout.trim()).unwrap();
        assert_eq!(body.as_array().unwrap().len(), 1);
    });
}
