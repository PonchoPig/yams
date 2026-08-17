//! Private local service lifecycle and request execution primitives.

#![warn(missing_docs)]

mod peer;
mod service;
mod socket;

use std::ffi::OsString;
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::time::Duration;

pub use peer::{
    PeerCredentialProvider, PeerError, SystemPeerCredentials, validate_peer, validate_peer_with,
};
pub use service::{
    ExecutionOutput, LoopStats, MAX_ACTIVE_REQUESTS, MAX_PENDING_ADMISSIONS, MAX_STREAM_BYTES,
    REQUEST_FRAME_DEADLINE, ServiceError, ShutdownToken, connect, serve_once, serve_until,
};
pub use socket::{
    OwnedSocket, SocketError, SocketProvenance, bind_listener, bind_with_provenance,
    cleanup_owned_socket, computed_default_socket, prepare_default_runtime_dir,
};

/// Bind the service socket only after `prepare` succeeds.
///
/// Clients that connect while the service is still loading a model must see
/// absence, not a listener that cannot yet accept. Failures in `prepare` leave
/// no socket behind.
pub fn bind_after<T, E: ToString>(
    socket: &Path,
    provenance: SocketProvenance,
    prepare: impl FnOnce() -> Result<T, E>,
) -> Result<(UnixListener, OwnedSocket, T), String> {
    let prepared = prepare().map_err(|error| error.to_string())?;
    let (listener, owned) =
        bind_with_provenance(socket, provenance).map_err(|error| error.to_string())?;
    Ok((listener, owned, prepared))
}

/// Validate and parse the raw `--idle-timeout` value shared by both the
/// space-separated and `=`-attached argument forms.
fn parse_idle_seconds(raw: &str) -> Result<u64, String> {
    if raw.is_empty() {
        return Err("--idle-timeout requires a value".into());
    }
    let idle: u64 = raw
        .parse()
        .map_err(|_| "--idle-timeout must be a nonnegative integer".to_string())?;
    if idle == 0 {
        return Err("--idle-timeout must be greater than zero".into());
    }
    Ok(idle)
}

/// Parse `yams-service` arguments using the process temporary directory.
pub fn parse_service_args(
    arguments: Vec<OsString>,
) -> Result<(PathBuf, Duration, SocketProvenance), String> {
    parse_service_args_with_temp_dir(&arguments, &std::env::temp_dir())
}

/// Parse `yams-service` arguments with deterministic environment inputs.
///
/// This entry point is intended for callers that must provide a captured
/// environment rather than mutate process-global variables. A supplied
/// `TMPDIR` controls the computed default; otherwise the process temporary
/// directory is used.
pub fn parse_service_args_in(
    arguments: &[OsString],
    environment: &[(&str, &std::path::Path)],
) -> Result<(PathBuf, Duration, SocketProvenance), String> {
    let temporary_directory = environment
        .iter()
        .rev()
        .find_map(|(name, value)| (*name == "TMPDIR").then_some(*value))
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    parse_service_args_with_temp_dir(arguments, &temporary_directory)
}

fn parse_service_args_with_temp_dir(
    arguments: &[OsString],
    temporary_directory: &std::path::Path,
) -> Result<(PathBuf, Duration, SocketProvenance), String> {
    let mut socket = None;
    let mut idle = 1200_u64;
    let mut index = 0;
    while index < arguments.len() {
        match arguments[index].to_str() {
            Some("--help") | Some("-h") if arguments.len() == 1 => return Err("help".into()),
            Some("--version") => return Err("version".into()),
            Some("--socket") => {
                index += 1;
                socket = Some(PathBuf::from(
                    arguments.get(index).ok_or("--socket requires a value")?,
                ));
            }
            Some(value) if value.starts_with("--socket=") => {
                socket = Some(PathBuf::from(&value[9..]));
            }
            Some("--idle-timeout") => {
                index += 1;
                let raw = arguments
                    .get(index)
                    .ok_or("--idle-timeout requires a value")?
                    .to_str()
                    .ok_or("--idle-timeout must be UTF-8")?;
                idle = parse_idle_seconds(raw)?;
            }
            Some(value) if value.starts_with("--idle-timeout=") => {
                let raw = value
                    .strip_prefix("--idle-timeout=")
                    .expect("guarded by starts_with");
                idle = parse_idle_seconds(raw)?;
            }
            Some(value) => return Err(format!("unknown option {value}")),
            None => return Err("arguments must be UTF-8".into()),
        }
        index += 1;
    }
    let (socket, provenance) = match socket {
        Some(socket) => (socket, SocketProvenance::Explicit),
        None => {
            if !temporary_directory.is_absolute() {
                return Err("TMPDIR must be absolute".into());
            }
            let resolved_temporary_directory = std::fs::canonicalize(temporary_directory)
                .map_err(|_| "TMPDIR must resolve to an existing directory".to_string())?;
            if !resolved_temporary_directory.is_dir() {
                return Err("TMPDIR must resolve to a directory".into());
            }
            (
                computed_default_socket(&resolved_temporary_directory),
                SocketProvenance::ComputedDefault,
            )
        }
    };
    if !socket.is_absolute() {
        return Err("--socket must be absolute".into());
    }
    Ok((socket, Duration::from_secs(idle), provenance))
}
