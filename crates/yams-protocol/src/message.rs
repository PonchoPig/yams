use std::fmt;
use std::time::Duration;

/// Wire-protocol version used by both the Yams client and service.
pub const PROTOCOL_VERSION: u8 = 4;
/// Shared client handshake and service request-frame admission bound.
pub const ADMISSION_TIMEOUT: Duration = Duration::from_secs(2);
/// Finite wait for an accepted request to produce a completion frame.
pub const COMPLETION_TIMEOUT: Duration = Duration::from_secs(10 * 60);
/// Maximum encoded request body size, excluding the four-byte frame header.
pub const MAX_REQUEST_BYTES: usize = 64 * 1024;
/// Maximum encoded response body size, excluding the four-byte frame header.
pub const MAX_RESPONSE_BYTES: usize = 8 * 1024 * 1024;
/// Maximum number of entries in a request argument vector.
pub const MAX_ARGUMENTS: usize = 256;
/// Maximum UTF-8 byte length of one request argument.
pub const MAX_ARGUMENT_BYTES: usize = 16 * 1024;

/// Typed service operation. `--write` is not representable.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ServiceOperation {
    /// Discriminator executed by the service.
    pub kind: OperationKind,
    /// Search text; empty for management operations.
    pub query: String,
    /// Canonical `-k` spelling; unused for management operations.
    pub k: String,
    /// `--json`
    pub json: bool,
    /// `--full`
    pub full: bool,
    /// `--no-gate`
    pub no_gate: bool,
    /// `--explain`
    pub explain: bool,
    /// Optional `--min-score` spelling.
    pub min_score: Option<String>,
    /// Optional `--max-gap` spelling.
    pub max_gap: Option<String>,
    /// Optional `--project` path.
    pub project: Option<String>,
}

/// Service operations the daemon may execute.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OperationKind {
    /// Selected-project search.
    Search,
    /// All-project search.
    All,
    /// Build or rebuild the selected project search store.
    Index,
    /// Selected-project inventory counts.
    Stats,
    /// List known projects.
    Projects,
    /// Vector garbage collection.
    Gc,
}

impl OperationKind {
    /// Stable wire spelling.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Search => "search",
            Self::All => "all",
            Self::Index => "index",
            Self::Stats => "stats",
            Self::Projects => "projects",
            Self::Gc => "gc",
        }
    }

    /// Parse a wire kind. `write` is intentionally absent.
    pub fn parse(value: &str) -> Option<Self> {
        Some(match value {
            "search" => Self::Search,
            "all" => Self::All,
            "index" => Self::Index,
            "stats" => Self::Stats,
            "projects" => Self::Projects,
            "gc" => Self::Gc,
            _ => return None,
        })
    }
}

impl ServiceOperation {
    /// Derive a typed operation from debug argv. `--write` becomes [`None`].
    pub fn from_argv(argv: &[String]) -> Option<Self> {
        if argv.iter().any(|argument| argument == "--write") {
            return None;
        }
        let mut kind = OperationKind::Search;
        let mut query = String::new();
        let mut k = String::from("5");
        let mut json = false;
        let mut full = false;
        let mut no_gate = false;
        let mut explain = false;
        let mut min_score = None;
        let mut max_gap = None;
        let mut project = None;
        let mut after_separator = false;
        let mut index = 0;
        while index < argv.len() {
            let argument = &argv[index];
            if after_separator {
                query = argument.clone();
                index += 1;
                continue;
            }
            match argument.as_str() {
                "--" => after_separator = true,
                "--all" => kind = OperationKind::All,
                "--index" => kind = OperationKind::Index,
                "--stats" => kind = OperationKind::Stats,
                "--projects" => kind = OperationKind::Projects,
                "--gc" => kind = OperationKind::Gc,
                "--json" => json = true,
                "--full" => full = true,
                "--no-gate" => no_gate = true,
                "--explain" => explain = true,
                "-k" => {
                    index += 1;
                    if let Some(value) = argv.get(index) {
                        k = value.clone();
                    }
                }
                "--min-score" => {
                    index += 1;
                    min_score = argv.get(index).cloned();
                }
                "--max-gap" => {
                    index += 1;
                    max_gap = argv.get(index).cloned();
                }
                "--project" => {
                    index += 1;
                    project = argv.get(index).cloned();
                }
                value if !value.starts_with('-') => query = value.to_owned(),
                _ => {}
            }
            index += 1;
        }
        Some(Self {
            kind,
            query,
            k,
            json,
            full,
            no_gate,
            explain,
            min_score,
            max_gap,
            project,
        })
    }
}

/// A direct CLI invocation sent to the service.
#[derive(Clone, Eq, PartialEq)]
pub struct Request {
    /// Typed operation the service executes. `argv` is debug-only.
    pub operation: ServiceOperation,
    /// CLI arguments, excluding the executable name. Debug / oracle field.
    pub argv: Vec<String>,
    /// Absolute working directory from which to interpret the invocation.
    pub cwd: String,
}

impl Request {
    /// Build a request whose typed operation is derived from `argv`.
    ///
    /// Returns `None` when argv includes `--write`.
    pub fn from_argv(argv: Vec<String>, cwd: impl Into<String>) -> Option<Self> {
        let operation = ServiceOperation::from_argv(&argv)?;
        Some(Self {
            operation,
            argv,
            cwd: cwd.into(),
        })
    }
}

/// Acknowledgement that the service owns execution of a request.
#[derive(Clone, Eq, PartialEq)]
pub struct Accepted {
    /// Nonempty identifier shared with the terminal completion message.
    pub request_id: String,
}

/// Terminal result for one accepted request.
#[derive(Clone, Eq, PartialEq)]
pub struct Completed {
    /// Identifier from the corresponding [`Accepted`] message.
    pub request_id: String,
    /// Process-compatible exit status from 0 through 255.
    pub exit_code: u8,
    /// Complete bounded standard-output text.
    pub stdout: String,
    /// Complete bounded standard-error text.
    pub stderr: String,
}

/// Pre-acknowledgement refusal to execute a request.
#[derive(Clone, Eq, PartialEq)]
pub struct Rejected {
    /// Nonempty stable machine-readable refusal code.
    pub code: String,
    /// Bounded human-readable refusal detail.
    pub message: String,
}

/// One versioned request or response message.
#[derive(Clone, Eq, PartialEq)]
pub enum Message {
    /// Client request.
    Request(Request),
    /// Service acknowledgement.
    Accepted(Accepted),
    /// Service completion.
    Completed(Completed),
    /// Service refusal before acknowledgement.
    Rejected(Rejected),
}

impl fmt::Debug for Request {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Request { operation: <redacted>, argv: <redacted>, cwd: <redacted> }")
    }
}

impl fmt::Debug for Accepted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Accepted { request_id: <redacted> }")
    }
}

impl fmt::Debug for Completed {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Completed")
            .field("request_id", &Redacted)
            .field("exit_code", &self.exit_code)
            .field("stdout", &Redacted)
            .field("stderr", &Redacted)
            .finish()
    }
}

impl fmt::Debug for Rejected {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("Rejected { code: <redacted>, message: <redacted> }")
    }
}

impl fmt::Debug for Message {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Request(_) => formatter.write_str("Message::Request(<redacted>)"),
            Self::Accepted(_) => formatter.write_str("Message::Accepted(<redacted>)"),
            Self::Completed(completed) => formatter
                .debug_tuple("Message::Completed")
                .field(completed)
                .finish(),
            Self::Rejected(_) => formatter.write_str("Message::Rejected(<redacted>)"),
        }
    }
}

struct Redacted;

impl fmt::Debug for Redacted {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("<redacted>")
    }
}
