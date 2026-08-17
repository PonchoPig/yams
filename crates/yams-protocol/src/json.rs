use std::{
    fmt,
    io::{self, Write},
    path::Path,
};

use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, MapAccess, Visitor},
};

use crate::message::{
    Accepted, Completed, MAX_ARGUMENT_BYTES, MAX_ARGUMENTS, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES,
    Message, OperationKind, PROTOCOL_VERSION, Rejected, Request, ServiceOperation,
};

const MAX_JSON_NESTING: usize = 128;
const DUPLICATE_SENTINEL: &str = "yams duplicate JSON field";
const UNKNOWN_SENTINEL: &str = "yams unknown JSON field";

macro_rules! read_once {
    ($map:expr, $slot:expr) => {{
        if $slot.is_some() {
            return Err(de::Error::custom(DUPLICATE_SENTINEL));
        }
        $slot = Some($map.next_value()?);
    }};
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// A safely printable protocol validation or transport failure.
pub enum ProtocolError {
    /// The body is not valid UTF-8.
    InvalidUtf8,
    /// The body is not valid bounded JSON for this protocol.
    InvalidJson,
    /// The JSON root is not an object.
    MessageMustBeObject,
    /// A root object key occurs more than once.
    DuplicateField,
    /// A root object key is not part of the protocol vocabulary.
    UnknownField,
    /// Present fields do not exactly match the declared message type.
    FieldsDoNotMatch,
    /// The version field is absent, ill-typed, or unsupported.
    UnsupportedVersion,
    /// A request decoder received a non-request type.
    InvalidRequestType,
    /// A response decoder received an unknown or request type.
    UnknownResponseType,
    /// A request working directory is not absolute.
    CwdNotAbsolute,
    /// An acknowledgement or completion has an empty request identifier.
    EmptyRequestId,
    /// A rejection has an empty machine-readable code.
    EmptyRejectionCode,
    /// A request contains more than [`MAX_ARGUMENTS`] arguments.
    TooManyArguments,
    /// One request argument exceeds [`MAX_ARGUMENT_BYTES`] UTF-8 bytes.
    ArgumentTooLarge,
    /// An encoded or declared frame body exceeds its direction's limit.
    FrameTooLarge {
        /// Encoded or header-declared body size.
        declared: usize,
        /// Applicable request or response body limit.
        limit: usize,
    },
    /// EOF occurred before the declared frame was complete.
    TruncatedFrame,
    /// One absolute frame I/O deadline expired.
    FrameDeadlineExceeded,
    /// The complete request was delivered, but its admission response missed the deadline.
    ResponseDeadlineAfterRequestDelivery,
    /// Bytes were fed after an incremental frame became complete.
    FrameAlreadyComplete,
    /// An incremental feed included bytes beyond the declared frame.
    FrameInputTooLong,
    /// Transport I/O failed; only the non-sensitive error kind is retained.
    Io(io::ErrorKind),
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUtf8 => formatter.write_str("invalid UTF-8"),
            Self::InvalidJson => formatter.write_str("invalid JSON"),
            Self::MessageMustBeObject => formatter.write_str("message must be a JSON object"),
            Self::DuplicateField => formatter.write_str("duplicate JSON field"),
            Self::UnknownField => formatter.write_str("unknown JSON field"),
            Self::FieldsDoNotMatch => formatter.write_str("message fields do not match its type"),
            Self::UnsupportedVersion => formatter.write_str("unsupported protocol version"),
            Self::InvalidRequestType => formatter.write_str("invalid request type"),
            Self::UnknownResponseType => formatter.write_str("unknown response type"),
            Self::CwdNotAbsolute => formatter.write_str("cwd must be an absolute path"),
            Self::EmptyRequestId => formatter.write_str("request_id must be a nonempty string"),
            Self::EmptyRejectionCode => {
                formatter.write_str("rejection code must be a nonempty string")
            }
            Self::TooManyArguments => formatter.write_str("too many arguments"),
            Self::ArgumentTooLarge => formatter.write_str("argument too large"),
            Self::FrameTooLarge { declared, limit } => {
                write!(formatter, "frame too large: {declared} > {limit}")
            }
            Self::TruncatedFrame => formatter.write_str("truncated frame"),
            Self::FrameDeadlineExceeded => formatter.write_str("frame deadline exceeded"),
            Self::ResponseDeadlineAfterRequestDelivery => {
                formatter.write_str("response deadline exceeded after request delivery")
            }
            Self::FrameAlreadyComplete => formatter.write_str("frame is already complete"),
            Self::FrameInputTooLong => formatter.write_str("frame input exceeds declared length"),
            Self::Io(kind) => write!(formatter, "protocol I/O failed ({kind:?})"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[derive(Default)]
struct RawMessage {
    version: Option<u8>,
    message_type: Option<String>,
    argv: Option<Vec<String>>,
    cwd: Option<String>,
    operation: Option<RawOperation>,
    request_id: Option<String>,
    exit_code: Option<u8>,
    stdout: Option<String>,
    stderr: Option<String>,
    code: Option<String>,
    message: Option<String>,
}

impl<'de> Deserialize<'de> for RawMessage {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(RawMessageVisitor)
    }
}

struct RawMessageVisitor;

impl<'de> Visitor<'de> for RawMessageVisitor {
    type Value = RawMessage;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a protocol message object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut raw = RawMessage::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "version" => read_once!(map, raw.version),
                "type" => read_once!(map, raw.message_type),
                "argv" => read_once!(map, raw.argv),
                "cwd" => read_once!(map, raw.cwd),
                "operation" => read_once!(map, raw.operation),
                "request_id" => read_once!(map, raw.request_id),
                "exit_code" => read_once!(map, raw.exit_code),
                "stdout" => read_once!(map, raw.stdout),
                "stderr" => read_once!(map, raw.stderr),
                "code" => read_once!(map, raw.code),
                "message" => read_once!(map, raw.message),
                _ => return Err(de::Error::custom(UNKNOWN_SENTINEL)),
            }
        }
        Ok(raw)
    }
}

#[derive(Default)]
struct RawOperation {
    kind: Option<String>,
    query: Option<String>,
    k: Option<String>,
    json: Option<bool>,
    full: Option<bool>,
    no_gate: Option<bool>,
    explain: Option<bool>,
    min_score: Option<String>,
    max_gap: Option<String>,
    project: Option<String>,
}

impl<'de> Deserialize<'de> for RawOperation {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_map(RawOperationVisitor)
    }
}

struct RawOperationVisitor;

impl<'de> Visitor<'de> for RawOperationVisitor {
    type Value = RawOperation;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a typed service operation object")
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut raw = RawOperation::default();
        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "kind" => read_once!(map, raw.kind),
                "query" => read_once!(map, raw.query),
                "k" => read_once!(map, raw.k),
                "json" => read_once!(map, raw.json),
                "full" => read_once!(map, raw.full),
                "no_gate" => read_once!(map, raw.no_gate),
                "explain" => read_once!(map, raw.explain),
                "min_score" => read_once!(map, raw.min_score),
                "max_gap" => read_once!(map, raw.max_gap),
                "project" => read_once!(map, raw.project),
                _ => return Err(de::Error::custom(UNKNOWN_SENTINEL)),
            }
        }
        Ok(raw)
    }
}

#[derive(Serialize)]
struct OperationWire<'a> {
    kind: &'a str,
    query: &'a str,
    k: &'a str,
    json: bool,
    full: bool,
    no_gate: bool,
    explain: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    min_score: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_gap: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    project: Option<&'a str>,
}

#[derive(Serialize)]
struct RequestWire<'a> {
    version: u8,
    #[serde(rename = "type")]
    message_type: &'static str,
    operation: OperationWire<'a>,
    argv: &'a [String],
    cwd: &'a str,
}

#[derive(Serialize)]
struct AcceptedWire<'a> {
    version: u8,
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: &'a str,
}

#[derive(Serialize)]
struct CompletedWire<'a> {
    version: u8,
    #[serde(rename = "type")]
    message_type: &'static str,
    request_id: &'a str,
    exit_code: u8,
    stdout: &'a str,
    stderr: &'a str,
}

#[derive(Serialize)]
struct RejectedWire<'a> {
    version: u8,
    #[serde(rename = "type")]
    message_type: &'static str,
    code: &'a str,
    message: &'a str,
}

/// Validate and encode one message as compact UTF-8 JSON without a frame header.
///
/// Serialization retains at most the applicable request or response limit even
/// when JSON escaping expands a caller-provided string beyond that limit.
pub fn encode(message: &Message) -> Result<Vec<u8>, ProtocolError> {
    match message {
        Message::Request(request) => {
            validate_request(request)?;
            encode_bounded(
                &RequestWire {
                    version: PROTOCOL_VERSION,
                    message_type: "request",
                    operation: OperationWire {
                        kind: request.operation.kind.as_str(),
                        query: &request.operation.query,
                        k: &request.operation.k,
                        json: request.operation.json,
                        full: request.operation.full,
                        no_gate: request.operation.no_gate,
                        explain: request.operation.explain,
                        min_score: request.operation.min_score.as_deref(),
                        max_gap: request.operation.max_gap.as_deref(),
                        project: request.operation.project.as_deref(),
                    },
                    argv: &request.argv,
                    cwd: &request.cwd,
                },
                MAX_REQUEST_BYTES,
            )
        }
        Message::Accepted(accepted) => {
            validate_request_id(&accepted.request_id)?;
            encode_bounded(
                &AcceptedWire {
                    version: PROTOCOL_VERSION,
                    message_type: "accepted",
                    request_id: &accepted.request_id,
                },
                MAX_RESPONSE_BYTES,
            )
        }
        Message::Completed(completed) => {
            validate_request_id(&completed.request_id)?;
            encode_bounded(
                &CompletedWire {
                    version: PROTOCOL_VERSION,
                    message_type: "completed",
                    request_id: &completed.request_id,
                    exit_code: completed.exit_code,
                    stdout: &completed.stdout,
                    stderr: &completed.stderr,
                },
                MAX_RESPONSE_BYTES,
            )
        }
        Message::Rejected(rejected) => {
            if rejected.code.is_empty() {
                return Err(ProtocolError::EmptyRejectionCode);
            }
            encode_bounded(
                &RejectedWire {
                    version: PROTOCOL_VERSION,
                    message_type: "rejected",
                    code: &rejected.code,
                    message: &rejected.message,
                },
                MAX_RESPONSE_BYTES,
            )
        }
    }
}

fn encode_bounded<T>(value: &T, limit: usize) -> Result<Vec<u8>, ProtocolError>
where
    T: Serialize,
{
    let mut writer = BoundedJsonWriter::new(limit);
    serde_json::to_writer(&mut writer, value).map_err(|_| ProtocolError::InvalidJson)?;
    if writer.written() > limit {
        return Err(ProtocolError::FrameTooLarge {
            declared: writer.written(),
            limit,
        });
    }
    Ok(writer.into_bytes())
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    written: usize,
    limit: usize,
}

impl BoundedJsonWriter {
    fn new(limit: usize) -> Self {
        Self {
            bytes: Vec::new(),
            written: 0,
            limit,
        }
    }

    fn written(&self) -> usize {
        self.written
    }

    fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.written = self.written.saturating_add(input.len());
        let retained = (self.limit - self.bytes.len()).min(input.len());
        self.bytes.extend_from_slice(&input[..retained]);
        Ok(input.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Decode one unframed request body with exact fields and strict bounds.
pub fn decode_request(body: &[u8]) -> Result<Message, ProtocolError> {
    let raw = decode_object(body, MAX_REQUEST_BYTES)?;
    validate_version(&raw)?;
    if raw.message_type.as_deref() != Some("request") {
        return Err(ProtocolError::InvalidRequestType);
    }
    if raw.argv.is_none()
        || raw.cwd.is_none()
        || raw.operation.is_none()
        || raw.request_id.is_some()
        || raw.exit_code.is_some()
        || raw.stdout.is_some()
        || raw.stderr.is_some()
        || raw.code.is_some()
        || raw.message.is_some()
    {
        return Err(ProtocolError::FieldsDoNotMatch);
    }

    let request = Request {
        operation: decode_operation(raw.operation.expect("checked"))?,
        argv: raw.argv.expect("checked"),
        cwd: raw.cwd.expect("checked"),
    };
    validate_request(&request)?;
    Ok(Message::Request(request))
}

/// Decode one unframed response body with exact fields and strict bounds.
pub fn decode_response(body: &[u8]) -> Result<Message, ProtocolError> {
    let raw = decode_object(body, MAX_RESPONSE_BYTES)?;
    validate_version(&raw)?;
    match raw.message_type.as_deref() {
        Some("accepted") => {
            if raw.request_id.is_none() || has_nonaccepted_fields(&raw) {
                return Err(ProtocolError::FieldsDoNotMatch);
            }
            let request_id = raw.request_id.expect("checked");
            validate_request_id(&request_id)?;
            Ok(Message::Accepted(Accepted { request_id }))
        }
        Some("completed") => {
            if raw.request_id.is_none()
                || raw.exit_code.is_none()
                || raw.stdout.is_none()
                || raw.stderr.is_none()
                || raw.argv.is_some()
                || raw.cwd.is_some()
                || raw.operation.is_some()
                || raw.code.is_some()
                || raw.message.is_some()
            {
                return Err(ProtocolError::FieldsDoNotMatch);
            }
            let request_id = raw.request_id.expect("checked");
            validate_request_id(&request_id)?;
            Ok(Message::Completed(Completed {
                request_id,
                exit_code: raw.exit_code.expect("checked"),
                stdout: raw.stdout.expect("checked"),
                stderr: raw.stderr.expect("checked"),
            }))
        }
        Some("rejected") => {
            if raw.code.is_none()
                || raw.message.is_none()
                || raw.argv.is_some()
                || raw.cwd.is_some()
                || raw.operation.is_some()
                || raw.request_id.is_some()
                || raw.exit_code.is_some()
                || raw.stdout.is_some()
                || raw.stderr.is_some()
            {
                return Err(ProtocolError::FieldsDoNotMatch);
            }
            let code = raw.code.expect("checked");
            if code.is_empty() {
                return Err(ProtocolError::EmptyRejectionCode);
            }
            Ok(Message::Rejected(Rejected {
                code,
                message: raw.message.expect("checked"),
            }))
        }
        Some(_) => Err(ProtocolError::UnknownResponseType),
        None => Err(ProtocolError::FieldsDoNotMatch),
    }
}

fn decode_object(body: &[u8], limit: usize) -> Result<RawMessage, ProtocolError> {
    if body.len() > limit {
        return Err(ProtocolError::FrameTooLarge {
            declared: body.len(),
            limit,
        });
    }
    let text = std::str::from_utf8(body).map_err(|_| ProtocolError::InvalidUtf8)?;
    validate_nesting(text.as_bytes())?;

    let first = text.bytes().find(|byte| !byte.is_ascii_whitespace());
    if first.is_some_and(|byte| byte != b'{') {
        return Err(ProtocolError::MessageMustBeObject);
    }

    let mut deserializer = serde_json::Deserializer::from_str(text);
    let raw = RawMessage::deserialize(&mut deserializer).map_err(classify_json_error)?;
    deserializer.end().map_err(classify_json_error)?;
    Ok(raw)
}

fn classify_json_error(error: serde_json::Error) -> ProtocolError {
    let rendered = error.to_string();
    if rendered.starts_with(DUPLICATE_SENTINEL) {
        ProtocolError::DuplicateField
    } else if rendered.starts_with(UNKNOWN_SENTINEL) {
        ProtocolError::UnknownField
    } else {
        ProtocolError::InvalidJson
    }
}

fn validate_nesting(body: &[u8]) -> Result<(), ProtocolError> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in body {
        if in_string {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                in_string = false;
            }
            continue;
        }
        match *byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth = depth.checked_add(1).ok_or(ProtocolError::InvalidJson)?;
                if depth > MAX_JSON_NESTING {
                    return Err(ProtocolError::InvalidJson);
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    Ok(())
}

fn validate_version(raw: &RawMessage) -> Result<(), ProtocolError> {
    match raw.version {
        Some(PROTOCOL_VERSION) if raw.message_type.is_some() => Ok(()),
        Some(PROTOCOL_VERSION) => Err(ProtocolError::FieldsDoNotMatch),
        _ => Err(ProtocolError::UnsupportedVersion),
    }
}

fn validate_request(request: &Request) -> Result<(), ProtocolError> {
    if request.argv.len() > MAX_ARGUMENTS {
        return Err(ProtocolError::TooManyArguments);
    }
    if request
        .argv
        .iter()
        .any(|argument| argument.len() > MAX_ARGUMENT_BYTES)
    {
        return Err(ProtocolError::ArgumentTooLarge);
    }
    if !Path::new(&request.cwd).is_absolute() {
        return Err(ProtocolError::CwdNotAbsolute);
    }
    Ok(())
}

fn validate_request_id(request_id: &str) -> Result<(), ProtocolError> {
    if request_id.is_empty() {
        Err(ProtocolError::EmptyRequestId)
    } else {
        Ok(())
    }
}

fn has_nonaccepted_fields(raw: &RawMessage) -> bool {
    raw.argv.is_some()
        || raw.cwd.is_some()
        || raw.operation.is_some()
        || raw.exit_code.is_some()
        || raw.stdout.is_some()
        || raw.stderr.is_some()
        || raw.code.is_some()
        || raw.message.is_some()
}

fn decode_operation(raw: RawOperation) -> Result<ServiceOperation, ProtocolError> {
    if raw.kind.is_none()
        || raw.query.is_none()
        || raw.k.is_none()
        || raw.json.is_none()
        || raw.full.is_none()
        || raw.no_gate.is_none()
        || raw.explain.is_none()
    {
        return Err(ProtocolError::FieldsDoNotMatch);
    }
    let kind = OperationKind::parse(raw.kind.as_deref().expect("checked"))
        .ok_or(ProtocolError::FieldsDoNotMatch)?;
    let query = raw.query.expect("checked");
    if query.len() > MAX_ARGUMENT_BYTES {
        return Err(ProtocolError::ArgumentTooLarge);
    }
    Ok(ServiceOperation {
        kind,
        query,
        k: raw.k.expect("checked"),
        json: raw.json.expect("checked"),
        full: raw.full.expect("checked"),
        no_gate: raw.no_gate.expect("checked"),
        explain: raw.explain.expect("checked"),
        min_score: raw.min_score,
        max_gap: raw.max_gap,
        project: raw.project,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Write;

    use super::BoundedJsonWriter;

    #[test]
    fn bounded_writer_counts_every_byte_but_never_retains_more_than_its_limit() {
        let mut writer = BoundedJsonWriter::new(4);

        writer.write_all(b"abcdef").unwrap();

        assert_eq!(writer.written(), 6);
        assert_eq!(writer.into_bytes(), b"abcd");
    }
}
