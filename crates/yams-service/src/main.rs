//! Local Yams service process.

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use yams_cli::{
    Environment, InvocationTime, RuntimeInputs, RuntimeLayout, execute_service_operation,
};
use yams_embed::{Embedder, Embedding, EmbeddingError};
use yams_protocol::Request;
use yams_service::{
    ExecutionOutput, ShutdownToken, bind_after, cleanup_owned_socket, parse_service_args,
    serve_until,
};

fn main() {
    let (socket, idle_timeout, provenance) =
        match parse_service_args(env::args_os().skip(1).collect()) {
            Ok(values) => values,
            Err(error) if error == "help" => {
                println!("Usage: yams-service [--socket PATH] [--idle-timeout SECONDS]");
                return;
            }
            Err(error) if error == "version" => {
                println!("yams-service {}", env!("CARGO_PKG_VERSION"));
                return;
            }
            Err(error) => {
                eprintln!("yams-service: {error}");
                std::process::exit(2);
            }
        };
    let stop = ShutdownToken::new();
    let environment: Arc<Vec<(OsString, OsString)>> = Arc::new(env::vars_os().collect());
    let (listener, owned, model) = match bind_after(&socket, provenance, || {
        let model_environment = Environment::resolve(environment.iter().cloned());
        let model_inputs = RuntimeInputs::current()?;
        let model_layout = RuntimeLayout::resolve(&model_environment, &model_inputs)?;
        let model = if model_environment.allow_net() {
            yams_embed::JinaEmbedder::online(
                &model_layout.model_cache_dir,
                &model_layout.model_lock_dir,
            )
        } else {
            yams_embed::JinaEmbedder::offline(
                &model_layout.model_cache_dir,
                &model_layout.model_lock_dir,
            )
        }?;
        Ok::<_, Box<dyn std::error::Error>>(model)
    }) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("yams-service: {error}");
            std::process::exit(4);
        }
    };
    // The signature and dimensions are immutable properties of this model.
    // Capture them before sharing the model so metadata access never needs
    // to join the inference lock's critical section.
    let model_signature: Arc<str> = Arc::from(model.signature());
    let model_dimensions = model.dimensions();
    let model = Arc::new(Mutex::new(model));
    let model_for_handler = Arc::clone(&model);
    let signature_for_handler = Arc::clone(&model_signature);
    println!("READY");
    let result = serve_until(
        listener,
        Duration::from_secs(30),
        Some(idle_timeout),
        stop,
        move |request| {
            execute_request(
                &environment,
                &model_for_handler,
                &signature_for_handler,
                model_dimensions,
                request,
            )
        },
    );
    if let Err(error) = result {
        eprintln!("yams-service: {error}");
    }
    let _ = cleanup_owned_socket(&owned);
}

/// An embedder view that serializes only calls which actually run inference.
///
/// Service request preparation, store access, ranking, and rendering use this
/// adapter without holding the shared model mutex. The immutable model
/// metadata is captured once during startup and shared independently.
struct SharedEmbedder<E> {
    model: Arc<Mutex<E>>,
    signature: Arc<str>,
    dimensions: usize,
}

impl<E: Embedder> Embedder for SharedEmbedder<E> {
    fn signature(&self) -> &str {
        &self.signature
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn embed_passages(&mut self, texts: &[String]) -> Result<Vec<Embedding>, EmbeddingError> {
        self.model
            .lock()
            .map_err(|_| EmbeddingError::Backend("model lock poisoned".to_owned()))?
            .embed_passages(texts)
    }

    fn embed_query(&mut self, text: &str) -> Result<Embedding, EmbeddingError> {
        self.model
            .lock()
            .map_err(|_| EmbeddingError::Backend("model lock poisoned".to_owned()))?
            .embed_query(text)
    }
}

pub(crate) fn execute_request<E: Embedder>(
    environment: &[(OsString, OsString)],
    model: &Arc<Mutex<E>>,
    signature: &Arc<str>,
    dimensions: usize,
    request: Request,
) -> ExecutionOutput {
    if request.argv.iter().any(|argument| argument == "--write") {
        return ExecutionOutput::new(2, "", "yams: --write is not a service operation\n");
    }
    let mut embedder = SharedEmbedder {
        model: Arc::clone(model),
        signature: Arc::clone(signature),
        dimensions,
    };
    let when = InvocationTime::capture();
    let completion = execute_service_operation(
        request.operation,
        PathBuf::from(request.cwd),
        environment.iter().cloned(),
        &when,
        &mut embedder,
    );
    ExecutionOutput::new(
        i32::from(completion.exit_code) as u8,
        completion.stdout,
        completion.stderr,
    )
}
