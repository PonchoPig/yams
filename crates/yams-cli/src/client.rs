use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::net::{AddressFamily, SocketAddrUnix, SocketType};
use yams_protocol::peer::{PeerCredentialProvider, SystemPeerCredentials, validate_peer_with};
use yams_protocol::{
    ADMISSION_TIMEOUT, Accepted, COMPLETION_TIMEOUT, Completed, FrameReader, MAX_RESPONSE_BYTES,
    Message, OperationKind, ProtocolError, Rejected, Request, ServiceOperation, decode_response,
    exchange_request_with_deadline,
};

use crate::{DirectCompletion, DirectOperation, DirectRequest, RuntimeLayout};

const HANDSHAKE_TIMEOUT: Duration = ADMISSION_TIMEOUT;

/// Injectable bounded connector used by the service-client state machine.
pub trait Connector {
    /// Attempt one connection, completing no later than `deadline`.
    fn connect(&self, path: &Path, deadline: Instant) -> ConnectOutcome;
}

/// Result of trying to establish a service connection.
pub enum ConnectOutcome {
    /// A connected stream; direct execution is no longer safe on later errors.
    Connected(UnixStream),
    /// No service accepted the connection before the bound; direct execution is safe.
    Absent,
    /// Connection setup failed in a way that must remain operational.
    Failed(io::Error),
}

/// Production connector using a nonblocking Unix socket and an absolute poll bound.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemConnector;

impl Connector for SystemConnector {
    fn connect(&self, path: &Path, deadline: Instant) -> ConnectOutcome {
        connect_system(path, deadline)
    }
}

/// Try the selected service socket. `None` authorizes direct execution because
/// no request could have been delivered.
pub fn try_service(
    request: &DirectRequest,
    layout: &RuntimeLayout,
) -> Option<Result<DirectCompletion, DirectCompletion>> {
    try_service_with(request, layout, &SystemConnector, &SystemPeerCredentials)
}

/// Execute the staged client state machine with injected connection and peer
/// credential providers.
pub fn try_service_with<C, P>(
    request: &DirectRequest,
    layout: &RuntimeLayout,
    connector: &C,
    peer: &P,
) -> Option<Result<DirectCompletion, DirectCompletion>>
where
    C: Connector,
    P: PeerCredentialProvider,
{
    if request
        .query
        .as_ref()
        .is_some_and(|query| query.len() > yams_protocol::MAX_ARGUMENT_BYTES)
    {
        return None;
    }

    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    let mut stream = match connector.connect(&layout.service_socket, deadline) {
        ConnectOutcome::Connected(stream) => stream,
        ConnectOutcome::Absent => return None,
        ConnectOutcome::Failed(error) => {
            return operational(format!("service connection failed: {error}"));
        }
    };
    if let Err(error) = validate_peer_with(&stream, peer) {
        return operational(format!("service peer rejected: {error}"));
    }
    let operation = match service_operation(request) {
        Some(operation) => operation,
        None => return operational("service request could not be encoded"),
    };
    let argv = match request_argv(request) {
        Some(argv) => argv,
        None => return operational("service request could not be encoded"),
    };
    let cwd = match layout.cwd.to_str() {
        Some(cwd) => cwd.to_owned(),
        None => return operational("working directory is not valid UTF-8"),
    };
    let request = Request {
        operation,
        argv,
        cwd,
    };
    let accepted = match exchange_request_with_deadline(&mut stream, &request, deadline) {
        Ok(Message::Accepted(Accepted { request_id })) => request_id,
        Ok(Message::Rejected(Rejected { code, message })) => {
            return operational(format!("service rejected request ({code}): {message}"));
        }
        Ok(_) => return operational("service did not accept request"),
        Err(ProtocolError::FrameDeadlineExceeded) => return None,
        Err(error) => return operational(format!("service acceptance failed: {error}")),
    };
    match receive_completion(&mut stream, Instant::now() + COMPLETION_TIMEOUT) {
        Ok(Message::Completed(Completed {
            request_id: response_id,
            exit_code,
            stdout,
            stderr,
        })) if response_id == accepted => Some(Ok(DirectCompletion {
            exit_code: exit_code_from_service(exit_code),
            stdout,
            stderr,
        })),
        Ok(Message::Completed(_)) => operational("service completion request ID did not match"),
        Ok(_) => operational("service sent an invalid completion"),
        Err(error) => operational(format!("service completion failed: {error}")),
    }
}

fn receive_completion(
    stream: &mut UnixStream,
    deadline: Instant,
) -> Result<Message, ProtocolError> {
    use std::io::Read;

    stream
        .set_nonblocking(true)
        .map_err(|error| ProtocolError::Io(error.kind()))?;
    let mut reader = FrameReader::new(MAX_RESPONSE_BYTES);
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        if let Some(frame) = reader.frame() {
            let message = decode_response(frame);
            let _ = stream.set_nonblocking(false);
            return message;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let _ = stream.set_nonblocking(false);
            return Err(ProtocolError::FrameDeadlineExceeded);
        }
        let timeout = Timespec {
            tv_sec: remaining.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: remaining.subsec_nanos().into(),
        };
        let mut descriptors = [PollFd::new(&*stream, PollFlags::IN)];
        match poll(&mut descriptors, Some(&timeout)) {
            Ok(0) => {
                let _ = stream.set_nonblocking(false);
                return Err(ProtocolError::FrameDeadlineExceeded);
            }
            Ok(_) => match stream.read(&mut buffer) {
                Ok(0) => {
                    let _ = stream.set_nonblocking(false);
                    return Err(ProtocolError::TruncatedFrame);
                }
                Ok(count) => {
                    if let Err(error) = reader.feed(&buffer[..count]) {
                        let _ = stream.set_nonblocking(false);
                        return Err(error);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    let _ = stream.set_nonblocking(false);
                    return Err(ProtocolError::Io(error.kind()));
                }
            },
            Err(Errno::INTR) => {}
            Err(error) => {
                let _ = stream.set_nonblocking(false);
                return Err(ProtocolError::Io(io::Error::from(error).kind()));
            }
        }
    }
}

fn operational(message: impl Into<String>) -> Option<Result<DirectCompletion, DirectCompletion>> {
    Some(Err(DirectCompletion::operational(message.into())))
}

fn exit_code_from_service(exit_code: u8) -> yams_core::ExitCode {
    match exit_code {
        0 => yams_core::ExitCode::Ok,
        1 => yams_core::ExitCode::Empty,
        2 => yams_core::ExitCode::Usage,
        3 => yams_core::ExitCode::Unsure,
        _ => yams_core::ExitCode::Operational,
    }
}

fn connect_system(path: &Path, deadline: Instant) -> ConnectOutcome {
    if Instant::now() >= deadline {
        return ConnectOutcome::Absent;
    }
    let address = match SocketAddrUnix::new(path) {
        Ok(address) => address,
        Err(error) => return ConnectOutcome::Failed(error.into()),
    };
    let socket = match rustix::net::socket(AddressFamily::UNIX, SocketType::STREAM, None) {
        Ok(socket) => socket,
        Err(error) => return ConnectOutcome::Failed(error.into()),
    };
    if let Err(error) = rustix::io::ioctl_fionbio(&socket, true) {
        return ConnectOutcome::Failed(error.into());
    }
    match rustix::net::connect(&socket, &address) {
        Ok(()) => finish_connected(socket, deadline),
        Err(error) if is_absent_errno(error) => ConnectOutcome::Absent,
        Err(Errno::INPROGRESS | Errno::WOULDBLOCK) => poll_connected(socket, deadline),
        Err(error) => ConnectOutcome::Failed(error.into()),
    }
}

fn poll_connected(socket: rustix::fd::OwnedFd, deadline: Instant) -> ConnectOutcome {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return ConnectOutcome::Absent;
        }
        let timeout = Timespec {
            tv_sec: remaining.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: remaining.subsec_nanos().into(),
        };
        let mut descriptors = [PollFd::new(&socket, PollFlags::OUT)];
        match poll(&mut descriptors, Some(&timeout)) {
            Ok(0) => return ConnectOutcome::Absent,
            Ok(_) => match classify_socket_error(rustix::net::sockopt::socket_error(&socket)) {
                SocketErrorStatus::Ready => return finish_connected(socket, deadline),
                SocketErrorStatus::Absent => return ConnectOutcome::Absent,
                SocketErrorStatus::Failed(error) => {
                    return ConnectOutcome::Failed(error.into());
                }
            },
            Err(Errno::INTR) => {}
            Err(error) => return ConnectOutcome::Failed(error.into()),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SocketErrorStatus {
    Ready,
    Absent,
    Failed(Errno),
}

fn classify_socket_error(result: rustix::io::Result<Result<(), Errno>>) -> SocketErrorStatus {
    match result {
        Ok(Ok(())) => SocketErrorStatus::Ready,
        Ok(Err(error)) if is_absent_errno(error) => SocketErrorStatus::Absent,
        Ok(Err(error)) | Err(error) => SocketErrorStatus::Failed(error),
    }
}

fn finish_connected(socket: rustix::fd::OwnedFd, deadline: Instant) -> ConnectOutcome {
    if Instant::now() >= deadline {
        return ConnectOutcome::Absent;
    }
    if let Err(error) = rustix::io::ioctl_fionbio(&socket, false) {
        return ConnectOutcome::Failed(error.into());
    }
    if Instant::now() >= deadline {
        return ConnectOutcome::Absent;
    }
    ConnectOutcome::Connected(UnixStream::from(socket))
}

fn is_absent_errno(error: Errno) -> bool {
    matches!(error, Errno::NOENT | Errno::CONNREFUSED | Errno::TIMEDOUT)
}

fn service_operation(request: &DirectRequest) -> Option<ServiceOperation> {
    let kind = match request.operation {
        DirectOperation::Search => OperationKind::Search,
        DirectOperation::All => OperationKind::All,
        DirectOperation::Index => OperationKind::Index,
        DirectOperation::Stats => OperationKind::Stats,
        DirectOperation::Projects => OperationKind::Projects,
        DirectOperation::Gc => OperationKind::Gc,
        DirectOperation::Write => return None,
    };
    Some(ServiceOperation {
        kind,
        query: request.query.clone().unwrap_or_default(),
        k: request.k.to_string(),
        json: request.json,
        full: request.full,
        no_gate: request.no_gate,
        explain: request.explain,
        min_score: request.min_score.map(|value| value.to_string()),
        max_gap: request.max_gap.map(|value| value.to_string()),
        project: match request.project.as_ref() {
            Some(path) => Some(path.to_str()?.to_owned()),
            None => None,
        },
    })
}

fn request_argv(request: &DirectRequest) -> Option<Vec<String>> {
    let mut argv = Vec::new();
    match request.operation {
        DirectOperation::Search => {}
        DirectOperation::All => argv.push("--all".into()),
        DirectOperation::Write => return None,
        DirectOperation::Index => argv.push("--index".into()),
        DirectOperation::Stats => argv.push("--stats".into()),
        DirectOperation::Projects => argv.push("--projects".into()),
        DirectOperation::Gc => argv.push("--gc".into()),
    }
    if request.json {
        argv.push("--json".into());
    }
    if request.full {
        argv.push("--full".into());
    }
    if request.no_gate {
        argv.push("--no-gate".into());
    }
    if request.explain {
        argv.push("--explain".into());
    }
    if request.k != 5 {
        argv.extend(["-k".into(), request.requested_k.clone()]);
    }
    if let Some(value) = request.min_score {
        argv.extend(["--min-score".into(), value.to_string()]);
    }
    if let Some(value) = request.max_gap {
        argv.extend(["--max-gap".into(), value.to_string()]);
    }
    if let Some(project) = &request.project {
        argv.extend(["--project".into(), project.to_str()?.to_owned()]);
    }
    if let Some(query) = &request.query {
        argv.push("--".into());
        argv.push(query.clone());
    }
    Some(argv)
}

#[cfg(test)]
mod tests {
    use super::{SocketErrorStatus, classify_socket_error};
    use rustix::io::Errno;

    #[test]
    fn socket_error_classification_preserves_absence_and_failure() {
        assert_eq!(classify_socket_error(Ok(Ok(()))), SocketErrorStatus::Ready);
        for error in [Errno::NOENT, Errno::CONNREFUSED, Errno::TIMEDOUT] {
            assert_eq!(
                classify_socket_error(Ok(Err(error))),
                SocketErrorStatus::Absent
            );
        }
        assert_eq!(
            classify_socket_error(Ok(Err(Errno::ACCESS))),
            SocketErrorStatus::Failed(Errno::ACCESS)
        );
        assert_eq!(
            classify_socket_error(Err(Errno::BADF)),
            SocketErrorStatus::Failed(Errno::BADF)
        );
    }
}
