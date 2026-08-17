use std::fs::File;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode as ProcessExitCode;

use chrono::Local;
use clap::{Parser, Subcommand};
use rustix::fs::{self as rfs, FileType, Mode, OFlags};
use serde::de::DeserializeOwned;
use serde_json::json;
use yams_core::{ExitCode, MAX_FILE_BYTES, TerminalText, sanitize_terminal};
use yams_wiki::{
    ApplyExitClass, DurableError, InitError, InitInspection, InitMode, InitPlanRequest,
    ManifestEnvelope, ProjectPageRequest, ReindexOptions, apply_manifest_classified, capabilities,
    check_wiki, compat_wiki, inspect_repository, plan_repository, plan_request_from_inspection,
    reindex_wiki, write_json,
};

const JSON_INPUT_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::CLOEXEC)
    .union(OFlags::NONBLOCK)
    .union(OFlags::NOFOLLOW);

#[derive(Debug, Parser)]
#[command(name = "yams-wiki", version)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report stable machine-readable Yams contracts.
    Capabilities {
        /// Emit the capability response as one JSON line.
        #[arg(long, required = true)]
        json: bool,
    },
    /// Inspect, plan, or apply repository memory initialization.
    Init {
        #[command(subcommand)]
        command: InitCommand,
    },
    /// Validate the structural wiki contract.
    Check { path: PathBuf },
    /// Report constructs outside the supported Obsidian-compatible profile.
    Compat { path: PathBuf },
    /// Check or regenerate the derived catalog in INDEX.md.
    Catalog {
        path: PathBuf,
        /// Report whether regeneration is needed without writing.
        #[arg(long, conflicts_with = "adopt")]
        check: bool,
        /// Adopt the exact legacy index layout before regenerating.
        #[arg(long)]
        adopt: bool,
    },
    /// Read one JSON request from stdin and write a page transactionally.
    Write { path: PathBuf },
}

#[derive(Debug, Subcommand)]
enum InitCommand {
    /// Inspect repository memory without changing the target.
    Inspect {
        /// Emit the inspection as one JSON line.
        #[arg(long, required = true)]
        json: bool,
        root: PathBuf,
    },
    /// Build an immutable initialization manifest from a JSON request file.
    Plan {
        /// Complete plan request JSON. Conflicts with `--from-inspect`.
        #[arg(long, required_unless_present = "from_inspect")]
        request: Option<PathBuf>,
        /// Inspection JSON from `init inspect --json`. Binds root and digest.
        #[arg(long, required_unless_present = "request")]
        from_inspect: Option<PathBuf>,
        /// Initialization mode. Defaults to the inspection's recommended mode.
        #[arg(long, requires = "from_inspect")]
        mode: Option<String>,
        /// Calendar date for the project page. Defaults to the local date.
        #[arg(long, requires = "from_inspect")]
        date: Option<String>,
        /// Project-page JSON object. Required with `--from-inspect`.
        #[arg(long, requires = "from_inspect")]
        project_page: Option<PathBuf>,
        /// Optional exact AGENTS.md file. Omit to use the canonical policy path.
        #[arg(long, requires = "from_inspect")]
        agents_md: Option<PathBuf>,
    },
    /// Apply one approved initialization manifest file.
    Apply {
        #[arg(long)]
        manifest: PathBuf,
    },
}

fn main() -> ProcessExitCode {
    let cli = match Cli::try_parse() {
        Ok(cli) => cli,
        Err(error) => {
            let code = error.exit_code();
            let printed = if code == 0 {
                error.print()
            } else {
                write_diagnostic(&mut io::stderr().lock(), error)
            };
            if printed.is_err() || io::stdout().flush().is_err() || io::stderr().flush().is_err() {
                return checked_process_exit(i32::from(ExitCode::Operational));
            }
            return checked_process_exit(code);
        }
    };

    let mut stdout = io::stdout().lock();
    let mut stderr = io::stderr().lock();
    let result = execute(cli.command, &mut stdout, &mut stderr);
    let stdout_flushed = stdout.flush();
    let stderr_flushed = stderr.flush();
    let code = match (result, stdout_flushed, stderr_flushed) {
        (Ok(code), Ok(()), Ok(())) => code,
        _ => ExitCode::Operational,
    };
    checked_process_exit(i32::from(code))
}

fn execute(
    command: Command,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<ExitCode> {
    match command {
        Command::Capabilities { json: true } => run_capabilities(stdout),
        Command::Capabilities { json: false } => Ok(ExitCode::Usage),
        Command::Init { command } => run_init(command, stdout, stderr),
        Command::Check { path } => run_check(&path, stdout, stderr),
        Command::Compat { path } => run_compat(&path, stderr),
        Command::Catalog { path, check, adopt } => run_catalog(&path, check, adopt, stdout, stderr),
        Command::Write { path } => run_write(&path, stdout),
    }
}

fn run_init(
    command: InitCommand,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<ExitCode> {
    match command {
        InitCommand::Inspect { json: true, root } => run_init_inspect(&root, stdout, stderr),
        InitCommand::Inspect { json: false, .. } => Ok(ExitCode::Usage),
        InitCommand::Plan {
            request,
            from_inspect,
            mode,
            date,
            project_page,
            agents_md,
        } => run_init_plan(
            PlanArgs {
                request: request.as_deref(),
                from_inspect: from_inspect.as_deref(),
                mode: mode.as_deref(),
                date: date.as_deref(),
                project_page: project_page.as_deref(),
                agents_md: agents_md.as_deref(),
            },
            stdout,
            stderr,
        ),
        InitCommand::Apply { manifest } => run_init_apply(&manifest, stdout, stderr),
    }
}

fn run_init_inspect(
    root: &Path,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<ExitCode> {
    match inspect_repository(root) {
        Ok(inspection) => write_json_line(stdout, &inspection).map(|()| ExitCode::Ok),
        Err(error) => {
            write_diagnostic(stderr, error)?;
            Ok(ExitCode::Operational)
        }
    }
}

struct PlanArgs<'a> {
    request: Option<&'a Path>,
    from_inspect: Option<&'a Path>,
    mode: Option<&'a str>,
    date: Option<&'a str>,
    project_page: Option<&'a Path>,
    agents_md: Option<&'a Path>,
}

fn run_init_plan(
    args: PlanArgs<'_>,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<ExitCode> {
    let request = if let Some(request_path) = args.request {
        match read_bounded_json(request_path) {
            Ok(request) => request,
            Err(error) => {
                return finish_plan_input_error(
                    request_path,
                    "initialization request",
                    error,
                    stderr,
                );
            }
        }
    } else {
        match assemble_plan_request(&args, stderr)? {
            Some(request) => request,
            None => return Ok(ExitCode::Usage),
        }
    };
    match plan_repository(&request) {
        Ok(envelope) => write_json_line(stdout, &envelope).map(|()| ExitCode::Ok),
        Err(error) => {
            write_diagnostic(stderr, &error)?;
            Ok(init_error_exit(&error))
        }
    }
}

fn assemble_plan_request(
    args: &PlanArgs<'_>,
    stderr: &mut impl Write,
) -> io::Result<Option<InitPlanRequest>> {
    let Some(inspection_path) = args.from_inspect else {
        write_diagnostic(stderr, "init plan requires --request or --from-inspect")?;
        return Ok(None);
    };
    let Some(project_page_path) = args.project_page else {
        write_diagnostic(stderr, "init plan --from-inspect requires --project-page")?;
        return Ok(None);
    };
    let inspection: InitInspection = match read_bounded_json(inspection_path) {
        Ok(inspection) => inspection,
        Err(error) => {
            write_plan_input_error(inspection_path, "inspection", error, stderr)?;
            return Ok(None);
        }
    };
    let project_page: ProjectPageRequest = match read_bounded_json(project_page_path) {
        Ok(page) => page,
        Err(error) => {
            write_plan_input_error(project_page_path, "project page", error, stderr)?;
            return Ok(None);
        }
    };
    let agents_md = match args.agents_md {
        None => String::new(),
        Some(path) => match read_bounded_utf8(path) {
            Ok(text) => text,
            Err(error) => {
                write_plan_input_error(path, "AGENTS.md", error, stderr)?;
                return Ok(None);
            }
        },
    };
    let mode = match args.mode {
        None => None,
        Some(value) => match parse_init_mode(value) {
            Ok(mode) => Some(mode),
            Err(message) => {
                write_diagnostic(stderr, message)?;
                return Ok(None);
            }
        },
    };
    let date = args
        .date
        .map(str::to_owned)
        .unwrap_or_else(|| Local::now().date_naive().to_string());
    match plan_request_from_inspection(&inspection, mode, date, project_page, agents_md) {
        Ok(request) => Ok(Some(request)),
        Err(error) => {
            write_diagnostic(stderr, &error)?;
            Ok(None)
        }
    }
}

fn parse_init_mode(value: &str) -> Result<InitMode, String> {
    match value {
        "minimal" => Ok(InitMode::Minimal),
        "full" => Ok(InitMode::Full),
        other => Err(format!("invalid mode {other:?}; expected minimal or full")),
    }
}

fn finish_plan_input_error(
    path: &Path,
    label: &str,
    error: InputError,
    stderr: &mut impl Write,
) -> io::Result<ExitCode> {
    let exit = plan_input_exit(&error);
    write_plan_input_error(path, label, error, stderr)?;
    Ok(exit)
}

fn plan_input_exit(error: &InputError) -> ExitCode {
    match error {
        InputError::Read(_) | InputError::Unsafe(_) => ExitCode::Operational,
        InputError::TooLarge | InputError::Json(_) => ExitCode::Usage,
    }
}

fn write_plan_input_error(
    path: &Path,
    label: &str,
    error: InputError,
    stderr: &mut impl Write,
) -> io::Result<()> {
    let message = match error {
        InputError::Read(error) => format!("could not read {}: {error}", path.display()),
        InputError::Unsafe(reason) => format!("could not read {}: {reason}", path.display()),
        InputError::TooLarge => {
            format!("{label} exceeds MAX_FILE_BYTES ({MAX_FILE_BYTES} bytes)")
        }
        InputError::Json(error) => format!("invalid initialization JSON: {error}"),
    };
    write_diagnostic(stderr, message)
}

fn read_bounded_utf8(path: &Path) -> Result<String, InputError> {
    let bytes = read_bounded_bytes(path)?;
    String::from_utf8(bytes).map_err(|_| InputError::Unsafe("input is not valid UTF-8"))
}

fn run_init_apply(
    manifest_path: &Path,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<ExitCode> {
    let envelope: ManifestEnvelope = match read_bounded_json(manifest_path) {
        Ok(envelope) => envelope,
        Err(InputError::Read(error)) => {
            write_diagnostic(
                stderr,
                format!("could not read {}: {error}", manifest_path.display()),
            )?;
            return Ok(ExitCode::Operational);
        }
        Err(InputError::Unsafe(reason)) => {
            write_diagnostic(
                stderr,
                format!("could not read {}: {reason}", manifest_path.display()),
            )?;
            return Ok(ExitCode::Operational);
        }
        Err(InputError::TooLarge) => {
            write_diagnostic(
                stderr,
                format!("initialization manifest exceeds MAX_FILE_BYTES ({MAX_FILE_BYTES} bytes)"),
            )?;
            return Ok(ExitCode::Usage);
        }
        Err(InputError::Json(error)) => {
            write_diagnostic(stderr, format!("invalid initialization JSON: {error}"))?;
            return Ok(ExitCode::Usage);
        }
    };
    let outcome = apply_manifest_classified(&envelope);
    let exit = match outcome.class {
        ApplyExitClass::Success => ExitCode::Ok,
        ApplyExitClass::Usage => ExitCode::Usage,
        ApplyExitClass::Operational => ExitCode::Operational,
    };
    write_json_line(stdout, &outcome.result)?;
    Ok(exit)
}

fn write_json_line(stdout: &mut impl Write, value: &impl serde::Serialize) -> io::Result<()> {
    serde_json::to_writer(&mut *stdout, value).map_err(io::Error::other)?;
    stdout.write_all(b"\n")
}

enum InputError {
    Read(io::Error),
    Unsafe(&'static str),
    TooLarge,
    Json(serde_json::Error),
}

fn read_bounded_json<T: DeserializeOwned>(path: &Path) -> Result<T, InputError> {
    let bytes = read_bounded_bytes(path)?;
    serde_json::from_slice(&bytes).map_err(InputError::Json)
}

fn read_bounded_bytes(path: &Path) -> Result<Vec<u8>, InputError> {
    let descriptor = rfs::open(path, JSON_INPUT_FLAGS, Mode::empty())
        .map_err(|error| InputError::Read(io::Error::from_raw_os_error(error.raw_os_error())))?;
    let stat = rfs::fstat(&descriptor)
        .map_err(|error| InputError::Read(io::Error::from_raw_os_error(error.raw_os_error())))?;
    if !FileType::from_raw_mode(stat.st_mode).is_file() || stat.st_nlink != 1 {
        return Err(InputError::Unsafe(
            "input must be a single-link regular file, and symlinks are not followed",
        ));
    }
    if stat.st_size < 0 {
        return Err(InputError::Unsafe("input has an invalid negative size"));
    }
    if stat.st_size as u64 > MAX_FILE_BYTES {
        return Err(InputError::TooLarge);
    }
    let file = File::from(descriptor);
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(InputError::Read)?;
    if bytes.len() as u64 > MAX_FILE_BYTES {
        return Err(InputError::TooLarge);
    }
    Ok(bytes)
}

fn write_diagnostic(stderr: &mut impl Write, message: impl std::fmt::Display) -> io::Result<()> {
    let message = message.to_string();
    writeln!(
        stderr,
        "{}",
        sanitize_terminal(&message, TerminalText::Inline)
    )
}

fn init_error_exit(error: &InitError) -> ExitCode {
    match error {
        InitError::InvalidRequest(_)
        | InitError::Conflict(_)
        | InitError::Drift(_)
        | InitError::Json(_) => ExitCode::Usage,
        InitError::Io { .. }
        | InitError::Git(_)
        | InitError::InvalidRoot(_)
        | InitError::Candidate(_)
        | InitError::Apply(_) => ExitCode::Operational,
    }
}

fn run_capabilities(stdout: &mut impl Write) -> io::Result<ExitCode> {
    serde_json::to_writer(&mut *stdout, &capabilities()).map_err(io::Error::other)?;
    stdout.write_all(b"\n")?;
    Ok(ExitCode::Ok)
}

fn run_check(
    path: &std::path::Path,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<ExitCode> {
    let report = match check_wiki(path) {
        Ok(report) => report,
        Err(error) => {
            write_diagnostic(stderr, error)?;
            return Ok(ExitCode::Operational);
        }
    };
    for note in report.notes {
        writeln!(stdout, "{note}")?;
    }
    for failure in &report.failures {
        write_diagnostic(stderr, failure)?;
    }
    if report.failures.is_empty() {
        Ok(ExitCode::Ok)
    } else {
        Ok(ExitCode::Empty)
    }
}

fn run_compat(path: &std::path::Path, stderr: &mut impl Write) -> io::Result<ExitCode> {
    let report = match compat_wiki(path) {
        Ok(report) => report,
        Err(error) => {
            write_diagnostic(stderr, error)?;
            return Ok(ExitCode::Operational);
        }
    };
    for violation in &report.violations {
        write_diagnostic(stderr, violation)?;
    }
    if report.violations.is_empty() {
        Ok(ExitCode::Ok)
    } else {
        Ok(ExitCode::Empty)
    }
}

fn run_catalog(
    path: &std::path::Path,
    check: bool,
    adopt: bool,
    stdout: &mut impl Write,
    stderr: &mut impl Write,
) -> io::Result<ExitCode> {
    let options = ReindexOptions {
        check_only: check,
        adopt,
        ..ReindexOptions::default()
    };
    match reindex_wiki(path, &options) {
        Ok(result) => {
            if let Some(note) = result.isolation_note {
                writeln!(stdout, "{note}")?;
            }
            if check {
                if result.changed {
                    writeln!(stdout, "INDEX.md differs from what catalog would produce.")?;
                    Ok(ExitCode::Empty)
                } else {
                    writeln!(stdout, "INDEX.md is up to date.")?;
                    Ok(ExitCode::Ok)
                }
            } else {
                if result.changed {
                    writeln!(stdout, "INDEX.md rewritten.")?;
                } else {
                    writeln!(stdout, "INDEX.md unchanged.")?;
                }
                Ok(ExitCode::Ok)
            }
        }
        Err(error) if is_catalog_refusal(&error) => {
            write_diagnostic(stderr, format!("catalog refused: {error}"))?;
            Ok(ExitCode::Usage)
        }
        Err(error) => {
            write_diagnostic(stderr, error)?;
            Ok(ExitCode::Operational)
        }
    }
}

fn is_catalog_refusal(error: &DurableError) -> bool {
    matches!(
        error,
        DurableError::Index(_)
            | DurableError::ExpectedIndexChanged
            | DurableError::InvalidIndexUtf8(_)
            | DurableError::InvalidPageUtf8 { .. }
            | DurableError::InvalidPageName { .. }
    )
}

fn run_write(path: &std::path::Path, stdout: &mut impl Write) -> io::Result<ExitCode> {
    let mut input = Vec::new();
    let result = match io::stdin().take(MAX_FILE_BYTES + 1).read_to_end(&mut input) {
        Ok(_) => {
            let today = Local::now().date_naive().to_string();
            write_json(path, &input, &today)
        }
        Err(error) => yams_wiki::WriteResult {
            exit_code: ExitCode::Operational,
            body: json!({
                "ok": false,
                "exit": 4,
                "error": format!("could not read stdin: {error}"),
                "hint": "retry with one readable JSON object"
            }),
        },
    };
    serde_json::to_writer(&mut *stdout, &result.body).map_err(io::Error::other)?;
    stdout.write_all(b"\n")?;
    Ok(result.exit_code)
}

fn checked_process_exit(code: i32) -> ProcessExitCode {
    u8::try_from(code)
        .map(ProcessExitCode::from)
        .unwrap_or(ProcessExitCode::FAILURE)
}
