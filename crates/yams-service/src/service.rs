//! One-request bounded service exchange and model-free client.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::os::fd::OwnedFd;
use std::os::unix::net::{UnixListener, UnixStream};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::io::Errno;
use rustix::pipe::pipe;
use thiserror::Error;
use yams_protocol::{
    ADMISSION_TIMEOUT, Accepted, Completed, FrameReader, MAX_REQUEST_BYTES, Message, ProtocolError,
    Rejected, Request, decode_request, receive_request_with_deadline, send_message_with_deadline,
};

use crate::peer::{PeerError, validate_peer};

/// Maximum UTF-8 bytes retained for each output stream (oracle parity: independent limits).
pub const MAX_STREAM_BYTES: usize = 4 * 1024 * 1024;
/// Maximum number of request handlers executing concurrently.
pub const MAX_ACTIVE_REQUESTS: usize = 8;
/// Maximum number of validated connections still delivering a request frame.
pub const MAX_PENDING_ADMISSIONS: usize = 64;
/// Absolute time allowed from connection acceptance to a complete request frame.
pub const REQUEST_FRAME_DEADLINE: Duration = ADMISSION_TIMEOUT;

/// Cooperative stop token whose closure is linearized against execution admission.
#[derive(Clone, Default)]
pub struct ShutdownToken(Arc<Mutex<ShutdownState>>);

#[derive(Default)]
struct ShutdownState {
    admission_closed: bool,
}

struct ExecutionReservation;

impl ShutdownToken {
    /// Construct a clear stop token.
    pub fn new() -> Self {
        Self::default()
    }

    /// Close execution admission and request worker drain.
    ///
    /// Execution reservations acquired before this call may run and drain;
    /// none can be acquired after this call returns.
    pub fn request(&self) {
        self.state().admission_closed = true;
    }

    fn requested(&self) -> bool {
        self.state().admission_closed
    }

    fn try_reserve_execution(&self) -> Option<ExecutionReservation> {
        let state = self.state();
        if state.admission_closed {
            None
        } else {
            Some(ExecutionReservation)
        }
    }

    fn state(&self) -> MutexGuard<'_, ShutdownState> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Bounded service-loop accounting returned after cooperative shutdown.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LoopStats {
    /// Connections accepted from the listener.
    pub accepted: usize,
    /// Connections rejected before execution.
    pub rejected: usize,
    /// Connections whose exchange completed (including protocol failures).
    pub completed: usize,
    /// Whether one or more workers exceeded the bounded drain period.
    pub workers_stuck: bool,
}

/// Owned output returned by a request handler.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionOutput {
    /// Process-compatible exit code.
    pub exit_code: u8,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

impl ExecutionOutput {
    /// Construct captured request output.
    pub fn new(exit_code: u8, stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            exit_code,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }
}

/// Errors from a bounded client/service exchange.
#[derive(Debug, Error)]
pub enum ServiceError {
    /// Filesystem or stream I/O failed.
    #[error("service I/O: {0}")]
    Io(#[from] io::Error),
    /// The protocol rejected the exchange.
    #[error("service protocol: {0}")]
    Protocol(#[from] ProtocolError),
    /// The local peer failed admission.
    #[error("service peer rejected: {0}")]
    Peer(#[from] PeerError),
    /// The service did not acknowledge the request as expected.
    #[error("service response was not accepted")]
    NotAccepted,
    /// The service explicitly refused the request; callers must not silently
    /// reroute this as a direct execution.
    #[error("service rejected request ({code}): {message}")]
    Rejected {
        /// Stable machine-readable refusal code.
        code: String,
        /// Human-readable refusal detail.
        message: String,
    },
    /// The completion did not match its acknowledgement.
    #[error("service completion request ID did not match")]
    RequestIdMismatch,
}

static NEXT_REQUEST_ID: AtomicU64 = AtomicU64::new(1);

/// Serve one accepted connection and one bounded request.
pub fn serve_once<H>(
    listener: &UnixListener,
    timeout: Duration,
    handler: H,
) -> Result<(), ServiceError>
where
    H: Fn(Request) -> ExecutionOutput,
{
    let (stream, _) = listener.accept()?;
    serve_stream(stream, timeout, handler)
}

fn serve_stream<H>(
    mut stream: UnixStream,
    timeout: Duration,
    handler: H,
) -> Result<(), ServiceError>
where
    H: Fn(Request) -> ExecutionOutput,
{
    // A nonblocking listener produces nonblocking accepted streams on some
    // Unix implementations; the bounded protocol reader owns its deadlines
    // and expects blocking stream syscalls.
    stream.set_nonblocking(false)?;
    validate_peer(&stream)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    let deadline = Instant::now() + timeout;
    let request = match receive_request_with_deadline(&mut stream, deadline) {
        Ok(Message::Request(request)) => request,
        Ok(_) => return Err(ServiceError::NotAccepted),
        Err(error) => {
            let _ = send_message_with_deadline(
                &mut stream,
                &Message::Rejected(Rejected {
                    code: "invalid_request".into(),
                    message: "request rejected".into(),
                }),
                Instant::now() + timeout,
            );
            return Err(error.into());
        }
    };
    execute_request(stream, timeout, request, handler)
}

fn execute_request<H>(
    mut stream: UnixStream,
    timeout: Duration,
    request: Request,
    handler: H,
) -> Result<(), ServiceError>
where
    H: Fn(Request) -> ExecutionOutput,
{
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(timeout))?;
    let request_id = format!("{:016x}", NEXT_REQUEST_ID.fetch_add(1, Ordering::Relaxed));
    send_message_with_deadline(
        &mut stream,
        &Message::Accepted(Accepted {
            request_id: request_id.clone(),
        }),
        Instant::now() + timeout,
    )?;
    let output = match catch_unwind(AssertUnwindSafe(|| handler(request))) {
        Ok(output) => output,
        Err(_) => ExecutionOutput::new(4, "", "request execution panicked\n"),
    };
    let completion = bounded_completion(request_id.clone(), output);
    match send_message_with_deadline(
        &mut stream,
        &Message::Completed(completion),
        Instant::now() + timeout,
    ) {
        Ok(()) => Ok(()),
        // The per-stream check above accepts each stream up to MAX_STREAM_BYTES,
        // but JSON escaping or per-field overhead can still push the strictly
        // encoded frame past the protocol's response limit; the bounded encoder
        // stays the final authority, so retry once with the output-limit
        // completion rather than ever attempting to truncate a valid frame.
        Err(ProtocolError::FrameTooLarge { .. }) => send_message_with_deadline(
            &mut stream,
            &Message::Completed(output_limit_completion(request_id)),
            // Encoding failed purely in memory before any I/O occurred, so a
            // fresh send deadline matches this file's per-send convention.
            Instant::now() + timeout,
        )
        .map_err(ServiceError::from),
        Err(error) => Err(ServiceError::from(error)),
    }
}

/// Run a bounded nonblocking listener until [`ShutdownToken::request`] is
/// called. At most eight request handlers execute concurrently, while up to
/// 64 validated nonblocking connections may deliver a request frame. Complete
/// requests that cannot execute immediately receive a framed busy rejection.
/// When `idle_timeout` is present, only a continuous period with no active
/// execution and no pending admission counts toward automatic shutdown.
/// The handler is isolated per worker and panics become exit-4 completions.
pub fn serve_until<H>(
    listener: UnixListener,
    timeout: Duration,
    idle_timeout: Option<Duration>,
    shutdown: ShutdownToken,
    handler: H,
) -> Result<LoopStats, ServiceError>
where
    H: Fn(Request) -> ExecutionOutput + Send + Sync + 'static,
{
    listener.set_nonblocking(true)?;
    let handler = Arc::new(handler);
    let (done_tx, done_rx) = mpsc::channel::<()>();
    let (done_read, done_write) =
        pipe().map_err(|error| ServiceError::Io(io::Error::from(error)))?;
    rustix::io::ioctl_fionbio(&done_read, true)
        .map_err(|error| ServiceError::Io(io::Error::from(error)))?;
    rustix::io::ioctl_fionbio(&done_write, true)
        .map_err(|error| ServiceError::Io(io::Error::from(error)))?;
    let done_write = Arc::new(done_write);
    let mut active = 0usize;
    let mut pending = VecDeque::<PendingAdmission>::new();
    let mut stats = LoopStats {
        accepted: 0,
        rejected: 0,
        completed: 0,
        workers_stuck: false,
    };
    let mut shutdown_at = None;
    let mut idle_since = idle_now();

    'service: loop {
        let mut progressed = false;
        while done_rx.try_recv().is_ok() {
            active = active.saturating_sub(1);
            stats.completed += 1;
            progressed = true;
        }

        if shutdown.requested() {
            shutdown_at.get_or_insert(Instant::now());
            stats.rejected += pending.len();
            pending.clear();
            if active == 0 {
                break;
            }
            if shutdown_at.expect("shutdown timestamp inserted").elapsed()
                >= Duration::from_secs(30)
            {
                stats.workers_stuck = true;
                break;
            }
            thread::sleep(Duration::from_millis(5));
            continue;
        }

        let mut index = 0;
        while index < pending.len() {
            let outcome = poll_live_admission(
                pending
                    .get_mut(index)
                    .expect("index remains within pending admissions"),
            );
            if shutdown.requested() {
                continue 'service;
            }
            match outcome {
                AdmissionPoll::Incomplete => index += 1,
                AdmissionPoll::Expired => {
                    let admission = pending
                        .remove(index)
                        .expect("expired admission remains present");
                    reject_bounded(
                        admission.stream,
                        "invalid_request",
                        "request frame did not arrive before the timeout",
                    );
                    stats.rejected += 1;
                    progressed = true;
                }
                AdmissionPoll::Complete(request) => {
                    let admission = pending
                        .remove(index)
                        .expect("complete admission remains present");
                    if active < MAX_ACTIVE_REQUESTS {
                        pause_before_execution_reservation();
                        let Some(reservation) = shutdown.try_reserve_execution() else {
                            stats.rejected += 1;
                            continue 'service;
                        };
                        spawn_execution(
                            reservation,
                            admission.stream,
                            request,
                            timeout,
                            Arc::clone(&handler),
                            done_tx.clone(),
                            Arc::clone(&done_write),
                        );
                        active += 1;
                    } else {
                        reject_bounded(
                            admission.stream,
                            "busy",
                            "service has eight active requests",
                        );
                        stats.rejected += 1;
                    }
                    progressed = true;
                }
                AdmissionPoll::Failed => {
                    let admission = pending
                        .remove(index)
                        .expect("failed admission remains present");
                    reject_bounded(admission.stream, "invalid_request", "request rejected");
                    stats.rejected += 1;
                    progressed = true;
                }
            }
        }

        if shutdown.requested() {
            continue;
        }

        match listener.accept() {
            Ok((stream, _)) => {
                let accepted_at = Instant::now();
                stats.accepted += 1;
                if !admission_peer_is_valid(&stream) {
                    stats.rejected += 1;
                } else {
                    stream.set_nonblocking(true)?;
                    let mut admission = PendingAdmission {
                        stream,
                        reader: FrameReader::new(MAX_REQUEST_BYTES),
                        expires_at: accepted_at + REQUEST_FRAME_DEADLINE,
                    };
                    let outcome = poll_live_admission(&mut admission);
                    if shutdown.requested() {
                        stats.rejected += 1;
                        continue;
                    }
                    match outcome {
                        AdmissionPoll::Complete(request) => {
                            if active < MAX_ACTIVE_REQUESTS {
                                pause_before_execution_reservation();
                                let Some(reservation) = shutdown.try_reserve_execution() else {
                                    stats.rejected += 1;
                                    continue;
                                };
                                spawn_execution(
                                    reservation,
                                    admission.stream,
                                    request,
                                    timeout,
                                    Arc::clone(&handler),
                                    done_tx.clone(),
                                    Arc::clone(&done_write),
                                );
                                active += 1;
                            } else {
                                reject_bounded(
                                    admission.stream,
                                    "busy",
                                    "service has eight active requests",
                                );
                                stats.rejected += 1;
                            }
                        }
                        AdmissionPoll::Failed => {
                            reject_bounded(admission.stream, "invalid_request", "request rejected");
                            stats.rejected += 1;
                        }
                        AdmissionPoll::Expired => {
                            reject_bounded(
                                admission.stream,
                                "invalid_request",
                                "request frame did not arrive before the timeout",
                            );
                            stats.rejected += 1;
                        }
                        AdmissionPoll::Incomplete => {
                            if pending.len() == MAX_PENDING_ADMISSIONS {
                                let oldest = pending
                                    .pop_front()
                                    .expect("full pending admissions contain an oldest entry");
                                reject_bounded(
                                    oldest.stream,
                                    "busy",
                                    "service has too many unfinished requests",
                                );
                                stats.rejected += 1;
                            }
                            pending.push_back(admission);
                        }
                    }
                    progressed = true;
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) => return Err(ServiceError::Io(error)),
        }
        observe_idle_state(active, pending.len());
        if active == 0 && pending.is_empty() {
            let now = idle_now();
            if progressed {
                idle_since = now;
            }
            if idle_timeout.is_some_and(|timeout| now.duration_since(idle_since) >= timeout) {
                shutdown.request();
                continue;
            }
        } else {
            idle_since = idle_now();
        }
        if !progressed {
            wait_for_service_event(&listener, &done_read, &pending, idle_timeout, idle_since)?;
            drain_completion_pipe(&done_read);
        }
    }
    Ok(stats)
}

fn wait_for_service_event(
    listener: &UnixListener,
    done_read: &OwnedFd,
    pending: &VecDeque<PendingAdmission>,
    idle_timeout: Option<Duration>,
    idle_since: Instant,
) -> Result<(), ServiceError> {
    let timeout = service_poll_timeout(pending, idle_timeout, idle_since);
    let mut descriptors = Vec::with_capacity(pending.len() + 2);
    descriptors.push(PollFd::new(listener, PollFlags::IN));
    descriptors.push(PollFd::new(done_read, PollFlags::IN));
    for admission in pending {
        descriptors.push(PollFd::new(&admission.stream, PollFlags::IN));
    }
    match poll(&mut descriptors, timeout.as_ref()) {
        Ok(_) | Err(Errno::INTR) => Ok(()),
        Err(error) => Err(ServiceError::Io(io::Error::from(error))),
    }
}

fn service_poll_timeout(
    pending: &VecDeque<PendingAdmission>,
    idle_timeout: Option<Duration>,
    idle_since: Instant,
) -> Option<Timespec> {
    if test_clock_is_installed() {
        return Some(duration_as_timespec(Duration::from_millis(1)));
    }
    let mut wait: Option<Duration> = None;
    let now = Instant::now();
    for admission in pending {
        let remaining = admission.expires_at.saturating_duration_since(now);
        wait = Some(wait.map_or(remaining, |current| current.min(remaining)));
    }
    if pending.is_empty()
        && let Some(idle) = idle_timeout
    {
        let remaining = idle.saturating_sub(idle_now().duration_since(idle_since));
        wait = Some(wait.map_or(remaining, |current| current.min(remaining)));
    }
    // Keep a finite bound so cooperative shutdown is observed without a 1ms spin.
    Some(duration_as_timespec(
        wait.unwrap_or(SHUTDOWN_POLL_BOUND).min(SHUTDOWN_POLL_BOUND),
    ))
}

const SHUTDOWN_POLL_BOUND: Duration = Duration::from_millis(250);

fn duration_as_timespec(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: duration.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: duration.subsec_nanos().into(),
    }
}

fn drain_completion_pipe(done_read: &OwnedFd) {
    let mut buffer = [0_u8; 64];
    loop {
        match rustix::io::read(done_read, &mut buffer) {
            Ok(0) | Err(Errno::AGAIN) => return,
            Ok(_) => {}
            Err(Errno::INTR) => {}
            Err(_) => return,
        }
    }
}

fn test_clock_is_installed() -> bool {
    #[cfg(test)]
    {
        IDLE_TEST_CLOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
    }
    #[cfg(not(test))]
    {
        false
    }
}

struct PendingAdmission {
    stream: UnixStream,
    reader: FrameReader,
    expires_at: Instant,
}

#[cfg(test)]
struct ReservationTestSeam {
    reached: mpsc::Sender<()>,
    release: std::sync::Barrier,
}

#[cfg(test)]
static EXECUTION_RESERVATION_TEST_SEAM: std::sync::Mutex<Option<Arc<ReservationTestSeam>>> =
    std::sync::Mutex::new(None);

#[cfg(test)]
struct IdleTestClock {
    now: Mutex<Instant>,
    states: mpsc::Sender<(usize, usize)>,
}

#[cfg(test)]
impl IdleTestClock {
    fn now(&self) -> Instant {
        *self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn advance(&self, duration: Duration) {
        let mut now = self
            .now
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *now += duration;
    }
}

#[cfg(test)]
static IDLE_TEST_CLOCK: Mutex<Option<Arc<IdleTestClock>>> = Mutex::new(None);

#[cfg(test)]
#[derive(Debug)]
enum PeerValidationTestEvent {
    Validation,
    Exited(LoopStats),
}

#[cfg(test)]
struct PeerValidationTestSeam {
    events: mpsc::Sender<PeerValidationTestEvent>,
    release: std::sync::Barrier,
}

#[cfg(test)]
static PEER_VALIDATION_TEST_SEAM: Mutex<Option<Arc<PeerValidationTestSeam>>> = Mutex::new(None);

#[cfg(test)]
static SERVICE_LOOP_TEST_LOCK: Mutex<()> = Mutex::new(());

fn admission_peer_is_valid(stream: &UnixStream) -> bool {
    #[cfg(test)]
    {
        let seam = PEER_VALIDATION_TEST_SEAM
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(seam) = seam {
            let _ = seam.events.send(PeerValidationTestEvent::Validation);
            seam.release.wait();
            return false;
        }
    }
    validate_peer(stream).is_ok()
}

fn idle_now() -> Instant {
    #[cfg(test)]
    {
        let clock = IDLE_TEST_CLOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(clock) = clock {
            return clock.now();
        }
    }
    Instant::now()
}

fn observe_idle_state(active: usize, pending: usize) {
    #[cfg(test)]
    {
        let clock = IDLE_TEST_CLOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(clock) = clock {
            let _ = clock.states.send((active, pending));
        }
    }
    #[cfg(not(test))]
    let _ = (active, pending);
}

fn pause_before_execution_reservation() {
    #[cfg(test)]
    {
        let seam = EXECUTION_RESERVATION_TEST_SEAM
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(seam) = seam {
            let _ = seam.reached.send(());
            seam.release.wait();
        }
    }
}

enum AdmissionPoll {
    Incomplete,
    Complete(Request),
    Failed,
    Expired,
}

fn poll_live_admission(admission: &mut PendingAdmission) -> AdmissionPoll {
    if Instant::now() >= admission.expires_at {
        return AdmissionPoll::Expired;
    }
    let outcome = poll_admission(admission);
    if Instant::now() >= admission.expires_at
        && matches!(
            &outcome,
            AdmissionPoll::Incomplete | AdmissionPoll::Complete(_)
        )
    {
        AdmissionPoll::Expired
    } else {
        outcome
    }
}

fn poll_admission(admission: &mut PendingAdmission) -> AdmissionPoll {
    let mut buffer = [0_u8; 8 * 1024];
    loop {
        if let Some(frame) = admission.reader.frame() {
            return match decode_request(frame) {
                Ok(Message::Request(request)) => AdmissionPoll::Complete(request),
                Ok(_) | Err(_) => AdmissionPoll::Failed,
            };
        }
        let wanted = admission.reader.want().min(buffer.len());
        match admission.stream.read(&mut buffer[..wanted]) {
            Ok(count) => match admission.reader.feed(&buffer[..count]) {
                Ok(_) => {}
                Err(_) => return AdmissionPoll::Failed,
            },
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                return AdmissionPoll::Incomplete;
            }
            Err(_) => return AdmissionPoll::Failed,
        }
    }
}

fn reject_bounded(mut stream: UnixStream, code: &str, message: &str) {
    if stream.set_nonblocking(false).is_err() {
        return;
    }
    let _ = send_message_with_deadline(
        &mut stream,
        &Message::Rejected(Rejected {
            code: code.into(),
            message: message.into(),
        }),
        Instant::now() + REQUEST_FRAME_DEADLINE,
    );
}

fn spawn_execution<H>(
    _reservation: ExecutionReservation,
    stream: UnixStream,
    request: Request,
    timeout: Duration,
    handler: Arc<H>,
    done: mpsc::Sender<()>,
    wake: Arc<OwnedFd>,
) where
    H: Fn(Request) -> ExecutionOutput + Send + Sync + 'static,
{
    thread::spawn(move || {
        // Held from the first line so the worker slot is released by Drop on
        // every exit path -- normal return, an early `?` in execute_request, or
        // an unwind outside the handler's own catch_unwind boundary.
        let _guard = SlotGuard::new(done, wake);
        let _ = execute_request(stream, timeout, request, |request| handler(request));
    });
}

/// Releases exactly one worker slot on drop -- on return, rejection, or unwind.
struct SlotGuard {
    done: Option<mpsc::Sender<()>>,
    wake: Arc<OwnedFd>,
}

impl SlotGuard {
    fn new(done: mpsc::Sender<()>, wake: Arc<OwnedFd>) -> Self {
        Self {
            done: Some(done),
            wake,
        }
    }
}

impl Drop for SlotGuard {
    fn drop(&mut self) {
        if let Some(done) = self.done.take() {
            let _ = done.send(());
        }
        let _ = rustix::io::write(&*self.wake, &[1_u8]);
    }
}

/// Connect to a private service and execute one request.
///
/// `timeout` bounds the request/acceptance handshake. Completion is bounded
/// by [`yams_protocol::COMPLETION_TIMEOUT`].
pub fn connect(
    path: &Path,
    request: Request,
    timeout: Duration,
) -> Result<Completed, ServiceError> {
    yams_runtime::connect(path, request, timeout).map_err(|error| match error {
        yams_runtime::IpcError::Io(error) => ServiceError::Io(error),
        yams_runtime::IpcError::Protocol(error) => ServiceError::Protocol(error),
        yams_runtime::IpcError::Peer(error) => ServiceError::Peer(error),
        yams_runtime::IpcError::Rejected { code, message } => {
            ServiceError::Rejected { code, message }
        }
        yams_runtime::IpcError::NotAccepted => ServiceError::NotAccepted,
        yams_runtime::IpcError::RequestIdMismatch => ServiceError::RequestIdMismatch,
    })
}

/// Build the completion for a request's captured output, enforcing the
/// independent per-stream limit. Neither stream is ever truncated: either
/// both fit within [`MAX_STREAM_BYTES`] and are returned verbatim, or the
/// fixed output-limit completion is returned instead.
fn bounded_completion(request_id: String, output: ExecutionOutput) -> Completed {
    if output.stdout.len() > MAX_STREAM_BYTES || output.stderr.len() > MAX_STREAM_BYTES {
        return output_limit_completion(request_id);
    }
    Completed {
        request_id,
        exit_code: output.exit_code,
        stdout: output.stdout,
        stderr: output.stderr,
    }
}

/// The oracle-pinned completion sent when output cannot be returned in full.
fn output_limit_completion(request_id: String) -> Completed {
    Completed {
        request_id,
        exit_code: 4,
        stdout: String::new(),
        stderr: "yams: output limit\n".into(),
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::ops::Deref;
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::sync::{Arc, Barrier, mpsc};
    use std::time::{Duration, Instant};

    use rustix::pipe::pipe;

    use yams_protocol::{MAX_REQUEST_BYTES, Message, Request, receive_response, send_message};

    use super::{
        AdmissionPoll, EXECUTION_RESERVATION_TEST_SEAM, ExecutionOutput, FrameReader,
        IDLE_TEST_CLOCK, IdleTestClock, PEER_VALIDATION_TEST_SEAM, PeerValidationTestEvent,
        PeerValidationTestSeam, PendingAdmission, ReservationTestSeam, SERVICE_LOOP_TEST_LOCK,
        ShutdownToken, SlotGuard, poll_live_admission, serve_until,
    };

    struct IdleClockGuard {
        clock: Arc<IdleTestClock>,
        previous: Option<Arc<IdleTestClock>>,
    }

    impl Deref for IdleClockGuard {
        type Target = IdleTestClock;

        fn deref(&self) -> &Self::Target {
            &self.clock
        }
    }

    impl Drop for IdleClockGuard {
        fn drop(&mut self) {
            let mut slot = IDLE_TEST_CLOCK
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = self.previous.take();
        }
    }

    fn install_idle_clock() -> (IdleClockGuard, mpsc::Receiver<(usize, usize)>) {
        let (states, state_rx) = mpsc::channel();
        let clock = Arc::new(IdleTestClock {
            now: std::sync::Mutex::new(Instant::now()),
            states,
        });
        let previous = IDLE_TEST_CLOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(Arc::clone(&clock));
        (IdleClockGuard { clock, previous }, state_rx)
    }

    struct PeerValidationSeamGuard {
        seam: Arc<PeerValidationTestSeam>,
        previous: Option<Arc<PeerValidationTestSeam>>,
    }

    impl Deref for PeerValidationSeamGuard {
        type Target = PeerValidationTestSeam;

        fn deref(&self) -> &Self::Target {
            &self.seam
        }
    }

    impl Drop for PeerValidationSeamGuard {
        fn drop(&mut self) {
            let mut slot = PEER_VALIDATION_TEST_SEAM
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *slot = self.previous.take();
        }
    }

    fn install_peer_validation_seam(
        events: mpsc::Sender<PeerValidationTestEvent>,
    ) -> PeerValidationSeamGuard {
        let seam = Arc::new(PeerValidationTestSeam {
            events,
            release: Barrier::new(2),
        });
        let previous = PEER_VALIDATION_TEST_SEAM
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(Arc::clone(&seam));
        PeerValidationSeamGuard { seam, previous }
    }

    fn receive_state(
        states: &mpsc::Receiver<(usize, usize)>,
        predicate: impl Fn((usize, usize)) -> bool,
    ) -> (usize, usize) {
        loop {
            let state = states
                .recv_timeout(Duration::from_secs(1))
                .expect("listener reported idle state");
            if predicate(state) {
                return state;
            }
        }
    }

    #[test]
    fn idle_timeout_begins_when_the_listener_loop_starts() {
        let _serial = SERVICE_LOOP_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (clock, states) = install_idle_clock();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (result_tx, result_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let stats = serve_until(
                listener,
                Duration::from_secs(1),
                Some(Duration::from_secs(10)),
                ShutdownToken::new(),
                |_| panic!("idle service must not execute a handler"),
            )
            .unwrap();
            result_tx.send(stats).unwrap();
        });

        receive_state(&states, |state| state == (0, 0));
        clock.advance(Duration::from_secs(10));
        let stats = result_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("idle service expired after logical deadline");
        server.join().unwrap();
        assert_eq!(stats.accepted, 0);
        assert_eq!(stats.rejected, 0);
        assert_eq!(stats.completed, 0);
    }

    #[test]
    fn no_idle_timeout_preserves_library_lifetime() {
        let _serial = SERVICE_LOOP_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (clock, states) = install_idle_clock();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stop = ShutdownToken::new();
        let server_stop = stop.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let stats = serve_until(listener, Duration::from_secs(1), None, server_stop, |_| {
                panic!("idle service must not execute a handler")
            })
            .unwrap();
            result_tx.send(stats).unwrap();
        });

        receive_state(&states, |state| state == (0, 0));
        while states.try_recv().is_ok() {}
        clock.advance(Duration::from_secs(10_000));
        receive_state(&states, |state| state == (0, 0));
        assert!(matches!(
            result_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        stop.request();
        let stats = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        server.join().unwrap();
        assert_eq!(stats.accepted, 0);
        assert_eq!(stats.rejected, 0);
        assert_eq!(stats.completed, 0);
    }

    #[test]
    fn active_work_suppresses_idle_expiry_and_completion_resets_it() {
        let _serial = SERVICE_LOOP_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (clock, states) = install_idle_clock();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stop = ShutdownToken::new();
        let (entered_tx, entered_rx) = mpsc::channel();
        let release = Arc::new(Barrier::new(2));
        let server_release = Arc::clone(&release);
        let (result_tx, result_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let stats = serve_until(
                listener,
                Duration::from_secs(1),
                Some(Duration::from_secs(10)),
                stop,
                move |_| {
                    entered_tx.send(()).unwrap();
                    server_release.wait();
                    ExecutionOutput::new(0, "ok\n", "")
                },
            )
            .unwrap();
            result_tx.send(stats).unwrap();
        });
        let mut client = UnixStream::connect(&path).unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(1)))
            .unwrap();
        send_message(
            &mut client,
            &Message::Request(
                Request::from_argv(vec!["search".into()], String::from("/tmp"))
                    .expect("service request is not --write"),
            ),
        )
        .unwrap();
        entered_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        receive_state(&states, |(active, _)| active == 1);

        while states.try_recv().is_ok() {}
        clock.advance(Duration::from_secs(100));
        receive_state(&states, |(active, _)| active == 1);
        assert!(matches!(
            result_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        release.wait();
        assert!(matches!(
            receive_response(&mut client),
            Ok(Message::Accepted(_))
        ));
        assert!(matches!(
            receive_response(&mut client),
            Ok(Message::Completed(_))
        ));
        receive_state(&states, |state| state == (0, 0));

        while states.try_recv().is_ok() {}
        clock.advance(Duration::from_secs(9));
        receive_state(&states, |state| state == (0, 0));
        assert!(matches!(
            result_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        clock.advance(Duration::from_secs(1));
        let stats = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        server.join().unwrap();
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.rejected, 0);
        assert_eq!(stats.completed, 1);
    }

    #[test]
    fn pending_admission_suppresses_idle_expiry() {
        let _serial = SERVICE_LOOP_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (clock, states) = install_idle_clock();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let mut client = UnixStream::connect(&path).unwrap();
        client.write_all(&[0]).unwrap();
        let stop = ShutdownToken::new();
        let server_stop = stop.clone();
        let (result_tx, result_rx) = mpsc::channel();
        let server = std::thread::spawn(move || {
            let stats = serve_until(
                listener,
                Duration::from_secs(1),
                Some(Duration::from_secs(10)),
                server_stop,
                |_| panic!("partial request must not execute a handler"),
            )
            .unwrap();
            result_tx.send(stats).unwrap();
        });

        receive_state(&states, |(_, pending)| pending == 1);
        while states.try_recv().is_ok() {}
        clock.advance(Duration::from_secs(100));
        receive_state(&states, |(_, pending)| pending == 1);
        assert!(matches!(
            result_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty)
        ));
        stop.request();
        let stats = result_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        server.join().unwrap();
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.rejected, 1);
        assert_eq!(stats.completed, 0);
    }

    #[test]
    fn rejected_peers_do_not_starve_idle_expiry() {
        let _serial = SERVICE_LOOP_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (clock, _states) = install_idle_clock();
        let (events_tx, events_rx) = mpsc::channel();
        let peer_seam = install_peer_validation_seam(events_tx.clone());
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let _clients = [
            UnixStream::connect(&path).unwrap(),
            UnixStream::connect(&path).unwrap(),
        ];
        let stop = ShutdownToken::new();
        let server_stop = stop.clone();
        let server = std::thread::spawn(move || {
            let stats = serve_until(
                listener,
                Duration::from_secs(1),
                Some(Duration::from_secs(10)),
                server_stop,
                |_| panic!("rejected peer must not execute a handler"),
            )
            .unwrap();
            events_tx
                .send(PeerValidationTestEvent::Exited(stats))
                .unwrap();
        });

        assert!(matches!(
            events_rx.recv_timeout(Duration::from_secs(1)),
            Ok(PeerValidationTestEvent::Validation)
        ));
        clock.advance(Duration::from_secs(10));
        peer_seam.release.wait();
        let next = events_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        let (stats, attempted_second_validation) = match next {
            PeerValidationTestEvent::Exited(stats) => (stats, false),
            PeerValidationTestEvent::Validation => {
                stop.request();
                peer_seam.release.wait();
                let PeerValidationTestEvent::Exited(stats) =
                    events_rx.recv_timeout(Duration::from_secs(1)).unwrap()
                else {
                    panic!("service did not exit after cleanup shutdown")
                };
                (stats, true)
            }
        };
        server.join().unwrap();

        assert!(
            !attempted_second_validation,
            "idle expiry must precede another rejected peer"
        );
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.rejected, 1);
        assert_eq!(stats.completed, 0);
    }

    #[test]
    fn shutdown_closure_wins_before_a_paused_execution_reservation() {
        let _serial = SERVICE_LOOP_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("service.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let stop = ShutdownToken::new();
        let server_stop = stop.clone();
        let (invoked_tx, invoked_rx) = mpsc::channel();
        let (reached_tx, reached_rx) = mpsc::channel();
        let seam = Arc::new(ReservationTestSeam {
            reached: reached_tx,
            release: Barrier::new(2),
        });
        *EXECUTION_RESERVATION_TEST_SEAM.lock().unwrap() = Some(Arc::clone(&seam));
        let server = std::thread::spawn(move || {
            serve_until(
                listener,
                Duration::from_secs(1),
                None,
                server_stop,
                move |_| {
                    invoked_tx.send(()).unwrap();
                    ExecutionOutput::new(0, "unexpected\n", "")
                },
            )
            .unwrap()
        });
        let mut client = UnixStream::connect(&path).unwrap();
        send_message(
            &mut client,
            &Message::Request(
                Request::from_argv(vec!["search".into()], String::from("/tmp"))
                    .expect("service request is not --write"),
            ),
        )
        .unwrap();

        reached_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("completed admission paused immediately before reservation");
        stop.request();
        seam.release.wait();
        let stats = server.join().unwrap();
        *EXECUTION_RESERVATION_TEST_SEAM.lock().unwrap() = None;

        assert!(matches!(
            invoked_rx.try_recv(),
            Err(mpsc::TryRecvError::Empty | mpsc::TryRecvError::Disconnected)
        ));
        assert_eq!(stats.accepted, 1);
        assert_eq!(stats.rejected, 1);
        assert_eq!(stats.completed, 0);
    }

    #[test]
    fn expired_admission_is_not_polled_even_with_a_complete_frame_waiting() {
        let (mut client, service) = UnixStream::pair().unwrap();
        send_message(
            &mut client,
            &Message::Request(
                Request::from_argv(vec!["search".into()], String::from("/tmp"))
                    .expect("service request is not --write"),
            ),
        )
        .unwrap();
        service.set_nonblocking(true).unwrap();
        let mut admission = PendingAdmission {
            stream: service,
            reader: FrameReader::new(MAX_REQUEST_BYTES),
            expires_at: Instant::now() - Duration::from_millis(1),
        };

        assert!(matches!(
            poll_live_admission(&mut admission),
            AdmissionPoll::Expired
        ));
        assert_eq!(admission.reader.want(), 4);
    }

    fn unused_wake() -> Arc<OwnedFd> {
        let (_read, write) = pipe().unwrap();
        Arc::new(write)
    }

    #[test]
    fn slot_guard_reports_completion_even_when_the_worker_panics() {
        let (tx, rx) = std::sync::mpsc::channel();
        let wake = unused_wake();
        let handle = std::thread::spawn(move || {
            let _guard = SlotGuard::new(tx, wake);
            panic!("outside the handler unwind boundary");
        });
        assert!(handle.join().is_err());
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .expect("slot credit released exactly once by Drop");
    }

    #[test]
    fn slot_guard_reports_completion_exactly_once_on_normal_return() {
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let _guard = SlotGuard::new(tx, unused_wake());
        }
        rx.recv_timeout(std::time::Duration::from_secs(1))
            .expect("slot credit released on drop");
        assert!(matches!(
            rx.try_recv(),
            Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
    }
}
