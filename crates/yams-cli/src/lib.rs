//! Direct Yams command-line parsing and runtime configuration.

mod args;
#[cfg(not(feature = "test-support"))]
mod client;
#[cfg(feature = "test-support")]
#[doc(hidden)]
pub mod client;
mod direct;
mod fault;
mod layout;
mod render;
mod when;

use std::ffi::OsString;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::ExitCode as ProcessExitCode;

use yams_protocol::{OperationKind, ServiceOperation};

pub use args::{
    DirectCompletion, DirectOperation, DirectRequest, ParseOutcome, parse_direct_request,
};
pub use fault::Fault;
pub use layout::{
    DirsOverride, Environment, LayoutError, Platform, ResolvedDirsOverride, RuntimeInputs,
    RuntimeLayout,
};
pub use render::{
    BoundedBuffer, GateEntry, GateVerdict, HitExplanation, ProjectSearchResponse, RenderError,
    SearchExplanation, SearchHit, SearchResponse, Styling, TextOptions, render_all_json,
    render_all_text, render_diagnostic, render_json, render_text,
};
pub use when::InvocationTime;

/// Parse and resolve one direct invocation without opening a store or model.
///
/// Parser and configuration tests stop here so they cannot accidentally
/// construct live state.
pub fn prepare_direct<I, A, E, K, V>(
    arguments: I,
    variables: E,
    inputs: &RuntimeInputs,
) -> Result<(DirectRequest, Environment, RuntimeLayout), DirectCompletion>
where
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
    E: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let request = match parse_direct_request(arguments) {
        ParseOutcome::Request(request) => request,
        ParseOutcome::Completion(completion) => return Err(completion),
    };
    let environment = Environment::resolve(variables);
    prepare_resolved(request, environment, inputs)
}

fn prepare_resolved(
    mut request: DirectRequest,
    environment: Environment,
    inputs: &RuntimeInputs,
) -> Result<(DirectRequest, Environment, RuntimeLayout), DirectCompletion> {
    let write = request.operation == DirectOperation::Write;
    let layout = RuntimeLayout::resolve(&environment, inputs)
        .map_err(|error| DirectCompletion::configuration(error.to_string(), write))?;
    if matches!(
        request.operation,
        DirectOperation::Search
            | DirectOperation::Write
            | DirectOperation::Index
            | DirectOperation::Stats
    ) {
        request.project = Some(
            layout::resolve_project_path(request.project.as_deref(), inputs)
                .map_err(|error| DirectCompletion::configuration(error.to_string(), write))?,
        );
    }
    Ok((request, environment, layout))
}

/// Parse, resolve, and execute one direct request with injected input and
/// invocation time.
///
/// The write operation terminates at `yams-wiki`; it never constructs a
/// model, store, or service client. Management operations use their resolved
/// store context directly. Search and index require an embedder.
pub fn execute_direct<I, A, E, K, V>(
    arguments: I,
    variables: E,
    inputs: &RuntimeInputs,
    stdin: &[u8],
    when: &InvocationTime,
) -> DirectCompletion
where
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
    E: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let completion = match prepare_direct(arguments, variables, inputs) {
        Ok((request, environment, layout)) => {
            dispatch_direct(request, stdin, when, Some((&environment, &layout)), None)
        }
        Err(completion) => completion,
    };
    bound_direct_completion(completion)
}

/// Execute a direct request with an injected embedder. This is the seam used
/// by tests and local harnesses; production wiring supplies a model only
/// after parsing, layout resolution, and operation preconditions succeed.
pub fn execute_direct_with_embedder<I, A, E, K, V>(
    arguments: I,
    variables: E,
    inputs: &RuntimeInputs,
    stdin: &[u8],
    when: &InvocationTime,
    embedder: &mut dyn yams_embed::Embedder,
) -> DirectCompletion
where
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
    E: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let completion = match prepare_direct(arguments, variables, inputs) {
        Ok((request, environment, layout)) => dispatch_direct(
            request,
            stdin,
            when,
            Some((&environment, &layout)),
            Some(embedder),
        ),
        Err(completion) => completion,
    };
    bound_direct_completion(completion)
}

/// Execute a typed service operation without reconstructing CLI argv.
pub fn execute_service_operation<E, K, V>(
    operation: ServiceOperation,
    cwd: PathBuf,
    variables: E,
    when: &InvocationTime,
    embedder: &mut dyn yams_embed::Embedder,
) -> DirectCompletion
where
    E: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let request = match direct_request_from_operation(operation) {
        Ok(request) => request,
        Err(completion) => return bound_direct_completion(completion),
    };
    let mut inputs = match RuntimeInputs::current() {
        Ok(inputs) => inputs,
        Err(error) => {
            return bound_direct_completion(DirectCompletion::operational(error.to_string()));
        }
    };
    inputs.cwd = cwd;
    let completion = match prepare_typed(request, variables, &inputs) {
        Ok((request, environment, layout)) => dispatch_direct(
            request,
            &[],
            when,
            Some((&environment, &layout)),
            Some(embedder),
        ),
        Err(completion) => completion,
    };
    bound_direct_completion(completion)
}

fn prepare_typed<E, K, V>(
    request: DirectRequest,
    variables: E,
    inputs: &RuntimeInputs,
) -> Result<(DirectRequest, Environment, RuntimeLayout), DirectCompletion>
where
    E: IntoIterator<Item = (K, V)>,
    K: Into<OsString>,
    V: Into<OsString>,
{
    let environment = Environment::resolve(variables);
    prepare_resolved(request, environment, inputs)
}

fn direct_request_from_operation(
    operation: ServiceOperation,
) -> Result<DirectRequest, DirectCompletion> {
    let kind = match operation.kind {
        OperationKind::Search => DirectOperation::Search,
        OperationKind::All => DirectOperation::All,
        OperationKind::Index => DirectOperation::Index,
        OperationKind::Stats => DirectOperation::Stats,
        OperationKind::Projects => DirectOperation::Projects,
        OperationKind::Gc => DirectOperation::Gc,
    };
    let k = operation
        .k
        .parse::<usize>()
        .map_err(|_| DirectCompletion::operational(format!("invalid -k '{}'", operation.k)))?;
    let min_score = operation
        .min_score
        .as_deref()
        .map(|value| {
            value.parse::<f64>().map_err(|_| {
                DirectCompletion::operational(format!("invalid --min-score '{value}'"))
            })
        })
        .transpose()?;
    let max_gap = operation
        .max_gap
        .as_deref()
        .map(|value| {
            value
                .parse::<f64>()
                .map_err(|_| DirectCompletion::operational(format!("invalid --max-gap '{value}'")))
        })
        .transpose()?;
    let project = operation.project.map(PathBuf::from);
    let query = if operation.query.is_empty() {
        None
    } else {
        Some(operation.query)
    };
    Ok(DirectRequest {
        operation: kind,
        project,
        query,
        k,
        requested_k: operation.k,
        json: operation.json,
        full: operation.full,
        no_gate: operation.no_gate,
        explain: operation.explain,
        min_score,
        max_gap,
    })
}

fn direct_output_limit_completion() -> DirectCompletion {
    DirectCompletion {
        exit_code: yams_core::ExitCode::Operational,
        stdout: String::new(),
        stderr: "yams: output limit\n".to_owned(),
    }
}

fn bound_direct_completion(completion: DirectCompletion) -> DirectCompletion {
    if completion.stdout.len() > BoundedBuffer::DIRECT_STREAM_CAP
        || completion.stderr.len() > BoundedBuffer::DIRECT_STREAM_CAP
    {
        direct_output_limit_completion()
    } else {
        completion
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServiceRoute {
    Direct,
    Service,
}

fn service_route(environment: &Environment) -> ServiceRoute {
    if environment.no_service() {
        return ServiceRoute::Direct;
    }
    if environment.dirs().is_some() {
        return ServiceRoute::Direct;
    }
    if environment.allow_net() {
        return ServiceRoute::Direct;
    }
    if environment.home().is_some() {
        return ServiceRoute::Direct;
    }
    ServiceRoute::Service
}

fn dispatch_direct(
    request: DirectRequest,
    stdin: &[u8],
    when: &InvocationTime,
    context: Option<(&Environment, &RuntimeLayout)>,
    embedder: Option<&mut dyn yams_embed::Embedder>,
) -> DirectCompletion {
    if request.operation == DirectOperation::Write {
        let project = request
            .project
            .expect("prepared selected-project operations have a canonical project");
        let result =
            yams_wiki::write_json(&project.join(".agents/memory"), stdin, &when.civil_date);
        return DirectCompletion {
            exit_code: result.exit_code,
            stdout: args::compact_json_line(&result.body),
            stderr: String::new(),
        };
    }
    let Some((environment, layout)) = context else {
        return DirectCompletion::operational(format!(
            "yams: {} is missing runtime context",
            request.operation
        ));
    };
    if matches!(
        request.operation,
        DirectOperation::Projects | DirectOperation::Stats | DirectOperation::Gc
    ) {
        return direct::dispatch_management(request, layout, environment);
    }
    let Some(embedder) = embedder else {
        return DirectCompletion::operational(format!(
            "yams: {} requires a model",
            request.operation
        ));
    };
    direct::dispatch(request, layout, environment, embedder, when)
}

/// Entry point shared by the primary command and the compatibility launcher.
pub fn process_main() -> ProcessExitCode {
    let when = InvocationTime::capture();
    let arguments: Vec<_> = std::env::args_os().collect();
    let variables: Vec<_> = std::env::vars_os().collect();
    let request = match parse_direct_request(arguments.iter().cloned()) {
        ParseOutcome::Request(request) => request,
        ParseOutcome::Completion(completion) => return emit(completion),
    };
    let write = request.operation == DirectOperation::Write;
    let environment = Environment::resolve(variables);
    let inputs = match RuntimeInputs::current() {
        Ok(inputs) => inputs,
        Err(error) => {
            return emit(DirectCompletion::operational_for_mode(
                error.to_string(),
                write,
            ));
        }
    };
    let (request, environment, layout) = match prepare_resolved(request, environment, &inputs) {
        Ok(prepared) => prepared,
        Err(completion) => return emit(completion),
    };
    if let DirsOverride::SetEmpty { variable } = environment.dirs_override() {
        eprintln!("yams: warning: {variable} is set but empty; using ordinary corpus discovery");
    }
    let completion = if write {
        let mut stdin = io::stdin().lock().take(yams_core::MAX_FILE_BYTES + 1);
        let mut input = Vec::new();
        if let Err(error) = stdin.read_to_end(&mut input) {
            return emit(DirectCompletion::operational_for_mode(
                format!("could not read stdin: {error}"),
                true,
            ));
        }
        dispatch_direct(request, &input, &when, None, None)
    } else {
        if matches!(service_route(&environment), ServiceRoute::Service) {
            match client::try_service(&request, &layout) {
                Some(Ok(completion)) => return emit(completion),
                Some(Err(completion)) => return emit(completion),
                None => {}
            }
        }
        dispatch_direct_with_model(request, &environment, &layout, &when)
    };
    emit(completion)
}

fn dispatch_direct_with_model(
    request: DirectRequest,
    environment: &Environment,
    layout: &RuntimeLayout,
    when: &InvocationTime,
) -> DirectCompletion {
    if !matches!(
        request.operation,
        DirectOperation::Search | DirectOperation::All | DirectOperation::Index
    ) {
        return dispatch_direct(request, &[], when, Some((environment, layout)), None);
    }
    if let Err(fault) = direct::model_preflight(&request, layout) {
        return fault.into_completion(request.json);
    }
    let model = if environment.allow_net() {
        yams_embed::JinaEmbedder::online(&layout.model_cache_dir, &layout.model_lock_dir)
    } else {
        yams_embed::JinaEmbedder::offline(&layout.model_cache_dir, &layout.model_lock_dir)
    };
    match model {
        Ok(mut model) => dispatch_direct(
            request,
            &[],
            when,
            Some((environment, layout)),
            Some(&mut model),
        ),
        Err(error) => DirectCompletion::operational(error.to_string()),
    }
}

fn emit(completion: DirectCompletion) -> ProcessExitCode {
    let completion = bound_direct_completion(completion);
    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let writes = stdout.write_all(completion.stdout.as_bytes());
    let errors = stderr.write_all(completion.stderr.as_bytes());
    let flushes = (stdout.flush(), stderr.flush());
    let exit_code = if writes.is_ok() && errors.is_ok() && flushes.0.is_ok() && flushes.1.is_ok() {
        i32::from(completion.exit_code)
    } else {
        i32::from(yams_core::ExitCode::Operational)
    };
    u8::try_from(exit_code)
        .map(ProcessExitCode::from)
        .unwrap_or(ProcessExitCode::FAILURE)
}

#[cfg(test)]
mod tests {
    use super::{
        BoundedBuffer, DirectCompletion, Environment, ServiceRoute, bound_direct_completion,
        service_route,
    };
    use yams_core::ExitCode;

    fn route_for(variables: &[(&str, &str)]) -> ServiceRoute {
        let environment = Environment::resolve(variables.iter().copied());
        service_route(&environment)
    }

    #[test]
    fn service_target_matrix_matches_the_oracle() {
        let cases: &[(&[(&str, &str)], ServiceRoute)] = &[
            (&[], ServiceRoute::Service),
            (&[("YAMS_NO_SERVICE", "1")], ServiceRoute::Direct),
            (&[("YAMS_NO_SERVICE", "true")], ServiceRoute::Service),
            (&[("YAMS_DIRS", "/tmp/x")], ServiceRoute::Direct),
            (&[("YAMS_DIRS", "")], ServiceRoute::Service),
            (&[("YAMS_ALLOW_NET", "1")], ServiceRoute::Direct),
            (&[("YAMS_ALLOW_NET", "true")], ServiceRoute::Service),
            (&[("YAMS_HOME", "/tmp/h")], ServiceRoute::Direct),
            (
                &[("YAMS_HOME", "/tmp/h"), ("YAMS_SERVICE_SOCKET", "/tmp/s")],
                ServiceRoute::Direct,
            ),
            (&[("YAMS_SERVICE_SOCKET", "")], ServiceRoute::Service),
            (
                &[("YAMS_HOME", "/tmp/h"), ("YAMS_SERVICE_SOCKET", "")],
                ServiceRoute::Direct,
            ),
        ];
        for (variables, expected) in cases {
            assert_eq!(route_for(variables), *expected, "env: {variables:?}");
        }
    }

    #[test]
    fn direct_completion_limits_stdout_and_stderr_independently() {
        let exact = bound_direct_completion(DirectCompletion {
            exit_code: ExitCode::Ok,
            stdout: "o".repeat(BoundedBuffer::DIRECT_STREAM_CAP),
            stderr: "e".repeat(BoundedBuffer::DIRECT_STREAM_CAP),
        });
        assert_eq!(exact.exit_code, ExitCode::Ok);
        assert_eq!(exact.stdout.len(), BoundedBuffer::DIRECT_STREAM_CAP);
        assert_eq!(exact.stderr.len(), BoundedBuffer::DIRECT_STREAM_CAP);

        for completion in [
            DirectCompletion {
                exit_code: ExitCode::Ok,
                stdout: "o".repeat(BoundedBuffer::DIRECT_STREAM_CAP + 1),
                stderr: String::new(),
            },
            DirectCompletion {
                exit_code: ExitCode::Ok,
                stdout: String::new(),
                stderr: "e".repeat(BoundedBuffer::DIRECT_STREAM_CAP + 1),
            },
        ] {
            assert_eq!(
                bound_direct_completion(completion),
                DirectCompletion {
                    exit_code: ExitCode::Operational,
                    stdout: String::new(),
                    stderr: "yams: output limit\n".to_owned(),
                }
            );
        }
    }
}
