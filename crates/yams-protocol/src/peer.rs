//! Fail-closed local peer credential validation.

use std::io;
use std::os::unix::net::UnixStream;

use thiserror::Error;

/// Errors encountered while deciding whether a local peer may use this process.
#[derive(Debug, Error)]
pub enum PeerError {
    /// The kernel could not provide a peer identity, so admission is refused.
    #[error("peer credentials are unavailable: {0}")]
    Unavailable(io::Error),
    /// The peer belongs to a different effective user.
    #[error("peer uid {actual} does not match expected uid {expected}")]
    Mismatch {
        /// Effective user ID required by this process.
        expected: u32,
        /// Effective user ID reported by the operating system.
        actual: u32,
    },
    /// A credential lookup failed for an unexpected reason.
    #[error("peer credential lookup failed: {0}")]
    Io(io::Error),
}

/// A source of peer effective user IDs, injectable for deterministic tests.
pub trait PeerCredentialProvider {
    /// Return the peer effective user ID, or a fail-closed lookup error.
    fn peer_uid(&self, stream: &UnixStream) -> Result<u32, PeerError>;
}

/// The operating-system peer credential provider.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemPeerCredentials;

impl PeerCredentialProvider for SystemPeerCredentials {
    fn peer_uid(&self, stream: &UnixStream) -> Result<u32, PeerError> {
        #[cfg(target_os = "linux")]
        {
            rustix::net::socket_peercred(stream)
                .map(|credentials| credentials.uid.as_raw() as u32)
                .map_err(|error| PeerError::Io(error.into()))
        }
        #[cfg(not(target_os = "linux"))]
        {
            #[cfg(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd"
            ))]
            {
                nix::unistd::getpeereid(stream)
                    .map(|(uid, _)| uid.as_raw())
                    .map_err(|error| PeerError::Io(error.into()))
            }
            #[cfg(not(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "netbsd"
            )))]
            {
                let _ = stream;
                Err(PeerError::Unavailable(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "peer credential lookup is unavailable on this target",
                )))
            }
        }
    }
}

/// Validate a stream's peer against this process's effective user.
pub fn validate_peer(stream: &UnixStream) -> Result<(), PeerError> {
    validate_peer_with(stream, &SystemPeerCredentials)
}

/// Validate a stream with an injected credential provider.
pub fn validate_peer_with<P: PeerCredentialProvider>(
    stream: &UnixStream,
    provider: &P,
) -> Result<(), PeerError> {
    let expected = rustix::process::geteuid().as_raw() as u32;
    let actual = provider.peer_uid(stream)?;
    if actual == expected {
        Ok(())
    } else {
        Err(PeerError::Mismatch { expected, actual })
    }
}
