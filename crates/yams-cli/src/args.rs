use std::collections::{HashMap, HashSet};
use std::ffi::OsString;
use std::fmt;
use std::path::PathBuf;
use std::str::FromStr;

use clap::builder::TypedValueParser;
use clap::error::{ContextKind, ContextValue, ErrorKind};
use clap::{CommandFactory, Parser};
use unicode_general_category::{GeneralCategory, get_general_category};
use yams_core::{ExitCode, TerminalText, sanitize_terminal};

const DESCRIPTION: &str = "Semantic search over a project's agent memory.";
const LONG_OPTIONS: &[&str] = &[
    "--help",
    "--version",
    "--json",
    "--full",
    "--index",
    "--write",
    "--stats",
    "--project",
    "--all",
    "--projects",
    "--no-gate",
    "--explain",
    "--min-score",
    "--max-gap",
    "--gc",
];

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectOperation {
    Search,
    All,
    Write,
    Index,
    Stats,
    Projects,
    Gc,
}

impl fmt::Display for DirectOperation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Search => "search",
            Self::All => "all-project search",
            Self::Write => "write",
            Self::Index => "index",
            Self::Stats => "stats",
            Self::Projects => "projects",
            Self::Gc => "garbage collection",
        })
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DirectRequest {
    pub operation: DirectOperation,
    pub project: Option<PathBuf>,
    pub query: Option<String>,
    pub k: usize,
    /// Canonical decimal spelling requested by the caller before the bounded
    /// execution count saturates at [`usize::MAX`].
    pub requested_k: String,
    pub json: bool,
    pub full: bool,
    pub no_gate: bool,
    pub explain: bool,
    pub min_score: Option<f64>,
    pub max_gap: Option<f64>,
}

impl DirectRequest {
    pub fn query(&self) -> Option<&str> {
        self.query.as_deref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectCompletion {
    pub exit_code: ExitCode,
    pub stdout: String,
    pub stderr: String,
}

impl DirectCompletion {
    fn help(exit_code: ExitCode) -> Self {
        let mut command = CliArgs::command();
        Self {
            exit_code,
            stdout: sanitize_terminal(
                &command.render_long_help().to_string(),
                TerminalText::Multiline,
            )
            .into_owned(),
            stderr: String::new(),
        }
    }

    fn version() -> Self {
        Self {
            exit_code: ExitCode::Ok,
            stdout: format!("yams {}\n", env!("CARGO_PKG_VERSION")),
            stderr: String::new(),
        }
    }

    fn usage(message: impl Into<String>, writes: bool, hint: &str) -> Self {
        let message = message.into();
        if writes {
            let stdout = compact_json_line(&serde_json::json!({
                "ok": false,
                "exit": i32::from(ExitCode::Usage),
                "error": message,
                "hint": hint,
            }));
            Self {
                exit_code: ExitCode::Usage,
                stdout,
                stderr: String::new(),
            }
        } else {
            let message = sanitize_terminal(&message, TerminalText::Inline);
            Self {
                exit_code: ExitCode::Usage,
                stdout: String::new(),
                stderr: format!("{message}\n"),
            }
        }
    }

    fn parser_usage(message: impl Into<String>, writes: bool) -> Self {
        let message = message.into();
        if writes {
            return Self::usage(message, true, "fix the invocation and retry");
        }
        let message = sanitize_terminal(&message, TerminalText::Inline);
        Self {
            exit_code: ExitCode::Usage,
            stdout: String::new(),
            stderr: format!("error: {message}\n\nUsage: yams [OPTIONS] [QUERY]...\n"),
        }
    }

    pub(crate) fn configuration(message: String, writes: bool) -> Self {
        Self::usage(message, writes, "fix the configuration and retry")
    }

    pub(crate) fn operational(message: String) -> Self {
        let message = sanitize_terminal(&message, TerminalText::Inline);
        Self {
            exit_code: ExitCode::Operational,
            stdout: String::new(),
            stderr: format!("{message}\n"),
        }
    }

    pub(crate) fn operational_for_mode(message: String, writes: bool) -> Self {
        if writes {
            return Self::machine_failure(
                ExitCode::Operational,
                message,
                "retry after fixing the runtime failure",
            );
        }
        Self::operational(message)
    }

    pub(crate) fn machine_failure(
        exit_code: ExitCode,
        message: impl Into<String>,
        hint: &str,
    ) -> Self {
        let stdout = compact_json_line(&serde_json::json!({
            "ok": false,
            "exit": i32::from(exit_code),
            "error": message.into(),
            "hint": hint,
        }));
        Self {
            exit_code,
            stdout,
            stderr: String::new(),
        }
    }

    pub(crate) fn classified_failure(fault: crate::Fault) -> Self {
        let hint = if fault.transient {
            "retry the same request shortly"
        } else if fault.code == "store_missing" {
            "run yams --index from the project; add YAMS_ALLOW_NET=1 only if the model cache is empty"
        } else {
            "inspect the index and retry"
        };
        let stdout = compact_json_line(&serde_json::json!({
            "ok": false,
            "exit": i32::from(fault.exit_code),
            "code": fault.code,
            "transient": fault.transient,
            "error": fault.message,
            "hint": hint,
        }));
        Self {
            exit_code: fault.exit_code,
            stdout,
            stderr: String::new(),
        }
    }
}

pub(crate) fn compact_json_line(value: &serde_json::Value) -> String {
    let serialized = serde_json::to_string(value).expect("machine response is serializable");
    let mut safe = String::with_capacity(serialized.len() + 1);
    for character in serialized.chars() {
        if matches!(character, '\u{007f}'..='\u{009f}') {
            use std::fmt::Write as _;
            write!(safe, "\\u{:04x}", u32::from(character))
                .expect("writing to a String cannot fail");
        } else {
            safe.push(character);
        }
    }
    safe.push('\n');
    safe
}

#[derive(Clone, Debug, PartialEq)]
pub enum ParseOutcome {
    Request(DirectRequest),
    Completion(DirectCompletion),
}

#[derive(Debug, Parser)]
#[command(
    name = "yams",
    about = DESCRIPTION,
    long_about = DESCRIPTION,
    infer_long_args = false,
    args_override_self = true,
    disable_version_flag = true,
    after_long_help = "environment:\n  YAMS_HOME            place all mutable state beneath an explicit root\n  YAMS_DIRS            trusted path-list replacing corpus discovery; `/` and symlink escapes are refused\n  YAMS_ALLOW_NET       set to 1 to allow model downloads\n  YAMS_NO_SERVICE      set to 1 to force direct execution\n  YAMS_SERVICE_SOCKET  override the local service socket"
)]
struct CliArgs {
    /// What you want to recall.
    #[arg(value_name = "QUERY")]
    query: Vec<String>,

    /// Number of results. Hybrid fusion still ranks at most 25 pages per source.
    #[arg(short = 'k', value_name = "N")]
    k: Option<CountArgument>,

    /// Print version and exit.
    #[arg(long)]
    version: bool,

    /// Emit machine-readable output.
    #[arg(long)]
    json: bool,

    /// Print whole chunks instead of snippets.
    #[arg(long)]
    full: bool,

    /// Build or rebuild the selected project's search store.
    #[arg(long)]
    index: bool,

    /// Read one JSON request from stdin and write one wiki page.
    #[arg(long)]
    write: bool,

    /// Report selected-project index status.
    #[arg(long)]
    stats: bool,

    /// Target another project instead of the current directory.
    #[arg(
        long,
        value_name = "PATH",
        value_parser = clap::builder::OsStringValueParser::new().map(PathBuf::from)
    )]
    project: Option<PathBuf>,

    /// Search every indexed project.
    #[arg(long)]
    all: bool,

    /// List indexed projects.
    #[arg(long)]
    projects: bool,

    /// Show low-confidence matches the gate would suppress.
    #[arg(long)]
    no_gate: bool,

    /// Explain the gate verdict and ranking contributions.
    #[arg(long)]
    explain: bool,

    /// Override the gate's cosine floor for this run.
    #[arg(long, value_name = "FLOAT")]
    min_score: Option<FloatArgument>,

    /// Override the gate's maximum score gap for this run.
    #[arg(long, value_name = "FLOAT")]
    max_gap: Option<FloatArgument>,

    /// Drop cached vectors no valid index references.
    #[arg(long)]
    gc: bool,
}

pub fn parse_direct_request<I, A>(arguments: I) -> ParseOutcome
where
    I: IntoIterator<Item = A>,
    A: Into<OsString>,
{
    let raw: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    let writes = selects_write(&raw);
    let normalized = normalize_arguments(&raw);

    let mut args = match CliArgs::try_parse_from(normalized.arguments.iter().cloned()) {
        Ok(args) => args,
        Err(error) if error.kind() == ErrorKind::DisplayHelp => {
            return ParseOutcome::Completion(DirectCompletion {
                exit_code: ExitCode::Ok,
                stdout: sanitize_terminal(&error.to_string(), TerminalText::Multiline).into_owned(),
                stderr: String::new(),
            });
        }
        Err(error) => {
            let message = clap_message(&error, &raw);
            return ParseOutcome::Completion(DirectCompletion::parser_usage(message, writes));
        }
    };
    if let Some(message) = &normalized.pending_error {
        return ParseOutcome::Completion(DirectCompletion::parser_usage(message.clone(), writes));
    }
    for query in &mut args.query {
        if let Some(original) = normalized.positional_rewrites.get(query) {
            query.clone_from(original);
        }
    }
    if let Some(message) = interrupted_query_message(&normalized) {
        return ParseOutcome::Completion(DirectCompletion::parser_usage(message, writes));
    }

    validate(args, writes)
}

struct NormalizedArguments {
    arguments: Vec<OsString>,
    positional_rewrites: HashMap<String, String>,
    unknown_options: HashMap<String, String>,
    pending_error: Option<String>,
}

fn normalize_arguments(raw: &[OsString]) -> NormalizedArguments {
    let mut arguments = Vec::with_capacity(raw.len().max(1));
    arguments.push(OsString::from("yams"));
    let mut reserved_arguments: HashSet<OsString> = raw.iter().cloned().collect();
    let mut positional_rewrites = HashMap::new();
    let mut unknown_options = HashMap::new();
    let mut pending_error = None;
    let mut placeholder_discriminator = 0_u64;
    let mut options = true;
    let mut index = 1;
    while index < raw.len() {
        let argument = &raw[index];
        if !options {
            arguments.push(argument.clone());
            index += 1;
            continue;
        }
        if argument == "--" {
            options = false;
            arguments.push(argument.clone());
            index += 1;
            continue;
        }
        let Some(text) = argument.to_str() else {
            arguments.push(argument.clone());
            index += 1;
            continue;
        };
        if text == "-h" {
            arguments.push(argument.clone());
            break;
        }
        if let Some(help_tail) = text.strip_prefix("-h") {
            if let Some(explicit) = help_tail.strip_prefix('=') {
                pending_error = Some(format!(
                    "argument -h/--help: ignored explicit argument '{explicit}'"
                ));
                break;
            }
            if help_tail.starts_with('-') {
                pending_error = Some(format!(
                    "argument -h/--help: ignored explicit argument '{help_tail}'"
                ));
                break;
            }
            if help_tail == "k"
                && raw
                    .get(index + 1)
                    .is_none_or(|value| value.to_str().is_some_and(is_argparse_option_looking))
            {
                arguments.push(OsString::from("-k"));
                arguments.push(OsString::from("--json"));
                break;
            }
            arguments.push(OsString::from("-h"));
            break;
        }

        let canonical = if text.starts_with("--") {
            let (name, value) = text
                .split_once('=')
                .map_or((text, None), |(name, value)| (name, Some(value)));
            let matches: Vec<_> = LONG_OPTIONS
                .iter()
                .copied()
                .filter(|option| option.starts_with(name))
                .collect();
            let resolved = if LONG_OPTIONS.contains(&name) {
                name
            } else {
                match matches.as_slice() {
                    [] => {
                        let placeholder = fresh_placeholder(
                            &mut reserved_arguments,
                            &mut placeholder_discriminator,
                        );
                        unknown_options.insert(placeholder.clone(), text.to_owned());
                        arguments.push(OsString::from(placeholder));
                        index += 1;
                        continue;
                    }
                    [resolved] => resolved,
                    many => {
                        pending_error = Some(format!(
                            "ambiguous option: {name} could match {}",
                            many.join(", ")
                        ));
                        break;
                    }
                }
            };
            value.map_or_else(
                || resolved.to_owned(),
                |value| format!("{resolved}={value}"),
            )
        } else if text == "-k" || text.starts_with("-k") {
            text.to_owned()
        } else if is_argparse_option_looking(text) {
            let placeholder =
                fresh_placeholder(&mut reserved_arguments, &mut placeholder_discriminator);
            unknown_options.insert(placeholder.clone(), text.to_owned());
            arguments.push(OsString::from(placeholder));
            index += 1;
            continue;
        } else if text == "index" {
            "--index".to_owned()
        } else if text != "-"
            && text.starts_with('-')
            && (is_python_negative_number(text) || text.contains(' '))
        {
            let placeholder =
                fresh_placeholder(&mut reserved_arguments, &mut placeholder_discriminator);
            positional_rewrites.insert(placeholder.clone(), text.to_owned());
            arguments.push(OsString::from(placeholder));
            index += 1;
            continue;
        } else {
            text.to_owned()
        };

        if canonical == "--help" || canonical == "--version" {
            arguments.push(OsString::from(canonical));
            break;
        }

        let needs_separated_value = matches!(
            canonical.as_str(),
            "-k" | "--project" | "--min-score" | "--max-gap"
        );
        if !needs_separated_value {
            arguments.push(OsString::from(canonical));
            index += 1;
            continue;
        }

        let Some(value) = raw.get(index + 1) else {
            arguments.push(OsString::from(canonical));
            break;
        };
        let Some(value_text) = value.to_str() else {
            arguments.push(OsString::from(canonical));
            arguments.push(value.clone());
            index += 2;
            continue;
        };
        if value_text.starts_with('-') && !is_argparse_option_looking(value_text) {
            arguments.push(OsString::from(format!("{canonical}={value_text}")));
            index += 2;
            continue;
        }
        if is_argparse_option_looking(value_text) {
            // argparse reports the earlier option's missing value before it
            // resolves the option-looking token. A known flag makes Clap take
            // the same branch without exposing or interpreting that token.
            arguments.push(OsString::from(canonical));
            arguments.push(OsString::from("--json"));
            break;
        }
        arguments.push(OsString::from(canonical));
        arguments.push(value.clone());
        index += 2;
    }
    NormalizedArguments {
        arguments,
        positional_rewrites,
        unknown_options,
        pending_error,
    }
}

fn fresh_placeholder(
    reserved_arguments: &mut HashSet<OsString>,
    discriminator: &mut u64,
) -> String {
    loop {
        let candidate = format!("\u{e000}yams-argument-{discriminator}\u{e001}");
        *discriminator = discriminator
            .checked_add(1)
            .expect("an argument vector cannot exhaust u64 placeholders");
        if reserved_arguments.insert(OsString::from(&candidate)) {
            return candidate;
        }
    }
}

fn is_python_negative_number(argument: &str) -> bool {
    let Some(unsigned) = argument.strip_prefix('-') else {
        return false;
    };
    if !unsigned.is_empty()
        && unsigned
            .chars()
            .all(|character| decimal_digit(character).is_some())
    {
        return true;
    }
    let Some((integer, fraction)) = unsigned.split_once('.') else {
        return false;
    };
    !fraction.is_empty()
        && !fraction.contains('.')
        && integer
            .chars()
            .all(|character| decimal_digit(character).is_some())
        && fraction
            .chars()
            .all(|character| decimal_digit(character).is_some())
}

fn is_argparse_option_looking(argument: &str) -> bool {
    argument != "-"
        && argument.starts_with('-')
        && (argument.starts_with("-h")
            || argument.starts_with("-k")
            || (!argument.contains(' ') && !is_python_negative_number(argument)))
}

fn interrupted_query_message(normalized: &NormalizedArguments) -> Option<String> {
    let mut options = true;
    let mut option_needs_value = false;
    let mut query_started = false;
    let mut query_interrupted = false;
    let mut unexpected = Vec::new();

    for argument in normalized.arguments.iter().skip(1) {
        if !options {
            if query_interrupted {
                unexpected.push(original_query_argument(argument, normalized));
            }
            continue;
        }
        if option_needs_value {
            option_needs_value = false;
            continue;
        }
        let Some(text) = argument.to_str() else {
            query_started = true;
            continue;
        };
        if let Some(original) = normalized.unknown_options.get(text) {
            unexpected.push(original.clone());
            query_interrupted |= query_started;
            continue;
        }
        if text == "--" {
            options = false;
            if query_interrupted {
                unexpected.push("--".to_owned());
            }
            continue;
        }
        if let Some(needs_value) = recognized_option_needs_value(text) {
            query_interrupted |= query_started;
            option_needs_value = needs_value;
            continue;
        }
        if query_interrupted {
            unexpected.push(original_query_argument(argument, normalized));
        } else {
            query_started = true;
        }
    }

    (!unexpected.is_empty()).then(|| format!("unrecognized arguments: {}", unexpected.join(" ")))
}

fn recognized_option_needs_value(argument: &str) -> Option<bool> {
    if argument == "-k" {
        return Some(true);
    }
    if argument.starts_with("-k") {
        return Some(false);
    }
    let (name, attached_value) = argument
        .split_once('=')
        .map_or((argument, false), |(name, _)| (name, true));
    LONG_OPTIONS
        .contains(&name)
        .then_some(!attached_value && matches!(name, "--project" | "--min-score" | "--max-gap"))
}

fn original_query_argument(argument: &OsString, normalized: &NormalizedArguments) -> String {
    let rendered = argument.to_string_lossy();
    normalized
        .positional_rewrites
        .get(rendered.as_ref())
        .or_else(|| normalized.unknown_options.get(rendered.as_ref()))
        .cloned()
        .unwrap_or_else(|| rendered.into_owned())
}

fn selects_write(raw: &[OsString]) -> bool {
    raw.iter()
        .skip(1)
        .take_while(|argument| *argument != "--")
        .any(|argument| {
            argument.to_str().is_some_and(|argument| {
                let name = argument.split_once('=').map_or(argument, |(name, _)| name);
                name.len() > 2 && "--write".starts_with(name)
            })
        })
}

fn clap_message(error: &clap::Error, raw: &[OsString]) -> String {
    let argument = match error.get(ContextKind::InvalidArg) {
        Some(ContextValue::String(argument)) => Some(argument.as_str()),
        _ => None,
    };
    let value = match error.get(ContextKind::InvalidValue) {
        Some(ContextValue::String(value)) => Some(value.as_str()),
        _ => None,
    };
    match error.kind() {
        ErrorKind::ValueValidation => {
            if let (Some(argument), Some(value), Some(source)) =
                (argument, value, std::error::Error::source(error))
            {
                // Clap's renderer interprets escape sequences in a custom
                // parser's error text. Rebuild from raw structured context so
                // machine JSON preserves it for the serializer to escape.
                return format!("invalid value '{value}' for '{argument}': {source}");
            }
        }
        ErrorKind::TooManyValues => {
            if let (Some(argument), Some(value)) = (argument, value) {
                return format!(
                    "unexpected value '{value}' for '{argument}' found; no more were expected"
                );
            }
        }
        ErrorKind::UnknownArgument => {
            if let Some(argument) = argument {
                let raw_argument = raw.iter().skip(1).find_map(|candidate| {
                    candidate
                        .to_str()
                        .filter(|candidate| candidate.starts_with(argument))
                });
                return format!(
                    "unexpected argument '{}' found",
                    raw_argument.unwrap_or(argument)
                );
            }
        }
        ErrorKind::InvalidValue => {
            if let (Some(argument), Some(value)) = (argument, value) {
                return if value.is_empty() {
                    format!("a value is required for '{argument}' but none was supplied")
                } else {
                    format!("invalid value '{value}' for '{argument}'")
                };
            }
        }
        _ => {}
    }

    let rendered = error.to_string();
    let rendered = rendered.as_str();
    let body = rendered.strip_prefix("error: ").unwrap_or(rendered);
    // Work backward from Clap's generated suffix. Splitting at the first
    // blank line lets a caller-controlled invalid value truncate itself (and
    // the JSON diagnostic) by containing two newlines.
    body.rsplit_once("\n\nUsage: yams ")
        .or_else(|| body.rsplit_once("\n\nFor more information"))
        .map_or(body, |(message, _suffix)| message)
        .trim_end_matches('\n')
        .to_owned()
}

fn validate(args: CliArgs, writes: bool) -> ParseOutcome {
    if args.version {
        return ParseOutcome::Completion(DirectCompletion::version());
    }
    let min_score = args.min_score.map(|argument| argument.value);
    let max_gap = args.max_gap.map(|argument| argument.value);
    if let Some(value) = min_score
        && (!value.is_finite() || !(-1.0..=1.0).contains(&value))
    {
        return usage(
            format!(
                "--min-score must be within [-1.0, 1.0], the range of a cosine similarity; got {}",
                python_float_repr(value)
            ),
            writes,
        );
    }
    if let Some(value) = max_gap
        && (!value.is_finite() || value < 0.0)
    {
        return usage(
            format!(
                "--max-gap must be 0.0 or greater; got {}",
                python_float_repr(value)
            ),
            writes,
        );
    }
    if let Some(value) = &args.k
        && value.less_than_one
    {
        return usage(
            format!("-k must be 1 or greater; got {}", value.canonical),
            writes,
        );
    }
    if args.no_gate && !args.explain && (min_score.is_some() || max_gap.is_some()) {
        return usage(
            "--min-score/--max-gap do nothing with --no-gate: the gate does not run, so they can neither filter nor annotate. Add --explain to see what they would have done, or drop --no-gate to apply them.",
            writes,
        );
    }
    if args.explain && args.all {
        return usage(
            "--explain covers one project at a time: every project has its own gate baseline, so one verdict cannot describe them. Drop --all — with --project PATH to pick a different one.",
            writes,
        );
    }

    let selected = selected_operations(&args);
    if selected.len() > 1 {
        if args.write {
            let conflicts = [
                ("--index", args.index),
                ("--stats", args.stats),
                ("--all", args.all),
                ("--projects", args.projects),
                ("--gc", args.gc),
            ]
            .into_iter()
            .filter_map(|(name, selected)| selected.then_some(name))
            .collect::<Vec<_>>()
            .join(", ");
            return ParseOutcome::Completion(DirectCompletion::usage(
                format!("--write cannot be combined with {conflicts}"),
                true,
                "run the write on its own",
            ));
        }
        return usage(
            format!(
                "choose exactly one operation; cannot combine {}",
                selected.join(", ")
            ),
            false,
        );
    }

    let operation = operation(&args);
    if args.project.is_some()
        && matches!(
            operation,
            DirectOperation::All | DirectOperation::Projects | DirectOperation::Gc
        )
    {
        return usage(
            format!(
                "--project is not valid with --{}",
                operation_flag(operation)
            ),
            writes,
        );
    }
    if args.json && operation == DirectOperation::Write {
        return usage("--json has no effect with --write", true);
    }
    if args.k.is_some() && !is_search(operation) {
        return usage("-k is only valid with search", writes);
    }
    if args.full && !is_search(operation) {
        return usage("--full is only valid with search", writes);
    }
    for (selected, name) in [
        (args.no_gate, "--no-gate"),
        (args.explain, "--explain"),
        (min_score.is_some(), "--min-score"),
        (max_gap.is_some(), "--max-gap"),
    ] {
        if selected && !is_search(operation) {
            return usage(format!("{name} is only valid with search"), writes);
        }
    }

    let query = args.query.join(" ");
    if !is_search(operation) && !args.query.is_empty() {
        return usage(
            format!("--{} does not accept query text", operation_flag(operation)),
            writes,
        );
    }
    if operation == DirectOperation::All
        && query.trim_matches(is_python_string_whitespace).is_empty()
    {
        return usage("--all needs a query", writes);
    }
    if operation == DirectOperation::Search {
        if args.query.is_empty() {
            return ParseOutcome::Completion(DirectCompletion::help(ExitCode::Usage));
        }
        if query.trim_matches(is_python_string_whitespace).is_empty() {
            return usage("empty query: nothing to search for", writes);
        }
    }

    ParseOutcome::Request(DirectRequest {
        operation,
        project: args.project,
        query: is_search(operation).then_some(query),
        k: args.k.as_ref().map_or(5, |value| value.saturated),
        requested_k: args
            .k
            .map_or_else(|| "5".to_owned(), |value| value.canonical),
        json: args.json,
        full: args.full,
        no_gate: args.no_gate,
        explain: args.explain,
        min_score,
        max_gap,
    })
}

fn selected_operations(args: &CliArgs) -> Vec<&'static str> {
    [
        ("--index", args.index),
        ("--write", args.write),
        ("--stats", args.stats),
        ("--all", args.all),
        ("--projects", args.projects),
        ("--gc", args.gc),
    ]
    .into_iter()
    .filter_map(|(name, selected)| selected.then_some(name))
    .collect()
}

fn operation(args: &CliArgs) -> DirectOperation {
    if args.index {
        DirectOperation::Index
    } else if args.write {
        DirectOperation::Write
    } else if args.stats {
        DirectOperation::Stats
    } else if args.all {
        DirectOperation::All
    } else if args.projects {
        DirectOperation::Projects
    } else if args.gc {
        DirectOperation::Gc
    } else {
        DirectOperation::Search
    }
}

fn operation_flag(operation: DirectOperation) -> &'static str {
    match operation {
        DirectOperation::Search => "search",
        DirectOperation::All => "all",
        DirectOperation::Write => "write",
        DirectOperation::Index => "index",
        DirectOperation::Stats => "stats",
        DirectOperation::Projects => "projects",
        DirectOperation::Gc => "gc",
    }
}

fn is_search(operation: DirectOperation) -> bool {
    matches!(operation, DirectOperation::Search | DirectOperation::All)
}

fn usage(message: impl Into<String>, writes: bool) -> ParseOutcome {
    ParseOutcome::Completion(DirectCompletion::usage(
        message,
        writes,
        "fix the invocation and retry",
    ))
}

#[derive(Clone, Debug)]
struct CountArgument {
    saturated: usize,
    canonical: String,
    less_than_one: bool,
}

impl FromStr for CountArgument {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_python_int(value).ok_or_else(|| {
            format!(
                "invalid int value: '{}'",
                value.replace('\\', "\\\\").replace('\'', "\\'")
            )
        })
    }
}

fn parse_python_int(value: &str) -> Option<CountArgument> {
    let value = value.trim_matches(is_python_whitespace);
    let (negative, digits) = if let Some(value) = value.strip_prefix('-') {
        (true, value)
    } else if let Some(value) = value.strip_prefix('+') {
        (false, value)
    } else {
        (false, value)
    };
    if digits.is_empty() {
        return None;
    }

    let mut normalized = Vec::with_capacity(digits.len());
    let mut previous_was_digit = false;
    for character in digits.chars() {
        if character == '_' {
            if !previous_was_digit {
                return None;
            }
            previous_was_digit = false;
            continue;
        }
        normalized.push(decimal_digit(character)?);
        if normalized.len() > 4_300 {
            return None;
        }
        previous_was_digit = true;
    }
    if !previous_was_digit {
        return None;
    }

    let first_nonzero = normalized
        .iter()
        .position(|digit| *digit != 0)
        .unwrap_or(normalized.len());
    if first_nonzero == normalized.len() {
        return Some(CountArgument {
            saturated: 0,
            canonical: "0".to_owned(),
            less_than_one: true,
        });
    }
    let significant = &normalized[first_nonzero..];
    let magnitude = significant
        .iter()
        .map(|digit| char::from(b'0' + *digit))
        .collect::<String>();
    if negative {
        return Some(CountArgument {
            saturated: 0,
            canonical: format!("-{magnitude}"),
            less_than_one: true,
        });
    }

    let saturated = significant.iter().fold(0usize, |value, digit| {
        value
            .checked_mul(10)
            .and_then(|value| value.checked_add(usize::from(*digit)))
            .unwrap_or(usize::MAX)
    });
    Some(CountArgument {
        saturated,
        canonical: magnitude,
        less_than_one: false,
    })
}

#[derive(Clone, Copy, Debug)]
struct FloatArgument {
    value: f64,
}

impl FromStr for FloatArgument {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        parse_python_float(value)
            .map(|value| Self { value })
            .ok_or_else(|| invalid_numeric_value("float", value))
    }
}

fn invalid_numeric_value(kind: &str, value: &str) -> String {
    format!(
        "invalid {kind} value: '{}'",
        value.replace('\\', "\\\\").replace('\'', "\\'")
    )
}

fn parse_python_float(value: &str) -> Option<f64> {
    let value = value.trim_matches(is_python_whitespace);
    if value.is_empty() {
        return None;
    }

    let (sign, unsigned) = if let Some(unsigned) = value.strip_prefix('-') {
        (Some('-'), unsigned)
    } else if let Some(unsigned) = value.strip_prefix('+') {
        (Some('+'), unsigned)
    } else {
        (None, value)
    };
    let special = unsigned.to_ascii_lowercase();
    if matches!(special.as_str(), "nan" | "inf" | "infinity") {
        return match (sign, special.as_str()) {
            (Some('-'), "nan") => Some(-f64::NAN),
            (_, "nan") => Some(f64::NAN),
            (Some('-'), _) => Some(f64::NEG_INFINITY),
            _ => Some(f64::INFINITY),
        };
    }

    let characters: Vec<char> = value.chars().collect();
    let mut normalized = String::with_capacity(value.len());
    for (index, character) in characters.iter().copied().enumerate() {
        if let Some(digit) = decimal_digit(character) {
            normalized.push(char::from(b'0' + digit));
            continue;
        }
        if character == '_' {
            let between_decimal_digits = index > 0
                && index + 1 < characters.len()
                && decimal_digit(characters[index - 1]).is_some()
                && decimal_digit(characters[index + 1]).is_some();
            if !between_decimal_digits {
                return None;
            }
            continue;
        }
        if matches!(character, '+' | '-' | '.' | 'e' | 'E') {
            normalized.push(character);
            continue;
        }
        return None;
    }
    if !valid_decimal_float_shape(normalized.as_bytes()) {
        return None;
    }
    normalized.parse().ok()
}

fn valid_decimal_float_shape(value: &[u8]) -> bool {
    let mut index = usize::from(matches!(value.first(), Some(b'+' | b'-')));
    let integer_start = index;
    while value.get(index).is_some_and(u8::is_ascii_digit) {
        index += 1;
    }
    let integer_digits = index - integer_start;
    let mut fraction_digits = 0;
    if value.get(index) == Some(&b'.') {
        index += 1;
        let fraction_start = index;
        while value.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        fraction_digits = index - fraction_start;
    }
    if integer_digits + fraction_digits == 0 {
        return false;
    }
    if matches!(value.get(index), Some(b'e' | b'E')) {
        index += 1;
        if matches!(value.get(index), Some(b'+' | b'-')) {
            index += 1;
        }
        let exponent_start = index;
        while value.get(index).is_some_and(u8::is_ascii_digit) {
            index += 1;
        }
        if index == exponent_start {
            return false;
        }
    }
    index == value.len()
}

fn decimal_digit(character: char) -> Option<u8> {
    if get_general_category(character) != GeneralCategory::DecimalNumber {
        return None;
    }
    let mut block_start = u32::from(character);
    while block_start > 0 {
        let previous = char::from_u32(block_start - 1);
        if previous
            .is_none_or(|previous| get_general_category(previous) != GeneralCategory::DecimalNumber)
        {
            break;
        }
        block_start -= 1;
    }
    Some(((u32::from(character) - block_start) % 10) as u8)
}

fn is_python_whitespace(character: char) -> bool {
    // CPython's integer and float Unicode transforms use this whitespace
    // table, which is narrower than str.isspace()/str.strip() at
    // U+001C..U+001F.
    matches!(
        character,
        '\u{0009}'..='\u{000d}'
            | '\u{0020}'
            | '\u{0085}'
            | '\u{00a0}'
            | '\u{1680}'
            | '\u{2000}'..='\u{200a}'
            | '\u{2028}'
            | '\u{2029}'
            | '\u{202f}'
            | '\u{205f}'
            | '\u{3000}'
    )
}

fn is_python_string_whitespace(character: char) -> bool {
    is_python_whitespace(character) || matches!(character, '\u{001c}'..='\u{001f}')
}

fn python_float_repr(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value == f64::INFINITY {
        return "inf".to_owned();
    }
    if value == f64::NEG_INFINITY {
        return "-inf".to_owned();
    }

    // Ryū chooses the closest member when more than one shortest decimal
    // round-trips to the same binary64 value. Rust's Debug formatter is also
    // shortest-round-trip, but does not make CPython's choice in every tie.
    let mut buffer = ryu::Buffer::new();
    let rendered = buffer.format_finite(value);
    if let Some((mantissa, exponent)) = rendered.split_once('e') {
        return format_python_exponent(mantissa, exponent);
    }

    // CPython switches to exponential notation below 1e-4. Ryū's pretty
    // presentation keeps exponent -5 in fixed notation, so convert that
    // presentation while retaining Ryū's already-selected shortest digits.
    let unsigned = rendered.strip_prefix('-').unwrap_or(rendered);
    if value != 0.0
        && let Some(fraction) = unsigned.strip_prefix("0.")
        && let Some(first_nonzero) = fraction.find(|character| character != '0')
    {
        let exponent = -(i32::try_from(first_nonzero).expect("finite decimal length fits i32") + 1);
        if exponent < -4 {
            let significant = &fraction[first_nonzero..];
            let (first, rest) = significant.split_at(1);
            let sign = if value.is_sign_negative() { "-" } else { "" };
            let mantissa = if rest.is_empty() {
                format!("{sign}{first}")
            } else {
                format!("{sign}{first}.{rest}")
            };
            return format_python_exponent(&mantissa, &exponent.to_string());
        }
    }
    rendered.to_owned()
}

fn format_python_exponent(mantissa: &str, exponent: &str) -> String {
    let exponent: i32 = exponent
        .parse()
        .expect("Ryū's finite f64 exponent is an integer");
    if exponent < 0 {
        format!("{mantissa}e-{:02}", exponent.unsigned_abs())
    } else {
        format!("{mantissa}e+{exponent:02}")
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_python_float, python_float_repr};

    #[test]
    fn python_float_parser_matches_the_frozen_cpython_312_fixture() {
        // Generated by applying CPython 3.12.13 float() and reading the exact
        // binary64 bits. These pin grammar, Unicode transformation, rounding,
        // underflow, overflow, and signed special values independently of the
        // Rust formatter used by the diagnostics.
        let accepted = [
            ("\u{0085}+٠.٥_٠\u{3000}", 0x3fe0_0000_0000_0000),
            ("١.٢_٥e+٠_٢", 0x405f_4000_0000_0000),
            ("-٤.٩٤٠٦٥٦٤٥٨٤١٢٤٦٥٤e-٣٢٤", 0x8000_0000_0000_0001),
            ("1_7976931348623157e292", 0x7fef_ffff_ffff_ffff),
            ("1e3_09", 0x7ff0_0000_0000_0000),
            ("1e-4_00", 0x0000_0000_0000_0000),
            ("-nan", 0xfff8_0000_0000_0000),
            ("9007199254740993", 0x4340_0000_0000_0000),
            ("2.2250738585072011e-308", 0x000f_ffff_ffff_ffff),
            ("2.2250738585072012e-308", 0x0010_0000_0000_0000),
            ("2.4703282292062327e-324", 0x0000_0000_0000_0000),
            ("2.4703282292062328e-324", 0x0000_0000_0000_0001),
            ("1.7976931348623158e308", 0x7fef_ffff_ffff_ffff),
            ("1.7976931348623159e308", 0x7ff0_0000_0000_0000),
            ("-1e-4000", 0x8000_0000_0000_0000),
        ];
        for (spelling, expected_bits) in accepted {
            assert_eq!(
                parse_python_float(spelling).map(f64::to_bits),
                Some(expected_bits),
                "{spelling:?}"
            );
        }

        for spelling in [
            "",
            " ",
            ".",
            "_1",
            "1_",
            "1__2",
            "1_.0",
            "1._0",
            "._1",
            "1e",
            "1e+",
            "1e_1",
            "1e+_1",
            "1e1_",
            "1_e1",
            "nan_",
            "i_nf",
            "nan(payload)",
            "0x1p2",
            "−1",
            "\u{001c}1\u{001c}",
            "\u{feff}1\u{feff}",
            "\u{200b}1\u{200b}",
            "1\u{2003}2",
        ] {
            assert_eq!(parse_python_float(spelling), None, "{spelling:?}");
        }
    }

    #[test]
    fn python_float_repr_matches_the_frozen_cpython_boundary_fixture() {
        // Generated with CPython 3.12 repr(float) from the exact binary64 bits.
        let cases = [
            (0x0000_0000_0000_0000, "0.0"),
            (0x8000_0000_0000_0000, "-0.0"),
            (0x3ff0_0000_0000_0000, "1.0"),
            (0xbff0_0000_0000_0000, "-1.0"),
            (0x0000_0000_0000_0001, "5e-324"),
            (0x000f_ffff_ffff_ffff, "2.225073858507201e-308"),
            (0x0010_0000_0000_0000, "2.2250738585072014e-308"),
            (0x7fef_ffff_ffff_ffff, "1.7976931348623157e+308"),
            (0x3fef_ffff_ffff_ffff, "0.9999999999999999"),
            (0x3ff0_0000_0000_0001, "1.0000000000000002"),
            (0xc30c_6bf5_2634_0002, "-1000000000000000.2"),
            (0x3ee4_f8b5_88e3_68f1, "1e-05"),
            (0x3f1a_36e2_eb1c_432d, "0.0001"),
            (0x430c_6bf5_2633_ffff, "999999999999999.9"),
            (0x430c_6bf5_2634_0000, "1000000000000000.0"),
            (0x4341_c379_37e0_8000, "1e+16"),
            (0x4415_af1d_78b5_8c40, "1e+20"),
            (0x3ff3_c0ca_428c_59fb, "1.2345678901234567"),
            (0x7ff0_0000_0000_0000, "inf"),
            (0xfff0_0000_0000_0000, "-inf"),
            (0x7ff8_0000_0000_0000, "nan"),
        ];
        for (bits, expected) in cases {
            assert_eq!(
                python_float_repr(f64::from_bits(bits)),
                expected,
                "{bits:#018x}"
            );
        }
    }
}
