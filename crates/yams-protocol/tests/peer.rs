use std::os::unix::net::UnixStream;

use yams_protocol::peer::{PeerError, validate_peer};

#[test]
fn peer_validation_accepts_the_current_process_peer() {
    let (left, right) = UnixStream::pair().unwrap();
    drop(right);
    validate_peer(&left).unwrap();
}

#[test]
fn peer_error_type_is_exposed_for_fail_closed_callers() {
    let error = PeerError::Mismatch {
        expected: 1,
        actual: 2,
    };
    assert!(error.to_string().contains("peer"));
}
