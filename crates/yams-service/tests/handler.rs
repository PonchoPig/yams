use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use chrono::DateTime;
use yams_core::{Corpus, CorpusKind, scan_corpora};
use yams_embed::{Embedder, Embedding, EmbeddingError};
use yams_protocol::{Request, ServiceOperation};
use yams_store::{StoreHome, SyncMode, synchronize};

#[path = "../src/main.rs"]
#[allow(dead_code)]
mod service_binary;

const TEST_SIGNATURE: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

#[test]
fn write_argv_is_rejected_without_running_a_handler_body() {
    let fixture = IndexedFixture::new(false);
    let model = Arc::new(Mutex::new(UnitEmbedder));
    let signature: Arc<str> = Arc::from(TEST_SIGNATURE);
    let output = service_binary::execute_request(
        &fixture.environment,
        &model,
        &signature,
        1,
        Request {
            operation: ServiceOperation::from_argv(&["--stats".into()]).expect("stats"),
            argv: vec!["--write".into()],
            cwd: fixture.project.to_string_lossy().into_owned(),
        },
    );
    assert_eq!(output.exit_code, 2, "{}", output.stderr);
    assert!(output.stderr.contains("--write"), "{}", output.stderr);
}

#[test]
fn a_paused_embedding_call_does_not_block_an_admitted_management_request() {
    let fixture = IndexedFixture::new(false);
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let model = Arc::new(Mutex::new(BlockableEmbedder {
        started: started_tx,
        release: release_rx,
    }));
    let signature: Arc<str> = Arc::from(TEST_SIGNATURE);

    let search_environment = fixture.environment.clone();
    let search_model = Arc::clone(&model);
    let search_signature = Arc::clone(&signature);
    let search_request = fixture.request(&["--json", "--no-gate", "alpha"]);
    let search = thread::spawn(move || {
        service_binary::execute_request(
            &search_environment,
            &search_model,
            &search_signature,
            1,
            search_request,
        )
    });
    started_rx
        .recv_timeout(Duration::from_secs(2))
        .expect("search reached query inference");

    let management_environment = fixture.environment.clone();
    let management_model = Arc::clone(&model);
    let management_signature = Arc::clone(&signature);
    let management_request = fixture.request(&["--stats", "--json"]);
    let (management_tx, management_rx) = mpsc::channel();
    let management = thread::spawn(move || {
        let output = service_binary::execute_request(
            &management_environment,
            &management_model,
            &management_signature,
            1,
            management_request,
        );
        management_tx.send(output).unwrap();
    });

    let admitted = management_rx.recv_timeout(Duration::from_secs(1));
    release_tx.send(()).unwrap();
    let search_output = search.join().unwrap();
    management.join().unwrap();
    let stats = admitted.expect("management must not wait behind inference");
    assert_eq!(stats.exit_code, 0, "{}", stats.stderr);
    assert_eq!(search_output.exit_code, 0, "{}", search_output.stderr);
}

#[test]
fn a_service_backed_search_logs_a_fresh_valid_timestamp_per_request() {
    let fixture = IndexedFixture::new(true);
    let model = Arc::new(Mutex::new(UnitEmbedder));
    let signature: Arc<str> = Arc::from(TEST_SIGNATURE);

    for query in ["alpha", "beta"] {
        let output = service_binary::execute_request(
            &fixture.environment,
            &model,
            &signature,
            1,
            fixture.request(&["--json", "--no-gate", query]),
        );
        assert!(matches!(output.exit_code, 0 | 1), "{}", output.stderr);
        thread::sleep(Duration::from_millis(2));
    }

    let records = std::fs::read_to_string(fixture.query_log()).unwrap();
    let timestamps = records
        .lines()
        .map(|line| {
            serde_json::from_str::<serde_json::Value>(line).unwrap()["ts"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect::<Vec<_>>();
    assert_eq!(timestamps.len(), 2, "each request appends one record");
    for timestamp in &timestamps {
        assert_eq!(
            timestamp.len(),
            24,
            "millisecond UTC timestamp: {timestamp}"
        );
        assert!(timestamp.ends_with('Z'), "UTC timestamp: {timestamp}");
        DateTime::parse_from_rfc3339(timestamp).expect("valid RFC 3339 timestamp");
    }
    assert_ne!(
        timestamps[0], timestamps[1],
        "captured per request, not per process"
    );
}

struct UnitEmbedder;

impl Embedder for UnitEmbedder {
    fn signature(&self) -> &str {
        TEST_SIGNATURE
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

struct BlockableEmbedder {
    started: mpsc::Sender<()>,
    release: mpsc::Receiver<()>,
}

impl Embedder for BlockableEmbedder {
    fn signature(&self) -> &str {
        TEST_SIGNATURE
    }

    fn dimensions(&self) -> usize {
        1
    }

    fn embed_passages(&mut self, _: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        unreachable!("the indexed fixture only performs query inference")
    }

    fn embed_query(&mut self, _: &str) -> Result<Embedding, EmbeddingError> {
        self.started.send(()).unwrap();
        self.release
            .recv_timeout(Duration::from_secs(3))
            .expect("test releases the paused inference");
        Embedding::new(vec![1.0])
    }
}

struct IndexedFixture {
    _temporary: tempfile::TempDir,
    project: PathBuf,
    state: PathBuf,
    environment: Vec<(OsString, OsString)>,
}

impl IndexedFixture {
    fn new(query_log: bool) -> Self {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::set_permissions(temporary.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let project = temporary.path().join("project");
        let corpus_path = project.join(".agents/memory");
        std::fs::create_dir_all(&corpus_path).unwrap();
        std::fs::write(
            corpus_path.join("alpha.md"),
            "---\ntitle: Alpha\nstatus: current\n---\n\nalpha search evidence\n",
        )
        .unwrap();
        let project = project.canonicalize().unwrap();
        let state = temporary.path().join("state");
        let home = StoreHome::new(&state);
        let corpus = Corpus::validated(&corpus_path, CorpusKind::Shared).unwrap();
        let scan = scan_corpora(&[corpus]);
        synchronize(
            &home,
            &project,
            &scan,
            &mut UnitEmbedder,
            SyncMode::Incremental,
        )
        .unwrap();
        if query_log {
            std::fs::create_dir_all(&state).unwrap();
            let path = state.join("queries.jsonl");
            std::fs::File::create(&path).unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let environment = vec![(OsString::from("YAMS_HOME"), state.clone().into_os_string())];
        Self {
            _temporary: temporary,
            project,
            state,
            environment,
        }
    }

    fn request(&self, argv: &[&str]) -> Request {
        Request::from_argv(
            argv.iter().map(|value| (*value).to_owned()).collect(),
            self.project.to_string_lossy().into_owned(),
        )
        .expect("service request is not --write")
    }

    fn query_log(&self) -> PathBuf {
        self.state.join("queries.jsonl")
    }
}
