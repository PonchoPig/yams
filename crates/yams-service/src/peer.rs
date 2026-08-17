//! Re-exports peer credential validation shared via `yams-protocol`.

pub use yams_protocol::peer::{
    PeerCredentialProvider, PeerError, SystemPeerCredentials, validate_peer, validate_peer_with,
};
