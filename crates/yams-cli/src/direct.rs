use std::fmt;
use std::path::Path;
use std::thread;
use std::time::Duration;

use yams_core::{
    CorpusKind, Discovery, GateVerdict as CoreGateVerdict, HitExplanation as CoreHitExplanation,
    SearchRequest, SearchResponse as CoreSearchResponse, SelectionConfig, append_query_log,
    compose_search, discover_corpora, scan_corpora,
};
use yams_embed::Embedder;
use yams_store::{
    ManagementError, ProjectInventory, RetrievalError, StoreError, StoreHome, VectorCache,
    VectorError, gc, open_index, project_inventory, reindex, stats,
};

use crate::{
    DirectCompletion, DirectOperation, DirectRequest, Environment, Fault, GateEntry, GateVerdict,
    HitExplanation, InvocationTime, ProjectSearchResponse, RenderError, RuntimeLayout,
    SearchExplanation, SearchHit, SearchResponse, Styling, TextOptions, render_all_json,
    render_all_text, render_json, render_text,
};

/// Brief in-process window for sidecar / integrity refusals. After this, exit 4
/// still means retrying the same way will not help.
const TRANSIENT_STORE_ATTEMPTS: u32 = 8;
const TRANSIENT_STORE_RETRY: Duration = Duration::from_millis(25);

pub(crate) fn model_preflight(
    request: &DirectRequest,
    layout: &RuntimeLayout,
) -> Result<(), Fault> {
    let home = StoreHome::new(&layout.cache_dir);
    match request.operation {
        DirectOperation::Search => {
            let root = request
                .project
                .as_deref()
                .ok_or_else(|| Fault::other("a project is required"))?;
            let path = home
                .project_path(root)
                .map_err(|error| Fault::from_store(&error))?;
            retry_transient_store(|| open_index(&path))
                .map_err(|error| Fault::from_management(&error))?;
        }
        DirectOperation::All => {
            let inventory = load_project_inventory(&home, Some(&layout.cwd))
                .map_err(|error| Fault::from_management(&error))?;
            if !inventory.unreadable.is_empty() {
                return Err(Fault::other(format!(
                    "all-project search requires a complete project inventory; unreadable paths: {:?}",
                    inventory.unreadable
                )));
            }
        }
        DirectOperation::Index
        | DirectOperation::Projects
        | DirectOperation::Stats
        | DirectOperation::Gc
        | DirectOperation::Write => {}
    }
    Ok(())
}

pub(crate) fn dispatch(
    request: DirectRequest,
    layout: &RuntimeLayout,
    environment: &Environment,
    embedder: &mut dyn Embedder,
    when: &InvocationTime,
) -> DirectCompletion {
    let home = StoreHome::new(&layout.cache_dir);
    if matches!(
        request.operation,
        DirectOperation::Search | DirectOperation::All
    ) {
        return search_dispatch(request, layout, &home, embedder, when);
    }
    let result = match request.operation {
        DirectOperation::Index => rebuild(&home, &request, layout, environment, embedder),
        DirectOperation::Search
        | DirectOperation::All
        | DirectOperation::Write
        | DirectOperation::Projects
        | DirectOperation::Stats
        | DirectOperation::Gc => {
            return DirectCompletion::operational(format!(
                "yams: {} execution is not available without the service runner",
                request.operation
            ));
        }
    };
    finish_management(request.json, result)
}

pub(crate) fn dispatch_management(
    request: DirectRequest,
    layout: &RuntimeLayout,
    _environment: &Environment,
) -> DirectCompletion {
    let home = StoreHome::new(&layout.cache_dir);
    let result = match request.operation {
        DirectOperation::Projects => projects(&home, &request, layout),
        DirectOperation::Stats => selected_stats(&home, &request),
        DirectOperation::Gc => collect(&home, &request),
        DirectOperation::Search
        | DirectOperation::All
        | DirectOperation::Write
        | DirectOperation::Index => {
            return DirectCompletion::operational(format!(
                "yams: {} is not a management operation",
                request.operation
            ));
        }
    };
    finish_management(request.json, result)
}

fn finish_management(json: bool, result: Result<serde_json::Value, Fault>) -> DirectCompletion {
    match result {
        Ok(value) => {
            if json {
                DirectCompletion {
                    exit_code: yams_core::ExitCode::Ok,
                    stdout: crate::args::compact_json_line(&value),
                    stderr: String::new(),
                }
            } else {
                DirectCompletion {
                    exit_code: yams_core::ExitCode::Ok,
                    stdout: format_text(&value),
                    stderr: String::new(),
                }
            }
        }
        Err(fault) => fault.into_completion(json),
    }
}

fn search_dispatch(
    request: DirectRequest,
    layout: &RuntimeLayout,
    home: &StoreHome,
    embedder: &mut dyn Embedder,
    when: &InvocationTime,
) -> DirectCompletion {
    let query = request.query.as_deref().unwrap_or_default();
    let json = request.json;
    let result = if request.operation == DirectOperation::Search {
        let root = request
            .project
            .as_deref()
            .expect("prepare selected search has a project");
        search_project(&request, root, query, layout, home, embedder, when).map(|response| {
            let exit_code = response.exit_code;
            let output = if json {
                render_json(&response.response)?
            } else {
                render_text(
                    &response.response,
                    TextOptions::single(request.full, Styling::Plain),
                )?
            };
            Ok::<_, RenderError>((exit_code, output))
        })
    } else {
        all_projects(&request, query, layout, home, embedder, when).map(|groups| {
            let output = if json {
                render_all_json(&groups)?
            } else {
                render_all_text(&groups, request.full, Styling::Plain)?
            };
            Ok::<_, RenderError>((
                if groups.iter().any(|group| !group.hits.is_empty()) {
                    yams_core::ExitCode::Ok
                } else {
                    yams_core::ExitCode::Empty
                },
                output,
            ))
        })
    };
    match result {
        Ok(Ok((exit_code, stdout))) => DirectCompletion {
            exit_code,
            stdout,
            stderr: String::new(),
        },
        Ok(Err(RenderError::OutputLimit)) => crate::direct_output_limit_completion(),
        Ok(Err(error)) => Fault::other(error.to_string()).into_completion(json),
        Err(error) => fault_from_direct(error).into_completion(json),
    }
}

fn fault_from_direct(error: DirectFailure) -> Fault {
    match error {
        DirectFailure::Management(error) => Fault::from_management(&error),
        DirectFailure::Store(error) => Fault::from_store(&error),
        DirectFailure::Vector(error) => Fault::from_vector(&error),
        DirectFailure::Retrieval(error) => Fault::from_retrieval(&error),
        DirectFailure::Other(message) => Fault::other(message),
    }
}

struct ProjectResult {
    response: SearchResponse,
    exit_code: yams_core::ExitCode,
}

trait TransientStoreContention {
    fn is_transient_contention(&self) -> bool;
}

impl TransientStoreContention for StoreError {
    fn is_transient_contention(&self) -> bool {
        StoreError::is_transient_contention(self)
    }
}

impl TransientStoreContention for ManagementError {
    fn is_transient_contention(&self) -> bool {
        ManagementError::is_transient_contention(self)
    }
}

impl TransientStoreContention for VectorError {
    fn is_transient_contention(&self) -> bool {
        VectorError::is_transient_contention(self)
    }
}

impl TransientStoreContention for DirectFailure {
    fn is_transient_contention(&self) -> bool {
        DirectFailure::is_transient_contention(self)
    }
}

fn retry_transient_store<T, E>(mut operation: impl FnMut() -> Result<T, E>) -> Result<T, E>
where
    E: TransientStoreContention,
{
    for _ in 1..TRANSIENT_STORE_ATTEMPTS {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if error.is_transient_contention() => {
                thread::sleep(TRANSIENT_STORE_RETRY);
            }
            Err(error) => return Err(error),
        }
    }
    operation()
}

fn load_project_inventory(
    home: &StoreHome,
    current: Option<&Path>,
) -> Result<ProjectInventory, ManagementError> {
    load_project_inventory_with(
        || project_inventory(home, current),
        |path| open_index(path).map(|_| ()),
        || thread::sleep(TRANSIENT_STORE_RETRY),
    )
}

fn load_project_inventory_with(
    mut inventory: impl FnMut() -> Result<ProjectInventory, ManagementError>,
    mut probe: impl FnMut(&Path) -> Result<(), ManagementError>,
    mut pause: impl FnMut(),
) -> Result<ProjectInventory, ManagementError> {
    for attempt in 0..TRANSIENT_STORE_ATTEMPTS {
        let snapshot = inventory()?;
        let mut changed = false;
        let mut transient = None;
        for path in &snapshot.unreadable {
            match probe(path) {
                Ok(()) => changed = true,
                Err(error) if error.is_transient_contention() => {
                    changed = true;
                    transient = Some(error);
                    break;
                }
                Err(_) => {}
            }
        }
        if !changed {
            return Ok(snapshot);
        }
        if attempt + 1 == TRANSIENT_STORE_ATTEMPTS {
            return match transient {
                Some(error) => Err(error),
                None => inventory(),
            };
        }
        pause();
    }
    inventory()
}

#[derive(Debug)]
enum DirectFailure {
    Management(ManagementError),
    Store(StoreError),
    Vector(VectorError),
    Retrieval(RetrievalError),
    Other(String),
}

impl DirectFailure {
    fn is_transient_contention(&self) -> bool {
        match self {
            Self::Management(error) => error.is_transient_contention(),
            Self::Store(error) => error.is_transient_contention(),
            Self::Vector(error) => error.is_transient_contention(),
            Self::Retrieval(error) => error.is_transient_contention(),
            Self::Other(_) => false,
        }
    }
}

impl fmt::Display for DirectFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Management(error) => error.fmt(formatter),
            Self::Store(error) => error.fmt(formatter),
            Self::Vector(error) => error.fmt(formatter),
            Self::Retrieval(error) => error.fmt(formatter),
            Self::Other(error) => formatter.write_str(error),
        }
    }
}

impl From<ManagementError> for DirectFailure {
    fn from(error: ManagementError) -> Self {
        Self::Management(error)
    }
}

impl From<StoreError> for DirectFailure {
    fn from(error: StoreError) -> Self {
        Self::Store(error)
    }
}

impl From<VectorError> for DirectFailure {
    fn from(error: VectorError) -> Self {
        Self::Vector(error)
    }
}

impl From<RetrievalError> for DirectFailure {
    fn from(error: RetrievalError) -> Self {
        Self::Retrieval(error)
    }
}

fn search_project(
    request: &DirectRequest,
    root: &Path,
    query: &str,
    layout: &RuntimeLayout,
    home: &StoreHome,
    embedder: &mut dyn Embedder,
    when: &InvocationTime,
) -> Result<ProjectResult, DirectFailure> {
    retry_transient_store(|| {
        search_project_once(request, root, query, layout, home, embedder, when)
    })
}

fn search_project_once(
    request: &DirectRequest,
    root: &Path,
    query: &str,
    layout: &RuntimeLayout,
    home: &StoreHome,
    embedder: &mut dyn Embedder,
    when: &InvocationTime,
) -> Result<ProjectResult, DirectFailure> {
    let path = home.project_path(root)?;
    let index = open_index(&path)?;
    let snapshot = index.retrieval_snapshot()?;
    let scheme = snapshot
        .scheme()
        .ok_or_else(|| DirectFailure::Other("project index has no embedding scheme".to_owned()))?;
    let query_embedding = embedder
        .embed_query(query)
        .map_err(|error| DirectFailure::Other(error.to_string()))?;
    let cache = VectorCache::open_for_search(home)?;
    let dense =
        snapshot.dense_candidates(&cache, &query_embedding, scheme, embedder.signature())?;
    let dense = dense
        .iter()
        .map(|candidate| candidate.as_candidate())
        .collect::<Vec<_>>();
    let pages = snapshot.page_metadata()?;
    let chunks = snapshot.chunk_metadata()?;
    let lexical = snapshot.lexical_scores(query)?;
    let statistics = snapshot.snippet_statistics(query)?;
    let selection = SelectionConfig::new(
        request.k,
        request.min_score.unwrap_or(yams_core::MIN_SCORE),
        request.max_gap.unwrap_or(yams_core::MAX_GAP),
        if request.no_gate {
            yams_core::GateMode::Bypass
        } else {
            yams_core::GateMode::Apply
        },
    )
    .map_err(|error| DirectFailure::Other(error.to_string()))?;
    let composed = compose_search(SearchRequest::new(
        query,
        query_embedding.values(),
        &pages,
        &chunks,
        &dense,
        &lexical,
        &statistics,
        selection,
    ))
    .map_err(|error| DirectFailure::Other(error.to_string()))?;
    let exit_code = composed.exit_code();
    let response = convert_response(composed, request.explain);
    append_log(
        layout,
        root,
        request,
        query,
        when,
        exit_code,
        response.hits.len(),
    );
    Ok(ProjectResult {
        response,
        exit_code,
    })
}

fn all_projects(
    request: &DirectRequest,
    query: &str,
    layout: &RuntimeLayout,
    home: &StoreHome,
    embedder: &mut dyn Embedder,
    when: &InvocationTime,
) -> Result<Vec<ProjectSearchResponse>, DirectFailure> {
    let inventory = load_project_inventory(home, Some(&layout.cwd))?;
    let mut groups = Vec::new();
    for project in inventory.projects {
        let result = search_project(request, &project.root, query, layout, home, embedder, when)?;
        groups.push(ProjectSearchResponse {
            project: project.root.to_string_lossy().into_owned(),
            hits: result.response.hits,
        });
    }
    Ok(groups)
}

fn convert_response(response: CoreSearchResponse, explain: bool) -> SearchResponse {
    let gate = response.explanation().gate().map(convert_gate);
    let explanation = explain.then(|| SearchExplanation {
        query: response.query().to_owned(),
        applied: response.explanation().applied(),
        gate,
    });
    let hits = response
        .hits()
        .iter()
        .map(|hit| SearchHit {
            name: hit.name().to_owned(),
            path: hit.path().to_owned(),
            score: hit.score().get(),
            text: hit.text().to_owned(),
            snippet: hit.snippet().to_owned(),
            clipped_start: hit.clipped_start(),
            clipped_end: hit.clipped_end(),
            corpus: match hit.corpus() {
                Some("private") => CorpusKind::Private,
                Some("override") => CorpusKind::Override,
                _ => CorpusKind::Shared,
            },
            exact: hit.exact(),
            status: hit.status().map(str::to_owned),
            explanation: explain.then(|| {
                response
                    .explanation()
                    .hit(hit.path())
                    .map(convert_hit_explanation)
                    .unwrap_or(HitExplanation {
                        dense_rank: None,
                        bm25_rank: None,
                        rrf_score: None,
                    })
            }),
        })
        .collect();
    SearchResponse { hits, explanation }
}

fn convert_hit_explanation(value: &CoreHitExplanation) -> HitExplanation {
    HitExplanation {
        dense_rank: value.dense_rank(),
        bm25_rank: value.bm25_rank(),
        rrf_score: value.rrf_score(),
    }
}

fn convert_gate(value: &CoreGateVerdict) -> GateVerdict {
    let entries = |entries: &[yams_core::GateHit]| {
        entries
            .iter()
            .map(|entry| GateEntry {
                path: entry.path().to_owned(),
                score: entry.score(),
            })
            .collect()
    };
    GateVerdict {
        baseline: value.baseline(),
        min_score: value.min_score(),
        max_gap: value.max_gap(),
        no_hits: value.no_hits(),
        floor_fired: value.floor_fired(),
        top: value.top(),
        floor_dropped: entries(value.floor_dropped()),
        gap_dropped: entries(value.gap_dropped()),
        rescued: entries(value.rescued()),
    }
}

fn append_log(
    layout: &RuntimeLayout,
    root: &Path,
    request: &DirectRequest,
    query: &str,
    when: &InvocationTime,
    exit_code: yams_core::ExitCode,
    hits: usize,
) {
    let record = yams_core::QueryLogRecord {
        timestamp: &when.utc_timestamp,
        project: root,
        query,
        k: u32::try_from(request.k).unwrap_or(u32::MAX),
        rc: i32::from(exit_code),
        hits: u64::try_from(hits).unwrap_or(u64::MAX),
        gate: !request.no_gate,
        explain: request.explain,
        min_score: request.min_score,
        max_gap: request.max_gap,
        all: request.operation == DirectOperation::All,
    };
    let _ = append_query_log(
        &layout.query_log,
        yams_core::QueryLogEligibility::SearchAttempted,
        &record,
    );
}

fn projects(
    home: &StoreHome,
    request: &DirectRequest,
    layout: &RuntimeLayout,
) -> Result<serde_json::Value, Fault> {
    let inventory = load_project_inventory(home, Some(&layout.cwd))
        .map_err(|error| Fault::from_management(&error))?;
    let projects = inventory
        .projects
        .iter()
        .map(|project| {
            serde_json::json!({
                "root": project.root,
                "current": project.current,
                "pages": project.index.page_count(),
                "chunks": project.index.chunk_count(),
                "generation": project.index.generation(),
                "bytes": project.index.bytes(),
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "ok": true,
        "operation": request.operation.to_string(),
        "projects": projects,
        "unrecorded": inventory.unrecorded,
        "unreadable": inventory.unreadable,
    }))
}

fn selected_stats(home: &StoreHome, request: &DirectRequest) -> Result<serde_json::Value, Fault> {
    let root = request
        .project
        .as_deref()
        .ok_or_else(|| Fault::other("a project is required"))?;
    let value = match retry_transient_store(|| stats(home, root)) {
        Ok(value) => value,
        Err(ManagementError::MissingIndex { .. }) => {
            return Ok(serde_json::json!({
                "ok": true,
                "operation": "stats",
                "root": root,
                "pages": 0,
                "chunks": 0,
                "generation": 0,
                "vectors": 0,
                "vector_bytes": 0,
            }));
        }
        Err(error) => return Err(Fault::from_management(&error)),
    };
    Ok(serde_json::json!({
        "ok": true,
        "operation": "stats",
        "root": root,
        "pages": value.page_count(),
        "chunks": value.chunk_count(),
        "generation": value.generation(),
        "vectors": value.vectors,
        "vector_bytes": value.vector_bytes,
    }))
}

fn rebuild(
    home: &StoreHome,
    request: &DirectRequest,
    layout: &RuntimeLayout,
    environment: &Environment,
    embedder: &mut dyn Embedder,
) -> Result<serde_json::Value, Fault> {
    let root = request
        .project
        .as_deref()
        .ok_or_else(|| Fault::other("a project is required"))?;
    let known_roots = project_inventory(home, Some(root))
        .map_err(|error| Fault::from_management(&error))?
        .projects
        .into_iter()
        .map(|project| project.root)
        .collect();
    let discovery = Discovery {
        home: environment.home().map(Path::new).map(Path::to_path_buf),
        override_dirs: match &layout.corpus_dirs {
            crate::ResolvedDirsOverride::NonEmpty(paths) => paths.clone(),
            _ => Vec::new(),
        },
        known_roots,
    };
    let report =
        discover_corpora(root, &discovery).map_err(|error| Fault::other(error.to_string()))?;
    let scan = scan_corpora(&report.corpora);
    let sync =
        reindex(home, root, &scan, embedder).map_err(|error| Fault::from_management(&error))?;
    Ok(serde_json::json!({
        "ok": true,
        "operation": "index",
        "changed": sync.changed,
        "removed": sync.removed,
        "embedded": sync.embedded,
        "generation": sync.generation,
        "note_count": sync.notes.len(),
    }))
}

fn collect(home: &StoreHome, request: &DirectRequest) -> Result<serde_json::Value, Fault> {
    let report = gc(home).map_err(|error| Fault::from_management(&error))?;
    Ok(serde_json::json!({
        "ok": true,
        "operation": request.operation.to_string(),
        "removed": report.removed,
        "kept": report.kept,
    }))
}

fn format_text(value: &serde_json::Value) -> String {
    serde_json::to_string_pretty(value).expect("management response serializes") + "\n"
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::path::{Path, PathBuf};

    use yams_store::{ManagementError, ProjectInventory};

    use super::{
        TRANSIENT_STORE_ATTEMPTS, TransientStoreContention, load_project_inventory_with,
        retry_transient_store,
    };

    #[derive(Debug, Eq, PartialEq)]
    enum Probe {
        Transient,
        Permanent,
    }

    impl TransientStoreContention for Probe {
        fn is_transient_contention(&self) -> bool {
            matches!(self, Self::Transient)
        }
    }

    #[test]
    fn retry_succeeds_after_transient_store_refusals() {
        let mut attempts = 0;
        let result = retry_transient_store(|| {
            attempts += 1;
            if attempts < 3 {
                Err(Probe::Transient)
            } else {
                Ok("ready")
            }
        });
        assert_eq!(result, Ok("ready"));
        assert_eq!(attempts, 3);
    }

    #[test]
    fn retry_returns_the_last_transient_refusal_after_the_budget() {
        let mut attempts = 0;
        let result = retry_transient_store(|| {
            attempts += 1;
            Err::<(), _>(Probe::Transient)
        });
        assert_eq!(result, Err(Probe::Transient));
        assert_eq!(attempts, TRANSIENT_STORE_ATTEMPTS);
    }

    #[test]
    fn retry_does_not_repeat_a_lasting_store_failure() {
        let mut attempts = 0;
        let result = retry_transient_store(|| {
            attempts += 1;
            Err::<(), _>(Probe::Permanent)
        });
        assert_eq!(result, Err(Probe::Permanent));
        assert_eq!(attempts, 1);
    }

    fn inventory(unreadable: &[&str]) -> ProjectInventory {
        ProjectInventory {
            projects: Vec::new(),
            unrecorded: Vec::new(),
            unreadable: unreadable.iter().map(PathBuf::from).collect(),
        }
    }

    #[test]
    fn stable_inventory_retries_a_transient_unreadable_path() {
        let mut snapshots = VecDeque::from([inventory(&["index"]), inventory(&[])]);
        let mut inventory_calls = 0;
        let mut pauses = 0;

        let result = load_project_inventory_with(
            || {
                inventory_calls += 1;
                Ok(snapshots.pop_front().unwrap())
            },
            |path: &Path| {
                Err(ManagementError::UnsafeSidecar {
                    path: path.with_extension("sqlite3-journal"),
                })
            },
            || pauses += 1,
        )
        .unwrap();

        assert!(result.unreadable.is_empty());
        assert_eq!(inventory_calls, 2);
        assert_eq!(pauses, 1);
    }

    #[test]
    fn stable_inventory_rescans_when_an_unreadable_probe_succeeds() {
        let mut snapshots = VecDeque::from([inventory(&["index"]), inventory(&[])]);
        let mut inventory_calls = 0;
        let mut pauses = 0;

        let result = load_project_inventory_with(
            || {
                inventory_calls += 1;
                Ok(snapshots.pop_front().unwrap())
            },
            |_: &Path| Ok(()),
            || pauses += 1,
        )
        .unwrap();

        assert!(result.unreadable.is_empty());
        assert_eq!(inventory_calls, 2);
        assert_eq!(pauses, 1);
    }

    #[test]
    fn stable_inventory_keeps_a_permanently_unreadable_path() {
        let mut inventory_calls = 0;
        let mut pauses = 0;

        let result = load_project_inventory_with(
            || {
                inventory_calls += 1;
                Ok(inventory(&["index"]))
            },
            |path: &Path| {
                Err(ManagementError::MissingIndex {
                    path: path.to_path_buf(),
                })
            },
            || pauses += 1,
        )
        .unwrap();

        assert_eq!(result.unreadable, [PathBuf::from("index")]);
        assert_eq!(inventory_calls, 1);
        assert_eq!(pauses, 0);
    }

    #[test]
    fn stable_inventory_returns_a_transient_error_after_the_retry_budget() {
        let mut inventory_calls = 0;
        let mut pauses = 0;

        let result = load_project_inventory_with(
            || {
                inventory_calls += 1;
                Ok(inventory(&["index"]))
            },
            |path: &Path| {
                Err(ManagementError::UnsafeSidecar {
                    path: path.with_extension("sqlite3-journal"),
                })
            },
            || pauses += 1,
        );

        assert!(matches!(result, Err(ManagementError::UnsafeSidecar { .. })));
        assert_eq!(inventory_calls, TRANSIENT_STORE_ATTEMPTS as usize);
        assert_eq!(pauses, TRANSIENT_STORE_ATTEMPTS as usize - 1);
    }
}
