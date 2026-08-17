//! Shared execution primitives used by both `yams` and `yams-service`.

#![warn(missing_docs)]

use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use thiserror::Error;
use yams_protocol::peer::{SystemPeerCredentials, validate_peer_with};
use yams_protocol::{
    Accepted, COMPLETION_TIMEOUT, Completed, FrameReader, MAX_RESPONSE_BYTES, Message,
    ProtocolError, Rejected, Request, decode_response, exchange_request_with_deadline,
};

/// Failures from a completed service exchange.
#[derive(Debug, Error)]
pub enum IpcError {
    /// Transport failed.
    #[error("service I/O failed: {0}")]
    Io(#[from] io::Error),
    /// Protocol framing or decode failed.
    #[error("{0}")]
    Protocol(#[from] ProtocolError),
    /// Peer credentials were rejected.
    #[error("service peer rejected: {0}")]
    Peer(#[from] yams_protocol::peer::PeerError),
    /// The service refused the request before acceptance.
    #[error("service rejected request ({code}): {message}")]
    Rejected {
        /// Machine-readable refusal.
        code: String,
        /// Human-readable detail.
        message: String,
    },
    /// The first response was not an acceptance.
    #[error("service did not accept request")]
    NotAccepted,
    /// Completion request id did not match the acceptance.
    #[error("service completion request ID did not match")]
    RequestIdMismatch,
}

/// Connect, admit, and wait for one bounded completion.
pub fn connect(path: &Path, request: Request, timeout: Duration) -> Result<Completed, IpcError> {
    let mut stream = UnixStream::connect(path)?;
    validate_peer_with(&stream, &SystemPeerCredentials)?;
    let response = exchange_request_with_deadline(&mut stream, &request, Instant::now() + timeout)?;
    let request_id = match response {
        Message::Accepted(Accepted { request_id }) => request_id,
        Message::Rejected(Rejected { code, message }) => {
            return Err(IpcError::Rejected { code, message });
        }
        _ => return Err(IpcError::NotAccepted),
    };
    match receive_completion(&mut stream, Instant::now() + COMPLETION_TIMEOUT)? {
        Message::Completed(completed) if completed.request_id == request_id => Ok(completed),
        Message::Completed(_) => Err(IpcError::RequestIdMismatch),
        _ => Err(IpcError::NotAccepted),
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
