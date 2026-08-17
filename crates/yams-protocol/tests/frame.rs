use std::io::{self, Cursor, Read, Write};
use std::{cell::Cell, time::Duration};

use yams_protocol::{
    Accepted, DeadlineWrite, FrameReader, MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, Message,
    ProtocolError, Request, encode, read_frame, receive_request, receive_response, send_message,
    send_message_with_deadline, write_frame, write_frame_with_deadline,
};

fn frame(body: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + body.len());
    bytes.extend_from_slice(&(body.len() as u32).to_be_bytes());
    bytes.extend_from_slice(body);
    bytes
}

#[test]
fn incremental_reader_reassembles_a_frame_one_byte_at_a_time() {
    let expected = b"request body";
    let mut reader = FrameReader::new(MAX_REQUEST_BYTES);

    for byte in frame(expected) {
        reader.feed(&[byte]).unwrap();
    }

    assert_eq!(reader.frame(), Some(expected.as_slice()));
    assert_eq!(reader.want(), 0);
}

#[test]
fn incremental_reader_reports_header_and_body_wants() {
    let mut reader = FrameReader::new(MAX_REQUEST_BYTES);
    assert_eq!(reader.want(), 4);

    assert!(!reader.feed(&[0, 0]).unwrap());
    assert_eq!(reader.want(), 2);
    assert!(!reader.feed(&[0, 3]).unwrap());
    assert_eq!(reader.want(), 3);
    assert!(!reader.feed(b"ab").unwrap());
    assert_eq!(reader.want(), 1);
    assert!(reader.feed(b"c").unwrap());
    assert_eq!(reader.frame(), Some(b"abc".as_slice()));
}

#[test]
fn incremental_reader_accepts_one_exact_whole_frame_but_never_overreads() {
    let complete = frame(b"ok");
    let mut reader = FrameReader::new(2);
    assert!(reader.feed(&complete).unwrap());
    assert_eq!(reader.frame(), Some(b"ok".as_slice()));

    let mut reader = FrameReader::new(2);
    let mut extra = complete;
    extra.push(b'x');
    assert_eq!(reader.feed(&extra), Err(ProtocolError::FrameInputTooLong));
    assert_eq!(reader.want(), 4);
    assert_eq!(reader.frame(), None);
}

#[test]
fn incremental_reader_handles_empty_exact_max_truncated_and_oversize_frames() {
    let mut empty = FrameReader::new(MAX_REQUEST_BYTES);
    assert!(empty.feed(&0_u32.to_be_bytes()).unwrap());
    assert_eq!(empty.frame(), Some([].as_slice()));

    let mut exact = FrameReader::new(MAX_REQUEST_BYTES);
    assert!(
        !exact
            .feed(&(MAX_REQUEST_BYTES as u32).to_be_bytes())
            .unwrap()
    );
    assert!(exact.feed(&vec![b'x'; MAX_REQUEST_BYTES]).unwrap());

    let mut truncated = FrameReader::new(MAX_REQUEST_BYTES);
    truncated.feed(&16_u32.to_be_bytes()).unwrap();
    assert_eq!(truncated.feed(&[]), Err(ProtocolError::TruncatedFrame));

    let mut oversized = FrameReader::new(MAX_REQUEST_BYTES);
    assert_eq!(
        oversized.feed(&((MAX_REQUEST_BYTES + 1) as u32).to_be_bytes()),
        Err(ProtocolError::FrameTooLarge {
            declared: MAX_REQUEST_BYTES + 1,
            limit: MAX_REQUEST_BYTES,
        })
    );
}

#[test]
fn blocking_reader_handles_bytewise_and_fragmented_input() {
    let expected = b"fragmented";
    let mut input = OneByteReader::new(frame(expected));

    assert_eq!(read_frame(&mut input, MAX_REQUEST_BYTES).unwrap(), expected);
    assert!(input.interruptions > 0);
}

#[test]
fn blocking_reader_maps_every_early_eof_to_truncated_frame() {
    for payload in [vec![0, 0], frame(b"abc")[..6].to_vec()] {
        assert_eq!(
            read_frame(&mut Cursor::new(payload), MAX_REQUEST_BYTES),
            Err(ProtocolError::TruncatedFrame)
        );
    }
}

#[test]
fn blocking_reader_rejects_an_oversized_header_before_reading_a_body() {
    let mut input = HeaderThenPanic::new((MAX_REQUEST_BYTES + 1) as u32);

    assert_eq!(
        read_frame(&mut input, MAX_REQUEST_BYTES),
        Err(ProtocolError::FrameTooLarge {
            declared: MAX_REQUEST_BYTES + 1,
            limit: MAX_REQUEST_BYTES,
        })
    );
    assert_eq!(input.reads, 1);
}

#[test]
fn blocking_writer_retries_interruptions_and_partial_writes() {
    let mut output = PartialWriter::new(2, true, None);

    write_frame(&mut output, b"abcdef", MAX_REQUEST_BYTES).unwrap();

    assert_eq!(output.bytes, frame(b"abcdef"));
    assert!(output.writes > 3);
}

#[test]
fn blocking_writer_reports_broken_pipe_without_leaking_payload() {
    let mut output = BrokenPipeWriter::new(None);

    let error = write_frame(&mut output, b"secret body", MAX_REQUEST_BYTES).unwrap_err();

    assert_eq!(error, ProtocolError::Io(io::ErrorKind::BrokenPipe));
    assert!(!format!("{error:?} {error}").contains("secret body"));
}

#[test]
fn blocking_writer_accepts_the_exact_limit_and_rejects_limit_plus_one() {
    let mut exact = Vec::new();
    write_frame(
        &mut exact,
        &vec![b'x'; MAX_REQUEST_BYTES],
        MAX_REQUEST_BYTES,
    )
    .unwrap();
    assert_eq!(exact.len(), MAX_REQUEST_BYTES + 4);

    let mut untouched = Vec::new();
    assert_eq!(
        write_frame(
            &mut untouched,
            &vec![b'x'; MAX_REQUEST_BYTES + 1],
            MAX_REQUEST_BYTES,
        ),
        Err(ProtocolError::FrameTooLarge {
            declared: MAX_REQUEST_BYTES + 1,
            limit: MAX_REQUEST_BYTES,
        })
    );
    assert!(untouched.is_empty());
}

#[test]
fn response_framing_accepts_its_exact_limit_and_rejects_an_oversized_header() {
    let mut exact = Vec::new();
    write_frame(
        &mut exact,
        &vec![b'x'; MAX_RESPONSE_BYTES],
        MAX_RESPONSE_BYTES,
    )
    .unwrap();
    assert_eq!(exact.len(), MAX_RESPONSE_BYTES + 4);

    let mut input = HeaderThenPanic::new((MAX_RESPONSE_BYTES + 1) as u32);
    assert_eq!(
        read_frame(&mut input, MAX_RESPONSE_BYTES),
        Err(ProtocolError::FrameTooLarge {
            declared: MAX_RESPONSE_BYTES + 1,
            limit: MAX_RESPONSE_BYTES,
        })
    );
    assert_eq!(input.reads, 1);
}

#[test]
fn deadline_writer_shares_one_budget_across_partial_and_interrupted_writes() {
    let original = Some(Duration::from_secs(3));
    let mut output = PartialWriter::new(2, true, original);

    write_frame_with_deadline(
        &mut output,
        b"abcdef",
        MAX_REQUEST_BYTES,
        std::time::Instant::now() + Duration::from_secs(1),
    )
    .unwrap();

    assert_eq!(output.bytes, frame(b"abcdef"));
    assert!(output.writes > 3);
    assert_eq!(output.timeout.get(), original);
    assert!(output.timeout_changes.get() > 1);
}

#[test]
fn spent_write_deadline_never_reaches_the_writer_or_changes_its_timeout() {
    let original = Some(Duration::from_secs(3));
    let mut output = PartialWriter::new(2, false, original);

    let result = write_frame_with_deadline(
        &mut output,
        b"body",
        MAX_REQUEST_BYTES,
        std::time::Instant::now() - Duration::from_millis(1),
    );

    assert_eq!(result, Err(ProtocolError::FrameDeadlineExceeded));
    assert_eq!(output.writes, 0);
    assert_eq!(output.timeout.get(), original);
    assert_eq!(output.timeout_changes.get(), 0);
}

#[test]
fn deadline_writer_restores_timeout_after_broken_pipe() {
    let original = Some(Duration::from_secs(3));
    let mut output = BrokenPipeWriter::new(original);

    let result = write_frame_with_deadline(
        &mut output,
        b"body",
        MAX_REQUEST_BYTES,
        std::time::Instant::now() + Duration::from_secs(1),
    );

    assert_eq!(result, Err(ProtocolError::Io(io::ErrorKind::BrokenPipe)));
    assert_eq!(output.timeout.get(), original);
}

#[test]
fn deadline_message_helper_uses_the_messages_wire_limit() {
    let message = Message::Request(
        Request::from_argv(vec!["query".into()], String::from("/tmp"))
            .expect("service request is not --write"),
    );
    let mut output = PartialWriter::new(3, false, None);

    send_message_with_deadline(
        &mut output,
        &message,
        std::time::Instant::now() + Duration::from_secs(1),
    )
    .unwrap();

    assert_eq!(output.bytes, frame(&encode(&message).unwrap()));
    assert_eq!(output.timeout.get(), None);
}

#[test]
fn message_helpers_apply_the_right_frame_limit_and_decoder() {
    let request = Message::Request(
        Request::from_argv(vec!["query".into()], String::from("/tmp"))
            .expect("service request is not --write"),
    );
    let response = Message::Accepted(Accepted {
        request_id: "request-1".into(),
    });
    let mut request_frame = Vec::new();
    let mut response_frame = Vec::new();
    send_message(&mut request_frame, &request).unwrap();
    send_message(&mut response_frame, &response).unwrap();

    assert_eq!(
        receive_request(&mut Cursor::new(request_frame)).unwrap(),
        request
    );
    assert_eq!(
        receive_response(&mut Cursor::new(response_frame)).unwrap(),
        response
    );

    assert!(receive_request(&mut Cursor::new(0_u32.to_be_bytes())).is_err());
}

#[cfg(unix)]
mod deadline {
    use std::{
        io::Write,
        os::unix::net::UnixStream,
        thread,
        time::{Duration, Instant},
    };

    use yams_protocol::{
        Accepted, Completed, MAX_REQUEST_BYTES, Message, ProtocolError, Request,
        exchange_request_with_deadline, read_frame_with_deadline, receive_request,
        receive_request_with_deadline, receive_response, receive_response_with_deadline,
        send_message, write_frame_with_deadline,
    };

    use super::frame;

    #[test]
    fn spent_deadline_is_rejected_without_changing_the_timeout() {
        let (_left, mut right) = UnixStream::pair().unwrap();
        right
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        let result = read_frame_with_deadline(
            &mut right,
            MAX_REQUEST_BYTES,
            Instant::now() - Duration::from_millis(1),
        );

        assert_eq!(result, Err(ProtocolError::FrameDeadlineExceeded));
        assert_eq!(
            right.read_timeout().unwrap(),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn successful_deadline_read_restores_the_callers_timeout() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        left.write_all(&frame(b"ready")).unwrap();
        right
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        let body = read_frame_with_deadline(
            &mut right,
            MAX_REQUEST_BYTES,
            Instant::now() + Duration::from_millis(100),
        )
        .unwrap();

        assert_eq!(body, b"ready");
        assert_eq!(
            right.read_timeout().unwrap(),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn sub_millisecond_read_deadline_is_classified_and_restores_timeout() {
        let (_left, mut right) = UnixStream::pair().unwrap();
        let original = Some(Duration::from_secs(7));
        right.set_read_timeout(original).unwrap();

        let result =
            receive_response_with_deadline(&mut right, Instant::now() + Duration::from_micros(300));

        assert_eq!(result, Err(ProtocolError::FrameDeadlineExceeded));
        assert_eq!(right.read_timeout().unwrap(), original);
    }

    #[test]
    fn one_absolute_deadline_bounds_a_slow_drip_across_header_and_body() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        right
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let bytes = frame(b"slow body");
        let writer = thread::spawn(move || {
            for byte in bytes {
                if left.write_all(&[byte]).is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(12));
            }
        });
        let started = Instant::now();

        let result = read_frame_with_deadline(
            &mut right,
            MAX_REQUEST_BYTES,
            started + Duration::from_millis(55),
        );

        assert_eq!(result, Err(ProtocolError::FrameDeadlineExceeded));
        assert!(started.elapsed() < Duration::from_millis(300));
        assert_eq!(right.read_timeout().unwrap(), Some(Duration::from_secs(2)));
        drop(right);
        writer.join().unwrap();
    }

    #[test]
    fn bounded_request_helper_uses_the_same_deadline_and_restores_timeout() {
        let (mut left, mut right) = UnixStream::pair().unwrap();
        let expected = Message::Request(
            Request::from_argv(vec!["query".into()], String::from("/tmp"))
                .expect("service request is not --write"),
        );
        send_message(&mut left, &expected).unwrap();
        right
            .set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();

        let actual =
            receive_request_with_deadline(&mut right, Instant::now() + Duration::from_millis(100))
                .unwrap();

        assert_eq!(actual, expected);
        assert_eq!(
            right.read_timeout().unwrap(),
            Some(Duration::from_millis(500))
        );
    }

    #[test]
    fn nonblocking_exchange_preserves_buffered_completion_for_blocking_read() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let expected_request = Request::from_argv(vec!["query".into()], String::from("/tmp"))
            .expect("service request is not --write");
        let server = thread::spawn(move || {
            assert!(matches!(
                receive_request(&mut server).unwrap(),
                Message::Request(_)
            ));
            send_message(
                &mut server,
                &Message::Accepted(Accepted {
                    request_id: "request-id".into(),
                }),
            )
            .unwrap();
            send_message(
                &mut server,
                &Message::Completed(Completed {
                    request_id: "request-id".into(),
                    exit_code: 0,
                    stdout: "done\n".into(),
                    stderr: String::new(),
                }),
            )
            .unwrap();
        });

        let response = exchange_request_with_deadline(
            &mut client,
            &expected_request,
            Instant::now() + Duration::from_secs(1),
        )
        .unwrap();

        assert!(matches!(response, Message::Accepted(_)));
        assert_eq!(client.read_timeout().unwrap(), None);
        assert_eq!(client.write_timeout().unwrap(), None);
        assert!(matches!(
            receive_response(&mut client).unwrap(),
            Message::Completed(_)
        ));
        server.join().unwrap();
    }

    #[test]
    fn exchange_distinguishes_a_response_deadline_after_request_delivery() {
        let (mut client, mut server) = UnixStream::pair().unwrap();
        let expected_request = Request::from_argv(vec!["query".into()], String::from("/tmp"))
            .expect("service request is not --write");
        let delivered = thread::spawn(move || {
            assert!(matches!(
                receive_request(&mut server).unwrap(),
                Message::Request(_)
            ));
            thread::sleep(Duration::from_millis(100));
        });

        let result = exchange_request_with_deadline(
            &mut client,
            &expected_request,
            Instant::now() + Duration::from_millis(40),
        );

        assert_eq!(
            result,
            Err(ProtocolError::ResponseDeadlineAfterRequestDelivery)
        );
        delivered.join().unwrap();
    }

    #[test]
    fn absolute_write_deadline_stops_a_peer_that_never_drains() {
        let (mut left, _right) = UnixStream::pair().unwrap();
        left.set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let body = vec![b'x'; yams_protocol::MAX_RESPONSE_BYTES];
        let started = Instant::now();

        let result = write_frame_with_deadline(
            &mut left,
            &body,
            yams_protocol::MAX_RESPONSE_BYTES,
            started + Duration::from_millis(55),
        );

        assert_eq!(result, Err(ProtocolError::FrameDeadlineExceeded));
        assert!(started.elapsed() < Duration::from_millis(500));
        assert_eq!(left.write_timeout().unwrap(), Some(Duration::from_secs(2)));
    }
}

struct OneByteReader {
    bytes: Cursor<Vec<u8>>,
    interrupt_next: bool,
    interruptions: usize,
}

impl OneByteReader {
    fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Cursor::new(bytes),
            interrupt_next: true,
            interruptions: 0,
        }
    }
}

impl Read for OneByteReader {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        if self.interrupt_next {
            self.interrupt_next = false;
            self.interruptions += 1;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        self.interrupt_next = true;
        let count = output.len().min(1);
        self.bytes.read(&mut output[..count])
    }
}

struct HeaderThenPanic {
    header: Cursor<[u8; 4]>,
    reads: usize,
}

impl HeaderThenPanic {
    fn new(size: u32) -> Self {
        Self {
            header: Cursor::new(size.to_be_bytes()),
            reads: 0,
        }
    }
}

impl Read for HeaderThenPanic {
    fn read(&mut self, output: &mut [u8]) -> io::Result<usize> {
        self.reads += 1;
        assert_eq!(self.reads, 1, "oversized frame attempted a body read");
        self.header.read(output)
    }
}

struct PartialWriter {
    bytes: Vec<u8>,
    max: usize,
    interrupt_next: bool,
    writes: usize,
    timeout: Cell<Option<Duration>>,
    timeout_changes: Cell<usize>,
}

impl PartialWriter {
    fn new(max: usize, interrupt_first: bool, timeout: Option<Duration>) -> Self {
        Self {
            bytes: Vec::new(),
            max,
            interrupt_next: interrupt_first,
            writes: 0,
            timeout: Cell::new(timeout),
            timeout_changes: Cell::new(0),
        }
    }
}

impl Write for PartialWriter {
    fn write(&mut self, input: &[u8]) -> io::Result<usize> {
        self.writes += 1;
        if self.interrupt_next {
            self.interrupt_next = false;
            return Err(io::Error::from(io::ErrorKind::Interrupted));
        }
        let count = input.len().min(self.max);
        self.bytes.extend_from_slice(&input[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl DeadlineWrite for PartialWriter {
    fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.timeout.get())
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.timeout.set(timeout);
        self.timeout_changes.set(self.timeout_changes.get() + 1);
        Ok(())
    }
}

struct BrokenPipeWriter {
    timeout: Cell<Option<Duration>>,
}

impl BrokenPipeWriter {
    fn new(timeout: Option<Duration>) -> Self {
        Self {
            timeout: Cell::new(timeout),
        }
    }
}

impl Write for BrokenPipeWriter {
    fn write(&mut self, _input: &[u8]) -> io::Result<usize> {
        Err(io::Error::from(io::ErrorKind::BrokenPipe))
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl DeadlineWrite for BrokenPipeWriter {
    fn write_timeout(&self) -> io::Result<Option<Duration>> {
        Ok(self.timeout.get())
    }

    fn set_write_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.timeout.set(timeout);
        Ok(())
    }
}
