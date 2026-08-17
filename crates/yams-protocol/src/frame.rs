use std::{
    fmt,
    io::{self, Read, Write},
};

use crate::{
    ProtocolError,
    json::{decode_request, decode_response, encode},
    message::{MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Message},
};

/// Bytes in the unsigned big-endian frame-length prefix.
pub const FRAME_HEADER_BYTES: usize = 4;

/// A blocking writer whose current timeout can be borrowed for one frame.
///
/// Implementations must report and replace the timeout used by [`Write::write`].
/// Deadline-aware helpers restore the reported value before returning.
pub trait DeadlineWrite: Write {
    /// Return the caller-owned write timeout.
    fn write_timeout(&self) -> io::Result<Option<std::time::Duration>>;

    /// Replace the write timeout used by subsequent writes.
    fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()>;
}

#[cfg(unix)]
impl DeadlineWrite for std::os::unix::net::UnixStream {
    fn write_timeout(&self) -> io::Result<Option<std::time::Duration>> {
        std::os::unix::net::UnixStream::write_timeout(self)
    }

    fn set_write_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        std::os::unix::net::UnixStream::set_write_timeout(self, timeout)
    }
}

#[cfg(unix)]
trait DeadlineRead: Read {
    fn read_timeout(&self) -> io::Result<Option<std::time::Duration>>;

    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()>;
}

#[cfg(unix)]
impl DeadlineRead for std::os::unix::net::UnixStream {
    fn read_timeout(&self) -> io::Result<Option<std::time::Duration>> {
        std::os::unix::net::UnixStream::read_timeout(self)
    }

    fn set_read_timeout(&self, timeout: Option<std::time::Duration>) -> io::Result<()> {
        std::os::unix::net::UnixStream::set_read_timeout(self, timeout)
    }
}

/// Incrementally accumulates exactly one bounded length-prefixed frame.
///
/// Event loops should read at most [`FrameReader::want`] bytes. [`FrameReader::feed`]
/// also rejects an input slice containing bytes beyond the declared frame so
/// bytes belonging to the next protocol stage are never silently consumed.
pub struct FrameReader {
    limit: usize,
    header: [u8; FRAME_HEADER_BYTES],
    header_len: usize,
    declared: Option<usize>,
    body: Vec<u8>,
}

impl FrameReader {
    /// Construct an empty reader with the supplied body-size limit.
    #[must_use]
    pub fn new(limit: usize) -> Self {
        Self {
            limit,
            header: [0; FRAME_HEADER_BYTES],
            header_len: 0,
            declared: None,
            body: Vec::new(),
        }
    }

    /// Return the maximum useful byte count for the next read.
    ///
    /// This is the remaining header length before declaration and the exact
    /// remaining body length afterward. It returns zero once complete.
    #[must_use]
    pub fn want(&self) -> usize {
        match self.declared {
            Some(declared) => declared.saturating_sub(self.body.len()),
            None => FRAME_HEADER_BYTES - self.header_len,
        }
    }

    /// Add bytes read from the peer and report whether the frame is complete.
    ///
    /// An empty slice represents EOF and is `truncated frame` while incomplete.
    /// A slice may contain an exact whole frame, but any byte beyond that frame
    /// is rejected without committing the slice.
    pub fn feed(&mut self, input: &[u8]) -> Result<bool, ProtocolError> {
        if self.frame().is_some() {
            return Err(ProtocolError::FrameAlreadyComplete);
        }
        if input.is_empty() {
            return Err(ProtocolError::TruncatedFrame);
        }

        if let Some(declared) = self.declared {
            if input.len() > declared - self.body.len() {
                return Err(ProtocolError::FrameInputTooLong);
            }
            self.body.extend_from_slice(input);
            return Ok(self.body.len() == declared);
        }

        let header_needed = FRAME_HEADER_BYTES - self.header_len;
        if input.len() < header_needed {
            self.header[self.header_len..self.header_len + input.len()].copy_from_slice(input);
            self.header_len += input.len();
            return Ok(false);
        }

        let mut complete_header = self.header;
        complete_header[self.header_len..].copy_from_slice(&input[..header_needed]);
        let declared = u32::from_be_bytes(complete_header) as usize;
        if declared > self.limit {
            return Err(ProtocolError::FrameTooLarge {
                declared,
                limit: self.limit,
            });
        }

        let body = &input[header_needed..];
        if body.len() > declared {
            return Err(ProtocolError::FrameInputTooLong);
        }

        self.header = complete_header;
        self.header_len = FRAME_HEADER_BYTES;
        self.declared = Some(declared);
        self.body.reserve(declared);
        self.body.extend_from_slice(body);
        Ok(self.body.len() == declared)
    }

    /// Borrow the completed body, or return `None` while incomplete.
    #[must_use]
    pub fn frame(&self) -> Option<&[u8]> {
        self.declared
            .filter(|declared| self.body.len() == *declared)
            .map(|_| self.body.as_slice())
    }
}

impl fmt::Debug for FrameReader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FrameReader")
            .field("limit", &self.limit)
            .field("header_len", &self.header_len)
            .field("declared", &self.declared)
            .field("body", &"<redacted>")
            .finish()
    }
}

/// Read exactly one four-byte-prefixed frame from a blocking reader.
///
/// The header is validated against `limit` before body allocation or reading.
/// Interrupted reads are retried and every premature EOF is `truncated frame`.
pub fn read_frame<R>(reader: &mut R, limit: usize) -> Result<Vec<u8>, ProtocolError>
where
    R: Read + ?Sized,
{
    let mut header = [0_u8; FRAME_HEADER_BYTES];
    read_exact(reader, &mut header)?;
    let declared = u32::from_be_bytes(header) as usize;
    if declared > limit {
        return Err(ProtocolError::FrameTooLarge { declared, limit });
    }

    let mut body = vec![0_u8; declared];
    read_exact(reader, &mut body)?;
    Ok(body)
}

/// Write one four-byte-prefixed frame to a blocking writer.
///
/// The body is checked before any byte is written. Partial and interrupted
/// writes are retried until the header and body are complete.
pub fn write_frame<W>(writer: &mut W, body: &[u8], limit: usize) -> Result<(), ProtocolError>
where
    W: Write + ?Sized,
{
    validate_frame_size(body, limit)?;

    write_all(writer, &(body.len() as u32).to_be_bytes())?;
    write_all(writer, body)
}

/// Write one frame within a single absolute deadline.
///
/// The deadline is shared by the header and every partial body write. The
/// writer's original timeout is restored on success and failure whenever this
/// function shortened it.
pub fn write_frame_with_deadline<W>(
    writer: &mut W,
    body: &[u8],
    limit: usize,
    deadline: std::time::Instant,
) -> Result<(), ProtocolError>
where
    W: DeadlineWrite + ?Sized,
{
    use std::time::Instant;

    validate_frame_size(body, limit)?;
    if deadline <= Instant::now() {
        return Err(ProtocolError::FrameDeadlineExceeded);
    }

    let original = writer
        .write_timeout()
        .map_err(|error| ProtocolError::Io(error.kind()))?;
    let mut current = original;
    let mut changed = false;
    let result = (|| {
        write_all_with_deadline(
            writer,
            &(body.len() as u32).to_be_bytes(),
            deadline,
            original,
            &mut current,
            &mut changed,
        )?;
        write_all_with_deadline(writer, body, deadline, original, &mut current, &mut changed)
    })();

    let restore = if changed {
        writer
            .set_write_timeout(original)
            .map_err(|error| ProtocolError::Io(error.kind()))
    } else {
        Ok(())
    };
    match (result, restore) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(error),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// Validate, encode, and write one message with its direction-specific limit.
pub fn send_message<W>(writer: &mut W, message: &Message) -> Result<(), ProtocolError>
where
    W: Write + ?Sized,
{
    let body = encode(message)?;
    let limit = match message {
        Message::Request(_) => MAX_REQUEST_BYTES,
        Message::Accepted(_) | Message::Completed(_) | Message::Rejected(_) => MAX_RESPONSE_BYTES,
    };
    write_frame(writer, &body, limit)
}

/// Validate, encode, and write one message within one absolute write deadline.
///
/// The message's request or response limit is selected before delegating to
/// [`write_frame_with_deadline`].
pub fn send_message_with_deadline<W>(
    writer: &mut W,
    message: &Message,
    deadline: std::time::Instant,
) -> Result<(), ProtocolError>
where
    W: DeadlineWrite + ?Sized,
{
    let body = encode(message)?;
    let limit = match message {
        Message::Request(_) => MAX_REQUEST_BYTES,
        Message::Accepted(_) | Message::Completed(_) | Message::Rejected(_) => MAX_RESPONSE_BYTES,
    };
    write_frame_with_deadline(writer, &body, limit, deadline)
}

#[cfg(unix)]
/// Send one request and receive its handshake response within one absolute deadline.
///
/// The stream enters nonblocking mode before any request byte is delivered, so
/// the write and response read share the same poll-driven deadline without
/// changing socket timeouts after the peer can close. A successfully decoded
/// response restores blocking mode with no read timeout, ready for an unbounded
/// completion read after `Accepted`.
///
/// After an error, callers must drop the stream rather than reuse it: blocking
/// mode is restored best-effort, but cleared timeouts are intentionally not
/// reinstated after request delivery may have occurred.
pub fn exchange_request_with_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    request: &crate::Request,
    deadline: std::time::Instant,
) -> Result<Message, ProtocolError> {
    let mut frame = Vec::new();
    send_message(&mut frame, &Message::Request(request.clone()))?;
    if deadline <= std::time::Instant::now() {
        return Err(ProtocolError::FrameDeadlineExceeded);
    }

    stream
        .set_read_timeout(None)
        .map_err(|error| ProtocolError::Io(error.kind()))?;
    stream
        .set_write_timeout(None)
        .map_err(|error| ProtocolError::Io(error.kind()))?;
    stream
        .set_nonblocking(true)
        .map_err(|error| ProtocolError::Io(error.kind()))?;

    let result = (|| {
        write_all_nonblocking_with_deadline(stream, &frame, deadline)?;
        let body = match read_frame_nonblocking_with_deadline(stream, MAX_RESPONSE_BYTES, deadline)
        {
            Err(ProtocolError::FrameDeadlineExceeded) => {
                return Err(ProtocolError::ResponseDeadlineAfterRequestDelivery);
            }
            result => result?,
        };
        decode_response(&body)
    })();
    let restore = stream
        .set_nonblocking(false)
        .map_err(|error| ProtocolError::Io(error.kind()));
    match (result, restore) {
        (Err(error), _) => Err(error),
        (Ok(_), Err(error)) => Err(error),
        (Ok(message), Ok(())) => Ok(message),
    }
}

/// Read and strictly decode one bounded request frame.
pub fn receive_request<R>(reader: &mut R) -> Result<Message, ProtocolError>
where
    R: Read + ?Sized,
{
    decode_request(&read_frame(reader, MAX_REQUEST_BYTES)?)
}

/// Read and strictly decode one bounded response frame.
pub fn receive_response<R>(reader: &mut R) -> Result<Message, ProtocolError>
where
    R: Read + ?Sized,
{
    decode_response(&read_frame(reader, MAX_RESPONSE_BYTES)?)
}

fn read_exact<R>(reader: &mut R, output: &mut [u8]) -> Result<(), ProtocolError>
where
    R: Read + ?Sized,
{
    let mut filled = 0;
    while filled < output.len() {
        match reader.read(&mut output[filled..]) {
            Ok(0) => return Err(ProtocolError::TruncatedFrame),
            Ok(count) => filled += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ProtocolError::Io(error.kind())),
        }
    }
    Ok(())
}

fn write_all<W>(writer: &mut W, input: &[u8]) -> Result<(), ProtocolError>
where
    W: Write + ?Sized,
{
    let mut written = 0;
    while written < input.len() {
        match writer.write(&input[written..]) {
            Ok(0) => return Err(ProtocolError::Io(io::ErrorKind::WriteZero)),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(ProtocolError::Io(error.kind())),
        }
    }
    Ok(())
}

fn write_all_with_deadline<W>(
    writer: &mut W,
    input: &[u8],
    deadline: std::time::Instant,
    original: Option<std::time::Duration>,
    current: &mut Option<std::time::Duration>,
    changed: &mut bool,
) -> Result<(), ProtocolError>
where
    W: DeadlineWrite + ?Sized,
{
    use std::{io::ErrorKind, time::Instant};

    let mut written = 0;
    while written < input.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ProtocolError::FrameDeadlineExceeded)?;
        // Some Unix implementations reject a zero-duration socket timeout;
        // keep the syscall timeout representable while the shared Instant
        // remains the authoritative deadline checked on every iteration.
        let timeout = timeout_for_deadline(remaining, original);
        if *current != Some(timeout) {
            writer
                .set_write_timeout(Some(timeout))
                .map_err(|error| ProtocolError::Io(error.kind()))?;
            *current = Some(timeout);
            *changed = true;
        }

        match writer.write(&input[written..]) {
            Ok(0) => return Err(ProtocolError::Io(io::ErrorKind::WriteZero)),
            Ok(count) => {
                written += count;
                if Instant::now() >= deadline {
                    return Err(ProtocolError::FrameDeadlineExceeded);
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if Instant::now() >= deadline || original.is_none_or(|caller| timeout < caller) {
                    return Err(ProtocolError::FrameDeadlineExceeded);
                }
                return Err(ProtocolError::Io(error.kind()));
            }
            Err(error) => return Err(ProtocolError::Io(error.kind())),
        }
    }
    Ok(())
}

fn timeout_for_deadline(
    remaining: std::time::Duration,
    original: Option<std::time::Duration>,
) -> std::time::Duration {
    let granularity = std::time::Duration::from_millis(1);
    let clamped = remaining.max(granularity);
    match original {
        Some(caller)
            if remaining >= granularity
                && caller > clamped
                && caller.saturating_sub(clamped) < granularity =>
        {
            caller
        }
        Some(caller) => caller.min(clamped),
        None => clamped,
    }
}

fn validate_frame_size(body: &[u8], limit: usize) -> Result<(), ProtocolError> {
    if body.len() > limit || body.len() > u32::MAX as usize {
        Err(ProtocolError::FrameTooLarge {
            declared: body.len(),
            limit: limit.min(u32::MAX as usize),
        })
    } else {
        Ok(())
    }
}

#[cfg(unix)]
/// Read one frame from a Unix stream within a single absolute deadline.
///
/// The deadline is shared across header and body reads. The stream's original
/// read timeout is restored before this function returns if it was shortened.
pub fn read_frame_with_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    limit: usize,
    deadline: std::time::Instant,
) -> Result<Vec<u8>, ProtocolError> {
    read_frame_with_deadline_from(stream, limit, deadline)
}

#[cfg(unix)]
fn read_frame_with_deadline_from<R>(
    reader: &mut R,
    limit: usize,
    deadline: std::time::Instant,
) -> Result<Vec<u8>, ProtocolError>
where
    R: DeadlineRead + ?Sized,
{
    use std::time::Instant;

    if deadline <= Instant::now() {
        return Err(ProtocolError::FrameDeadlineExceeded);
    }

    let original = reader
        .read_timeout()
        .map_err(|error| ProtocolError::Io(error.kind()))?;
    let mut current = original;
    let mut changed = false;

    let result = (|| {
        let mut header = [0_u8; FRAME_HEADER_BYTES];
        read_exact_with_deadline(
            reader,
            &mut header,
            deadline,
            original,
            &mut current,
            &mut changed,
        )?;
        let declared = u32::from_be_bytes(header) as usize;
        if declared > limit {
            return Err(ProtocolError::FrameTooLarge { declared, limit });
        }

        let mut body = vec![0_u8; declared];
        read_exact_with_deadline(
            reader,
            &mut body,
            deadline,
            original,
            &mut current,
            &mut changed,
        )?;
        Ok(body)
    })();

    match result {
        Err(error) => {
            if changed {
                let _ = reader.set_read_timeout(original);
            }
            Err(error)
        }
        Ok(body) => {
            if changed {
                reader
                    .set_read_timeout(original)
                    .map_err(|error| ProtocolError::Io(error.kind()))?;
            }
            Ok(body)
        }
    }
}

#[cfg(unix)]
/// Read and strictly decode one request from a Unix stream by an absolute deadline.
pub fn receive_request_with_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    deadline: std::time::Instant,
) -> Result<Message, ProtocolError> {
    decode_request(&read_frame_with_deadline(
        stream,
        MAX_REQUEST_BYTES,
        deadline,
    )?)
}

#[cfg(unix)]
/// Read and strictly decode one response from a Unix stream by an absolute deadline.
pub fn receive_response_with_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    deadline: std::time::Instant,
) -> Result<Message, ProtocolError> {
    decode_response(&read_frame_with_deadline(
        stream,
        MAX_RESPONSE_BYTES,
        deadline,
    )?)
}

#[cfg(unix)]
fn read_exact_with_deadline<R>(
    reader: &mut R,
    output: &mut [u8],
    deadline: std::time::Instant,
    original: Option<std::time::Duration>,
    current: &mut Option<std::time::Duration>,
    changed: &mut bool,
) -> Result<(), ProtocolError>
where
    R: DeadlineRead + ?Sized,
{
    use std::{io::ErrorKind, time::Instant};

    let mut filled = 0;
    while filled < output.len() {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ProtocolError::FrameDeadlineExceeded)?;
        let timeout = read_timeout_for_deadline(remaining, original);
        if *current != Some(timeout) {
            reader
                .set_read_timeout(Some(timeout))
                .map_err(|error| ProtocolError::Io(error.kind()))?;
            *current = Some(timeout);
            *changed = true;
        }

        match reader.read(&mut output[filled..]) {
            Ok(0) => return Err(ProtocolError::TruncatedFrame),
            Ok(count) => {
                filled += count;
                if Instant::now() >= deadline {
                    return Err(ProtocolError::FrameDeadlineExceeded);
                }
            }
            Err(error) if error.kind() == ErrorKind::Interrupted => {}
            Err(error) if matches!(error.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock) => {
                if Instant::now() >= deadline || original.is_none_or(|caller| timeout < caller) {
                    return Err(ProtocolError::FrameDeadlineExceeded);
                }
                return Err(ProtocolError::Io(error.kind()));
            }
            Err(error) => return Err(ProtocolError::Io(error.kind())),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn write_all_nonblocking_with_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    input: &[u8],
    deadline: std::time::Instant,
) -> Result<(), ProtocolError> {
    let mut written = 0;
    while written < input.len() {
        if std::time::Instant::now() >= deadline {
            return Err(ProtocolError::FrameDeadlineExceeded);
        }
        match stream.write(&input[written..]) {
            Ok(0) => return Err(ProtocolError::Io(io::ErrorKind::WriteZero)),
            Ok(count) => written += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                poll_until(stream, rustix::event::PollFlags::OUT, deadline)?;
            }
            Err(error) => return Err(ProtocolError::Io(error.kind())),
        }
    }
    if std::time::Instant::now() >= deadline {
        return Err(ProtocolError::FrameDeadlineExceeded);
    }
    Ok(())
}

#[cfg(unix)]
fn read_frame_nonblocking_with_deadline(
    stream: &mut std::os::unix::net::UnixStream,
    limit: usize,
    deadline: std::time::Instant,
) -> Result<Vec<u8>, ProtocolError> {
    let mut reader = FrameReader::new(limit);
    let mut buffer = [0_u8; 8192];
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(ProtocolError::FrameDeadlineExceeded);
        }
        let wanted = reader.want().min(buffer.len());
        match stream.read(&mut buffer[..wanted]) {
            Ok(0) => return Err(ProtocolError::TruncatedFrame),
            Ok(count) => {
                if reader.feed(&buffer[..count])? {
                    if std::time::Instant::now() >= deadline {
                        return Err(ProtocolError::FrameDeadlineExceeded);
                    }
                    return Ok(reader.frame().expect("completed frame").to_vec());
                }
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                poll_until(stream, rustix::event::PollFlags::IN, deadline)?;
            }
            Err(error) => return Err(ProtocolError::Io(error.kind())),
        }
    }
}

#[cfg(unix)]
fn poll_until(
    stream: &std::os::unix::net::UnixStream,
    events: rustix::event::PollFlags,
    deadline: std::time::Instant,
) -> Result<(), ProtocolError> {
    loop {
        let remaining = deadline
            .checked_duration_since(std::time::Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(ProtocolError::FrameDeadlineExceeded)?;
        let timeout = rustix::event::Timespec {
            tv_sec: remaining.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: remaining.subsec_nanos().into(),
        };
        let mut descriptors = [rustix::event::PollFd::new(stream, events)];
        match rustix::event::poll(&mut descriptors, Some(&timeout)) {
            Ok(0) => return Err(ProtocolError::FrameDeadlineExceeded),
            Ok(_) => return Ok(()),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(ProtocolError::Io(io::Error::from(error).kind())),
        }
    }
}

#[cfg(unix)]
fn read_timeout_for_deadline(
    remaining: std::time::Duration,
    original: Option<std::time::Duration>,
) -> std::time::Duration {
    timeout_for_deadline(remaining, original)
}

#[cfg(all(test, unix))]
mod tests {
    use std::cell::Cell;
    use std::io::{self, Cursor, Read, Write};
    use std::time::Duration;

    use crate::ProtocolError;

    use super::{
        DeadlineRead, DeadlineWrite, read_frame_with_deadline_from, read_timeout_for_deadline,
        write_frame_with_deadline,
    };

    struct RestoreRejectingWriter {
        bytes: Vec<u8>,
        timeout: Cell<Option<Duration>>,
        timeout_changes: Cell<usize>,
    }

    impl RestoreRejectingWriter {
        fn new(timeout: Duration) -> Self {
            Self {
                bytes: Vec::new(),
                timeout: Cell::new(Some(timeout)),
                timeout_changes: Cell::new(0),
            }
        }
    }

    impl Write for RestoreRejectingWriter {
        fn write(&mut self, input: &[u8]) -> io::Result<usize> {
            self.bytes.extend_from_slice(input);
            Ok(input.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    impl DeadlineWrite for RestoreRejectingWriter {
        fn write_timeout(&self) -> io::Result<Option<Duration>> {
            Ok(self.timeout.get())
        }

        fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            if timeout == Some(Duration::from_secs(5)) && self.timeout.get() != timeout {
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            self.timeout.set(timeout);
            self.timeout_changes.set(self.timeout_changes.get() + 1);
            Ok(())
        }
    }

    struct RestoreRejectingReader {
        bytes: Cursor<Vec<u8>>,
        timeout: Cell<Option<Duration>>,
        restore_attempts: Cell<usize>,
    }

    impl RestoreRejectingReader {
        fn new() -> Self {
            let mut frame = Vec::from(1_u32.to_be_bytes());
            frame.push(b'x');
            Self {
                bytes: Cursor::new(frame),
                timeout: Cell::new(Some(Duration::from_secs(5))),
                restore_attempts: Cell::new(0),
            }
        }
    }

    impl Read for RestoreRejectingReader {
        fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
            self.bytes.read(output)
        }
    }

    impl DeadlineRead for RestoreRejectingReader {
        fn read_timeout(&self) -> io::Result<Option<Duration>> {
            Ok(self.timeout.get())
        }

        fn set_read_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
            if timeout == Some(Duration::from_secs(5)) && self.timeout.get() != timeout {
                self.restore_attempts.set(self.restore_attempts.get() + 1);
                return Err(io::Error::from(io::ErrorKind::InvalidInput));
            }
            self.timeout.set(timeout);
            Ok(())
        }
    }

    #[test]
    fn sub_millisecond_read_timeout_is_clamped_to_one_millisecond() {
        assert_eq!(
            read_timeout_for_deadline(Duration::from_micros(300), Some(Duration::from_secs(7))),
            Duration::from_millis(1)
        );
    }

    #[test]
    fn read_timeout_tolerance_has_an_exact_one_millisecond_boundary() {
        let caller = Duration::from_secs(5);
        let within_granularity = caller - Duration::from_micros(999);
        assert_eq!(
            read_timeout_for_deadline(within_granularity, Some(caller)),
            caller
        );

        let full_millisecond_shorter = caller - Duration::from_millis(1);
        assert_eq!(
            read_timeout_for_deadline(full_millisecond_shorter, Some(caller)),
            full_millisecond_shorter
        );
    }

    #[test]
    fn ordinary_deadline_read_reports_a_successful_frames_restore_failure() {
        let mut reader = RestoreRejectingReader::new();

        let result = read_frame_with_deadline_from(
            &mut reader,
            crate::MAX_RESPONSE_BYTES,
            std::time::Instant::now() + Duration::from_secs(4),
        );

        assert_eq!(result, Err(ProtocolError::Io(io::ErrorKind::InvalidInput)));
        assert_eq!(reader.restore_attempts.get(), 1);
    }

    #[test]
    fn write_timeout_tolerance_has_an_exact_one_millisecond_boundary() {
        let caller = Duration::from_secs(5);
        let within_granularity = caller - Duration::from_micros(999);
        assert_eq!(
            super::timeout_for_deadline(within_granularity, Some(caller)),
            caller
        );

        let full_millisecond_shorter = caller - Duration::from_millis(1);
        assert_eq!(
            super::timeout_for_deadline(full_millisecond_shorter, Some(caller)),
            full_millisecond_shorter
        );
    }

    #[test]
    fn caller_write_timeout_shorter_than_deadline_needs_no_restore() {
        let mut writer = RestoreRejectingWriter::new(Duration::from_secs(5));

        write_frame_with_deadline(
            &mut writer,
            b"x",
            1,
            std::time::Instant::now() + Duration::from_secs(60),
        )
        .unwrap();

        assert_eq!(writer.timeout_changes.get(), 0);
        assert_eq!(writer.bytes, [0, 0, 0, 1, b'x']);
    }
}
