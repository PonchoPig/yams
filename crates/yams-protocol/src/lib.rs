//! Strict, bounded wire protocol shared by the Yams client and service.

#![warn(missing_docs)]

mod frame;
mod json;
mod message;
pub mod peer;

pub use frame::{
    DeadlineWrite, FRAME_HEADER_BYTES, FrameReader, read_frame, receive_request, receive_response,
    send_message, send_message_with_deadline, write_frame, write_frame_with_deadline,
};
#[cfg(unix)]
pub use frame::{
    exchange_request_with_deadline, read_frame_with_deadline, receive_request_with_deadline,
    receive_response_with_deadline,
};
pub use json::{ProtocolError, decode_request, decode_response, encode};
pub use message::{
    ADMISSION_TIMEOUT, Accepted, COMPLETION_TIMEOUT, Completed, MAX_ARGUMENT_BYTES, MAX_ARGUMENTS,
    MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Message, OperationKind, PROTOCOL_VERSION, Rejected,
    Request, ServiceOperation,
};
