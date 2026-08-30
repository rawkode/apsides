#![forbid(unsafe_code)]

//! Process-isolated loopback renderer supervision.
//!
//! This crate intentionally makes no claim of network isolation for evaluator
//! children. Its boundary is process lifecycle and a capability-scoped,
//! revalidated bundle store. Deployments that require network isolation must
//! impose it outside this process (for example, by the container runtime).

use std::{
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use oxideloop_bundle_contract::{BundleStore, ExecutionPhaseV0, InstalledBundle};
use oxideloop_renderer_abi::sha256_hex;
use oxideloop_renderer_abi::{parse_validate_action_input, parse_validate_render_input};
use oxideloop_renderer_protocol::{
    ErrorCategoryV0, FrameV0, MAX_DURATION_MILLIS, PhaseV0, ProtocolError, RequestV0,
    ResponseStatusV0, ResponseV0, phase_cap, read_frame, write_frame,
};
use renderer_host::{HostError, Invocation, Limits, RendererHost};
use thiserror::Error;

const HOST_TERMINATION_GRACE: Duration = Duration::from_millis(100);
/// The small, fixed time reserved after the request deadline to prove that a
/// terminated direct child is reaped and its two protocol pipes have closed.
/// If that proof is unavailable, the supervisor must exit rather than reuse
/// the process namespace.
const CONTAINMENT_CONFIRMATION_GRACE: Duration = Duration::from_millis(100);
const MAX_SOCKET_DEADLINE: Duration = Duration::from_millis(MAX_DURATION_MILLIS as u64);
const MAX_SOURCE_BYTES: usize = 256 * 1024;

#[derive(Debug, Error)]
pub enum WorkerError {
    #[error("renderer worker must bind exactly 127.0.0.1")]
    NonLoopbackBind,
    #[error("renderer worker socket I/O failed")]
    Io,
    #[error("renderer worker observed an invalid protocol frame")]
    Protocol,
    #[error("renderer evaluator failed or its containment is uncertain")]
    EvaluatorFailure,
    #[error("renderer evaluator containment cannot be confirmed")]
    ContainmentUncertain,
}

impl From<io::Error> for WorkerError {
    fn from(_: io::Error) -> Self {
        Self::Io
    }
}

/// Fixed supervisor inputs. The evaluator executable is deliberately an
/// explicit path, rather than a `PATH` lookup inherited from the controller.
#[derive(Clone, Debug)]
pub struct WorkerConfig {
    pub bind: SocketAddr,
    pub bundle_store: PathBuf,
    pub evaluator_binary: PathBuf,
}

impl WorkerConfig {
    #[must_use]
    pub fn new(bind: SocketAddr, bundle_store: PathBuf, evaluator_binary: PathBuf) -> Self {
        Self {
            bind,
            bundle_store,
            evaluator_binary,
        }
    }
}

/// A one-at-a-time loopback renderer supervisor.
pub struct Supervisor {
    listener: TcpListener,
    config: WorkerConfig,
}

impl Supervisor {
    /// Bind only the explicit IPv4 localhost address.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::NonLoopbackBind`] for all wildcard, IPv6, and
    /// non-local addresses. Socket errors are returned as [`WorkerError::Io`].
    pub fn bind(config: WorkerConfig) -> Result<Self, WorkerError> {
        validate_loopback_bind(config.bind)?;
        let listener = TcpListener::bind(config.bind)?;
        Ok(Self { listener, config })
    }

    /// The concrete listener address, including an OS-selected port zero.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::Io`] when the listener address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr, WorkerError> {
        self.listener.local_addr().map_err(Into::into)
    }

    /// Serve exactly one client connection. A malformed frame, evaluator loss,
    /// or response ambiguity is an error for the outer process to treat as a
    /// restart condition.
    ///
    /// # Errors
    ///
    /// Returns an error for any condition for which continuing would make the
    /// worker's containment or protocol state uncertain.
    pub fn serve_one(&self) -> Result<(), WorkerError> {
        let (mut stream, peer) = self.listener.accept()?;
        if peer.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
            return Err(WorkerError::Protocol);
        }
        stream.set_nonblocking(true)?;
        let admission_deadline = Instant::now() + MAX_SOCKET_DEADLINE;
        let frame = read_frame(DeadlineStream::new(&mut stream, admission_deadline))
            .map_err(map_protocol_error)?;
        let FrameV0::Request(request) = frame else {
            return Err(WorkerError::Protocol);
        };
        let request_deadline = Instant::now() + request_duration(&request);
        let response = self.execute_request(&request)?;
        response.matches(&request).map_err(map_protocol_error)?;
        write_frame(
            DeadlineStream::new(&mut stream, request_deadline),
            &FrameV0::Response(response),
        )
        .map_err(map_protocol_error)?;
        stream.shutdown(Shutdown::Write)?;
        Ok(())
    }

    /// Run until any unsafe condition occurs. The caller must exit its process
    /// when this function returns an error, so an orchestrator can replace the
    /// supervisor with a clean process.
    ///
    /// # Errors
    ///
    /// See [`Self::serve_one`].
    pub fn run(&self) -> Result<(), WorkerError> {
        loop {
            self.serve_one()?;
        }
    }

    fn execute_request(&self, request: &RequestV0) -> Result<ResponseV0, WorkerError> {
        run_evaluator(&self.config, request).and_then(|response| {
            response.matches(request).map_err(map_protocol_error)?;
            match response.status {
                ResponseStatusV0::Ok | ResponseStatusV0::Recoverable => Ok(response),
                ResponseStatusV0::Fatal | ResponseStatusV0::Protocol => {
                    Err(WorkerError::EvaluatorFailure)
                }
            }
        })
    }
}

/// Reject anything other than an explicit IPv4 loopback bind.
///
/// # Errors
///
/// Returns [`WorkerError::NonLoopbackBind`] unless `address` is 127.0.0.1.
pub fn validate_loopback_bind(address: SocketAddr) -> Result<(), WorkerError> {
    match address {
        SocketAddr::V4(value) if *value.ip() == Ipv4Addr::LOCALHOST => Ok(()),
        SocketAddr::V4(_) | SocketAddr::V6(_) => Err(WorkerError::NonLoopbackBind),
    }
}

/// Entry point for the short-lived evaluator child. It is public only so the
/// package binary can dispatch its private subcommand; callers should launch
/// it solely through [`Supervisor`].
///
/// # Errors
///
/// Returns an error for malformed parent input, failed output, or fatal host
/// outcomes. The binary maps such errors to a non-zero exit, causing its
/// supervisor to exit rather than continue.
pub fn evaluator_once(store_root: &Path) -> Result<(), WorkerError> {
    let frame = read_frame(io::stdin().lock()).map_err(map_protocol_error)?;
    let FrameV0::Request(request) = frame else {
        return Err(WorkerError::Protocol);
    };
    let response = evaluate_request(store_root, &request)?;
    response.matches(&request).map_err(map_protocol_error)?;
    match response.status {
        ResponseStatusV0::Ok | ResponseStatusV0::Recoverable => {
            write_frame(io::stdout().lock(), &FrameV0::Response(response))
                .map_err(map_protocol_error)?;
            Ok(())
        }
        ResponseStatusV0::Fatal | ResponseStatusV0::Protocol => Err(WorkerError::EvaluatorFailure),
    }
}

fn run_evaluator(config: &WorkerConfig, request: &RequestV0) -> Result<ResponseV0, WorkerError> {
    let child = Command::new(&config.evaluator_binary)
        .arg("--evaluator")
        .arg("--store-root")
        .arg(&config.bundle_store)
        .env_clear()
        .current_dir("/")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| WorkerError::EvaluatorFailure)?;
    let mut child = ChildGuard::new(child);
    let deadline = Instant::now() + request_duration(request);
    let cleanup_deadline = deadline + CONTAINMENT_CONFIRMATION_GRACE;
    // These anonymous pipe endpoints are capabilities created for this child:
    // no network listener or inherited Kubernetes environment participates in
    // the evaluator protocol. Both directions still use the fixed P0 frame
    // parser, which bounds allocation and requires exactly one frame plus EOF.
    let stdin = match child.take_stdin() {
        Ok(stdin) => stdin,
        Err(error) => return child.cleanup_after(error, cleanup_deadline),
    };
    let stdout = match child.take_stdout() {
        Ok(stdout) => stdout,
        Err(error) => return child.cleanup_after(error, cleanup_deadline),
    };
    let (write_tx, write_rx) = mpsc::sync_channel(1);
    let (read_tx, read_rx) = mpsc::sync_channel(1);
    let write_request = request.clone();
    let writer = thread::spawn(move || {
        let result = write_frame(stdin, &FrameV0::Request(write_request));
        let _ = write_tx.send(result);
    });
    let reader = thread::spawn(move || {
        let result = read_frame(stdout);
        let _ = read_tx.send(result);
    });

    let frame = match supervise_evaluator(&mut child, &write_rx, &read_rx, deadline) {
        Ok(frame) => {
            child.reap_after_exit(cleanup_deadline)?;
            join_pipe_tasks(writer, reader, cleanup_deadline)?;
            frame
        }
        Err(error) => {
            // Preserve the original evaluator failure only after direct-child
            // reaping and both pipe workers are conclusively complete. A
            // descendant that inherited stdout is containment uncertainty:
            // returning that distinct error makes `main` exit immediately.
            child.cleanup(cleanup_deadline)?;
            join_pipe_tasks(writer, reader, cleanup_deadline)?;
            return Err(error);
        }
    };
    let FrameV0::Response(response) = frame else {
        return Err(WorkerError::EvaluatorFailure);
    };
    Ok(response)
}

fn supervise_evaluator(
    child: &mut ChildGuard,
    write_rx: &mpsc::Receiver<Result<(), ProtocolError>>,
    read_rx: &mpsc::Receiver<Result<FrameV0, ProtocolError>>,
    deadline: Instant,
) -> Result<FrameV0, WorkerError> {
    let mut write_complete = false;
    let mut frame = None;
    loop {
        if Instant::now() >= deadline {
            return Err(WorkerError::EvaluatorFailure);
        }
        if !write_complete {
            match write_rx.try_recv() {
                Ok(Ok(())) => write_complete = true,
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(WorkerError::EvaluatorFailure);
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if frame.is_none() {
            match read_rx.try_recv() {
                Ok(Ok(value)) => frame = Some(value),
                Ok(Err(_)) | Err(mpsc::TryRecvError::Disconnected) => {
                    return Err(WorkerError::Protocol);
                }
                Err(mpsc::TryRecvError::Empty) => {}
            }
        }
        if let Some(status) = child.try_wait()? {
            return if status.success() {
                if !write_complete {
                    match write_rx.recv_timeout(remaining(deadline)?) {
                        Ok(Ok(())) => {}
                        Ok(Err(_)) | Err(_) => return Err(WorkerError::EvaluatorFailure),
                    }
                }
                match frame {
                    Some(frame) => Ok(frame),
                    None => match read_rx.recv_timeout(remaining(deadline)?) {
                        Ok(Ok(frame)) => Ok(frame),
                        Ok(Err(_)) | Err(_) => Err(WorkerError::Protocol),
                    },
                }
            } else {
                Err(WorkerError::EvaluatorFailure)
            };
        }
        thread::sleep(Duration::from_millis(2));
    }
}

fn join_pipe_tasks(
    writer: thread::JoinHandle<()>,
    reader: thread::JoinHandle<()>,
    deadline: Instant,
) -> Result<(), WorkerError> {
    // `join` may otherwise wait forever when a descendant inherited one of
    // the pipe descriptors. Joining only a completed thread is non-blocking;
    // dropping unfinished handles detaches them so the caller can terminate
    // the whole supervisor process and let the container runtime clean the
    // namespace.
    while (!writer.is_finished() || !reader.is_finished()) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(1));
    }
    if !writer.is_finished() || !reader.is_finished() {
        drop(writer);
        drop(reader);
        return Err(WorkerError::ContainmentUncertain);
    }
    writer
        .join()
        .map_err(|_| WorkerError::ContainmentUncertain)?;
    reader
        .join()
        .map_err(|_| WorkerError::ContainmentUncertain)?;
    Ok(())
}

/// Owns a spawned evaluator until it has definitely been reaped. All normal
/// post-spawn paths call either [`Self::reap_after_exit`] or [`Self::cleanup`].
struct ChildGuard {
    child: Option<Child>,
    status: Option<std::process::ExitStatus>,
}

impl ChildGuard {
    fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            status: None,
        }
    }

    fn take_stdin(&mut self) -> Result<std::process::ChildStdin, WorkerError> {
        self.child
            .as_mut()
            .and_then(|child| child.stdin.take())
            .ok_or(WorkerError::EvaluatorFailure)
    }

    fn take_stdout(&mut self) -> Result<std::process::ChildStdout, WorkerError> {
        self.child
            .as_mut()
            .and_then(|child| child.stdout.take())
            .ok_or(WorkerError::EvaluatorFailure)
    }

    fn try_wait(&mut self) -> Result<Option<std::process::ExitStatus>, WorkerError> {
        if let Some(status) = self.status {
            return Ok(Some(status));
        }
        let status = self
            .child
            .as_mut()
            .ok_or(WorkerError::EvaluatorFailure)?
            .try_wait()?;
        if let Some(status) = status {
            self.status = Some(status);
        }
        Ok(status)
    }

    fn reap_after_exit(&mut self, deadline: Instant) -> Result<(), WorkerError> {
        self.confirm_reaped(deadline)
    }

    fn cleanup(&mut self, deadline: Instant) -> Result<(), WorkerError> {
        if self.status.is_none() {
            let child = self
                .child
                .as_mut()
                .ok_or(WorkerError::ContainmentUncertain)?;
            if let Err(error) = child.kill()
                && error.kind() != io::ErrorKind::InvalidInput
            {
                return Err(WorkerError::ContainmentUncertain);
            }
        }
        self.confirm_reaped(deadline)
    }

    fn confirm_reaped(&mut self, deadline: Instant) -> Result<(), WorkerError> {
        loop {
            match self.try_wait() {
                Ok(Some(_)) => {
                    self.child.take();
                    return Ok(());
                }
                Ok(None) if Instant::now() < deadline => {
                    thread::sleep(Duration::from_millis(1));
                }
                Ok(None) | Err(_) => return Err(WorkerError::ContainmentUncertain),
            }
        }
    }

    fn cleanup_after<T>(
        &mut self,
        error: WorkerError,
        deadline: Instant,
    ) -> Result<T, WorkerError> {
        self.cleanup(deadline)?;
        Err(error)
    }
}

fn descriptor_response(store_root: &Path, request: &RequestV0) -> Result<ResponseV0, WorkerError> {
    if !request.payload.is_empty() {
        return Ok(recoverable(request, ErrorCategoryV0::InvalidInput));
    }
    let store = BundleStore::new(store_root).map_err(|_| WorkerError::EvaluatorFailure)?;
    let Ok(installed) = store.open_installed(&request.bundle_digest) else {
        return Ok(recoverable(request, ErrorCategoryV0::BundleUnavailable));
    };
    let Ok(artifact) = installed.open_declared(&installed.manifest().descriptor.path) else {
        return Ok(recoverable(request, ErrorCategoryV0::BundleUnavailable));
    };
    if sha256_hex(artifact.bytes()) != artifact.sha256()
        || artifact.bytes().len() > phase_cap(request.phase)
    {
        return Ok(recoverable(request, ErrorCategoryV0::BundleUnavailable));
    }
    Ok(ResponseV0 {
        request_id: request.request_id,
        phase: request.phase,
        bundle_digest: request.bundle_digest.clone(),
        module_digest: None,
        status: ResponseStatusV0::Ok,
        error_category: None,
        payload: artifact.bytes().to_vec(),
    })
}

fn evaluate_request(store_root: &Path, request: &RequestV0) -> Result<ResponseV0, WorkerError> {
    if request.phase == PhaseV0::Descriptor {
        return descriptor_response(store_root, request);
    }
    let store = BundleStore::new(store_root).map_err(|_| WorkerError::EvaluatorFailure)?;
    let Ok(installed) = store.open_installed(&request.bundle_digest) else {
        return Ok(recoverable(request, ErrorCategoryV0::BundleUnavailable));
    };
    let Some((phase, identity, expected_digest)) = module_for_request(&installed, request) else {
        return Ok(recoverable(request, ErrorCategoryV0::BundleUnavailable));
    };
    let Ok(artifact) = installed.open_phase_module(phase, identity) else {
        return Ok(recoverable(request, ErrorCategoryV0::BundleUnavailable));
    };
    let Some(protocol_digest) = request.module_digest.as_deref() else {
        return Err(WorkerError::Protocol);
    };
    if installed.digest() != request.bundle_digest
        || expected_digest != protocol_digest
        || artifact.sha256() != protocol_digest
        || sha256_hex(artifact.bytes()) != protocol_digest
    {
        return Ok(recoverable(request, ErrorCategoryV0::BundleUnavailable));
    }
    let Ok(source) = std::str::from_utf8(artifact.bytes()) else {
        return Ok(recoverable(request, ErrorCategoryV0::GuestFailure));
    };
    let limits = Limits {
        max_source_bytes: MAX_SOURCE_BYTES,
        max_input_bytes: phase_cap(request.phase),
        max_output_bytes: phase_cap(request.phase),
        wall_clock: request_duration(request),
        termination_grace: HOST_TERMINATION_GRACE,
    };
    let Ok(payload) = std::str::from_utf8(&request.payload) else {
        return Ok(recoverable(request, ErrorCategoryV0::InvalidInput));
    };
    if !payload_matches_request(request, identity, payload) {
        return Ok(recoverable(request, ErrorCategoryV0::InvalidInput));
    }
    let host = RendererHost::new(limits);
    match request.phase {
        PhaseV0::Render => host_response(request, host.render(source, payload)),
        PhaseV0::Action => host_response(request, host.action(source, payload)),
        PhaseV0::Descriptor => Err(WorkerError::Protocol),
    }
}

/// Validate the typed caller input a second time at the process boundary and
/// bind its configured bundle/action values to the exact P0 request and module
/// selected from the verified manifest before constructing a V8 isolate.
fn payload_matches_request(request: &RequestV0, identity: &str, payload: &str) -> bool {
    match request.phase {
        PhaseV0::Render => parse_validate_render_input(payload)
            .is_ok_and(|input| input.document.bundle_digest == request.bundle_digest),
        PhaseV0::Action => {
            let Some(module_digest) = request.module_digest.as_deref() else {
                return false;
            };
            parse_validate_action_input(payload).is_ok_and(|input| {
                input.document.bundle_digest == request.bundle_digest
                    && input.document.action_module_identity == identity
                    && input.document.action_module_digest == module_digest
            })
        }
        PhaseV0::Descriptor => false,
    }
}

fn module_for_request<'a>(
    installed: &'a InstalledBundle,
    request: &RequestV0,
) -> Option<(ExecutionPhaseV0, &'a str, &'a str)> {
    let protocol_digest = request.module_digest.as_deref()?;
    let (phase, modules) = match request.phase {
        PhaseV0::Render => (
            ExecutionPhaseV0::Render,
            &installed.manifest().render_modules,
        ),
        PhaseV0::Action => (
            ExecutionPhaseV0::Action,
            &installed.manifest().action_modules,
        ),
        PhaseV0::Descriptor => return None,
    };
    let mut matches =
        modules.iter().filter(|module| {
            module.file.sha256 == protocol_digest
                && installed.manifest().phase_modules.iter().any(|mapping| {
                    mapping.phase == phase && mapping.module_identity == module.identity
                })
        });
    let module = matches.next()?;
    if matches.next().is_some() {
        return None;
    }
    Some((phase, &module.identity, &module.file.sha256))
}

fn host_response<T>(
    request: &RequestV0,
    invocation: Invocation<T>,
) -> Result<ResponseV0, WorkerError> {
    match invocation {
        Invocation::Completed(output) => Ok(ok(request, output.canonical_bytes)),
        Invocation::Recoverable(error) => match error {
            HostError::StartupFailure => Err(WorkerError::EvaluatorFailure),
            HostError::InputTooLarge { .. } | HostError::InputContract => {
                Ok(recoverable(request, ErrorCategoryV0::InvalidInput))
            }
            HostError::SourceTooLarge { .. }
            | HostError::OutputTooLarge { .. }
            | HostError::OutputContract
            | HostError::InvalidSource
            | HostError::RuntimeFailure => Ok(recoverable(request, ErrorCategoryV0::GuestFailure)),
        },
        Invocation::TimedOut | Invocation::ProcessFatal(_) => Err(WorkerError::EvaluatorFailure),
    }
}

fn ok(request: &RequestV0, payload: Vec<u8>) -> ResponseV0 {
    ResponseV0 {
        request_id: request.request_id,
        phase: request.phase,
        bundle_digest: request.bundle_digest.clone(),
        module_digest: request.module_digest.clone(),
        status: ResponseStatusV0::Ok,
        error_category: None,
        payload,
    }
}

fn recoverable(request: &RequestV0, category: ErrorCategoryV0) -> ResponseV0 {
    ResponseV0 {
        request_id: request.request_id,
        phase: request.phase,
        bundle_digest: request.bundle_digest.clone(),
        module_digest: request.module_digest.clone(),
        status: ResponseStatusV0::Recoverable,
        error_category: Some(category),
        payload: vec![],
    }
}

fn request_duration(request: &RequestV0) -> Duration {
    Duration::from_millis(u64::from(request.duration_millis))
}

fn remaining(deadline: Instant) -> Result<Duration, WorkerError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(WorkerError::EvaluatorFailure)
}

/// Applies the remaining absolute request time before every socket operation.
/// Reconfiguring the timeout on each `Read`/`Write` prevents a drip-feeding
/// peer from extending its allotted wall-clock duration with partial progress.
struct DeadlineStream<'a> {
    stream: &'a mut TcpStream,
    deadline: Instant,
}

impl<'a> DeadlineStream<'a> {
    fn new(stream: &'a mut TcpStream, deadline: Instant) -> Self {
        Self { stream, deadline }
    }

    fn until_ready<T>(
        &mut self,
        operation: impl FnMut(&mut TcpStream) -> io::Result<T>,
    ) -> io::Result<T> {
        let mut operation = operation;
        loop {
            if self.deadline <= Instant::now() {
                return Err(timeout_error());
            }
            match operation(self.stream) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(1));
                }
                result => return result,
            }
        }
    }
}

impl Read for DeadlineStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.until_ready(|stream| stream.read(buffer))
    }
}

impl Write for DeadlineStream<'_> {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        self.until_ready(|stream| stream.write(buffer))
    }

    fn flush(&mut self) -> io::Result<()> {
        self.until_ready(TcpStream::flush)
    }
}

fn timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "renderer deadline elapsed")
}

fn map_protocol_error(_: ProtocolError) -> WorkerError {
    WorkerError::Protocol
}
