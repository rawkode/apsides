#![forbid(unsafe_code)]

//! Process-isolated loopback renderer supervision.
//!
//! This crate intentionally makes no claim of network isolation for evaluator
//! children. Its boundary is process lifecycle and a capability-scoped,
//! revalidated bundle store. Deployments that require network isolation must
//! impose it outside this process (for example, by the container runtime).

#[cfg(unix)]
use std::os::unix::{
    ffi::OsStrExt,
    fs::{FileTypeExt, MetadataExt},
    net::{UnixListener, UnixStream},
};
#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};
use std::{
    fs::OpenOptions,
    io::{self, Read, Write},
    net::{IpAddr, Ipv4Addr, Shutdown, SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use bundle_contract::{BundleStore, ExecutionPhaseV0, InstalledBundle};
use renderer_abi::sha256_hex;
use renderer_abi::{parse_validate_action_input, parse_validate_render_input};
use renderer_host::{HostError, Invocation, Limits, RendererHost};
use renderer_protocol::{
    ErrorCategoryV0, FrameV0, MAX_DURATION_MILLIS, PhaseV0, ProtocolError, RequestV0,
    ResponseStatusV0, ResponseV0, phase_cap, read_frame, write_frame,
};
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
    #[error("invalid unix socket path")]
    InvalidUnixPath,
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
    #[cfg(unix)]
    pub unix_socket: Option<PathBuf>,
    pub bundle_store: PathBuf,
    pub evaluator_binary: PathBuf,
}

impl WorkerConfig {
    #[must_use]
    pub fn new(bind: SocketAddr, bundle_store: PathBuf, evaluator_binary: PathBuf) -> Self {
        Self {
            bind,
            #[cfg(unix)]
            unix_socket: None,
            bundle_store,
            evaluator_binary,
        }
    }
}

/// A one-at-a-time loopback renderer supervisor.
pub struct Supervisor {
    listener: Listener,
    config: WorkerConfig,
    #[cfg(unix)]
    socket_path: Option<PathBuf>,
    #[cfg(unix)]
    socket_identity: Option<UnixSocketIdentity>,
}

enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
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
        Ok(Self {
            listener: Listener::Tcp(listener),
            config,
            #[cfg(unix)]
            socket_path: None,
            #[cfg(unix)]
            socket_identity: None,
        })
    }

    #[cfg(unix)]
    /// # Errors
    /// Returns [`WorkerError`] when the path is unsafe, stale ownership cannot be established, or binding fails.
    pub fn bind_unix(
        mut config: WorkerConfig,
        path: impl AsRef<Path>,
    ) -> Result<Self, WorkerError> {
        let path = path.as_ref();
        validate_unix_socket_path(path)?;
        remove_stale_unix_socket(path)?;
        let listener = UnixListener::bind(path)?;
        config.unix_socket = Some(path.to_owned());
        let identity = snapshot_unix_socket(path)?;
        Ok(Self {
            listener: Listener::Unix(listener),
            config,
            socket_path: Some(path.to_owned()),
            socket_identity: Some(identity),
        })
    }

    /// The concrete listener address, including an OS-selected port zero.
    ///
    /// # Errors
    ///
    /// Returns [`WorkerError::Io`] when the listener address cannot be read.
    pub fn local_addr(&self) -> Result<SocketAddr, WorkerError> {
        match &self.listener {
            Listener::Tcp(listener) => listener.local_addr().map_err(Into::into),
            #[cfg(unix)]
            Listener::Unix(_) => Err(WorkerError::Io),
        }
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
        let mut stream = match &self.listener {
            Listener::Tcp(listener) => {
                let (stream, peer) = listener.accept()?;
                if peer.ip() != IpAddr::V4(Ipv4Addr::LOCALHOST) {
                    return Err(WorkerError::Protocol);
                }
                Transport::Tcp(stream)
            }
            #[cfg(unix)]
            Listener::Unix(listener) => Transport::Unix(listener.accept()?.0),
        };
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

#[cfg(unix)]
impl Drop for Supervisor {
    fn drop(&mut self) {
        let (Some(path), Some(identity)) = (&self.socket_path, self.socket_identity) else {
            return;
        };
        let _ = quarantine_exact_unix_socket(path, identity, false);
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
enum Transport {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}
impl Read for Transport {
    fn read(&mut self, b: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.read(b),
            #[cfg(unix)]
            Self::Unix(s) => s.read(b),
        }
    }
}
impl Write for Transport {
    fn write(&mut self, b: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(s) => s.write(b),
            #[cfg(unix)]
            Self::Unix(s) => s.write(b),
        }
    }
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.flush(),
            #[cfg(unix)]
            Self::Unix(s) => s.flush(),
        }
    }
}
impl Transport {
    fn set_nonblocking(&self, v: bool) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.set_nonblocking(v),
            #[cfg(unix)]
            Self::Unix(s) => s.set_nonblocking(v),
        }
    }
    fn shutdown(&self, how: Shutdown) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.shutdown(how),
            #[cfg(unix)]
            Self::Unix(s) => s.shutdown(how),
        }
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnixSocketIdentity {
    dev: u64,
    ino: u64,
    uid: u32,
}

#[cfg(unix)]
fn socket_identity(meta: &std::fs::Metadata) -> Option<UnixSocketIdentity> {
    (meta.file_type().is_socket() && !meta.file_type().is_symlink()).then_some(UnixSocketIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
        uid: meta.uid(),
    })
}

#[cfg(unix)]
fn snapshot_unix_socket(path: &Path) -> Result<UnixSocketIdentity, WorkerError> {
    std::fs::symlink_metadata(path)
        .ok()
        .and_then(|meta| socket_identity(&meta))
        .ok_or(WorkerError::InvalidUnixPath)
}

#[cfg(unix)]
fn is_same_euid_socket(identity: UnixSocketIdentity) -> bool {
    identity.uid == rustix::process::geteuid().as_raw()
}

#[cfg(unix)]
static QUARANTINE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
type StaleSnapshotHook = (
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(test)]
type StaleSnapshotHookSlot = std::sync::OnceLock<std::sync::Mutex<Option<StaleSnapshotHook>>>;
#[cfg(test)]
static AFTER_STALE_SNAPSHOT_HOOK: StaleSnapshotHookSlot = std::sync::OnceLock::new();

#[cfg(test)]
fn run_after_stale_snapshot_hook() {
    let hook = AFTER_STALE_SNAPSHOT_HOOK.get_or_init(|| std::sync::Mutex::new(None));
    let guard = hook.lock().expect("stale snapshot hook lock");
    if let Some((ready, continue_bind)) = guard.as_ref() {
        ready.send(()).expect("stale snapshot hook receiver");
        continue_bind.recv().expect("stale snapshot hook sender");
    }
}

/// Reserve a private same-directory name before moving a stale socket there.
/// The reservation is deliberately `create_new`: an existing attacker-chosen
/// name is never reused. The caller owns the replacement of that empty
/// reservation by `rename`, and only unlinks after a fresh exact identity
/// check at the quarantine name.
#[cfg(unix)]
fn reserve_quarantine_name(path: &Path) -> Result<PathBuf, WorkerError> {
    let parent = path.parent().ok_or(WorkerError::InvalidUnixPath)?;
    let stem = path.file_name().ok_or(WorkerError::InvalidUnixPath)?;
    for _ in 0..32 {
        let sequence = QUARANTINE_COUNTER.fetch_add(1, Ordering::Relaxed);
        let candidate = parent.join(format!(
            ".{}.oxideloop-quarantine-{}-{}",
            stem.to_string_lossy(),
            std::process::id(),
            sequence
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(_) => return Ok(candidate),
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                return Err(WorkerError::Io);
            }
            Err(_) => {}
        }
    }
    Err(WorkerError::Io)
}

/// Move an exact socket out of its public name before deleting it. If another
/// object won the public name before the move, the post-rename identity check
/// fails and that object is restored when the name remains vacant; otherwise
/// it stays preserved under the unique quarantine name. In neither case do we
/// unlink the replacement.
#[cfg(unix)]
fn quarantine_exact_unix_socket(
    path: &Path,
    expected: UnixSocketIdentity,
    restore_on_mismatch: bool,
) -> Result<bool, WorkerError> {
    let Ok(current) = snapshot_unix_socket(path) else {
        return Ok(false);
    };
    if current != expected {
        return Ok(false);
    }
    #[cfg(test)]
    run_after_stale_snapshot_hook();
    let quarantine = reserve_quarantine_name(path)?;
    if std::fs::rename(path, &quarantine).is_err() {
        let _ = std::fs::remove_file(&quarantine);
        return Ok(false);
    }
    let quarantined = snapshot_unix_socket(&quarantine).ok();
    if quarantined == Some(expected) {
        std::fs::remove_file(&quarantine).map_err(|_| WorkerError::Io)?;
        return Ok(true);
    }
    if restore_on_mismatch && !path.exists() {
        // If a concurrent creator has already populated `path`, leave the
        // quarantined replacement intact rather than overwriting or deleting it.
        let _ = std::fs::rename(&quarantine, path);
    }
    Ok(false)
}

#[cfg(unix)]
fn remove_stale_unix_socket(path: &Path) -> Result<(), WorkerError> {
    let identity = match snapshot_unix_socket(path) {
        Ok(identity) => identity,
        Err(WorkerError::InvalidUnixPath) if !path.exists() => return Ok(()),
        Err(_) => return Err(WorkerError::InvalidUnixPath),
    };
    if !is_same_euid_socket(identity) {
        return Err(WorkerError::InvalidUnixPath);
    }
    if quarantine_exact_unix_socket(path, identity, true)? {
        Ok(())
    } else {
        Err(WorkerError::InvalidUnixPath)
    }
}

#[cfg(unix)]
fn validate_unix_socket_path(path: &Path) -> Result<(), WorkerError> {
    if !path.is_absolute()
        || path.as_os_str().is_empty()
        || path.as_os_str().as_bytes().contains(&0)
    {
        return Err(WorkerError::InvalidUnixPath);
    }
    if path.as_os_str().len() > 100 {
        return Err(WorkerError::InvalidUnixPath);
    }
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && socket_identity(&meta).is_none()
    {
        return Err(WorkerError::InvalidUnixPath);
    }
    Ok(())
}

struct DeadlineStream<'a> {
    stream: &'a mut Transport,
    deadline: Instant,
}

impl<'a> DeadlineStream<'a> {
    fn new(stream: &'a mut Transport, deadline: Instant) -> Self {
        Self { stream, deadline }
    }

    fn until_ready<T>(
        &mut self,
        operation: impl FnMut(&mut Transport) -> io::Result<T>,
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
        self.until_ready(Transport::flush)
    }
}

fn timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "renderer deadline elapsed")
}

fn map_protocol_error(_: ProtocolError) -> WorkerError {
    WorkerError::Protocol
}

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::{
        os::unix::{ffi::OsStringExt, fs::FileTypeExt, net::UnixListener},
        sync::mpsc,
    };

    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn config(directory: &Path) -> WorkerConfig {
        WorkerConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            directory.into(),
            directory.join("evaluator"),
        )
    }

    #[test]
    fn unix_bind_refuses_malformed_and_non_socket_paths() {
        let _test_lock = TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        for path in [
            PathBuf::from("relative.sock"),
            PathBuf::new(),
            PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0])),
            PathBuf::from(format!("/{}", "a".repeat(100))),
        ] {
            assert!(matches!(
                Supervisor::bind_unix(config(directory.path()), path),
                Err(WorkerError::InvalidUnixPath)
            ));
        }
        for kind in ["regular", "symlink", "directory"] {
            let path = directory.path().join(kind);
            match kind {
                "regular" => std::fs::write(&path, b"regular").unwrap(),
                "symlink" => std::os::unix::fs::symlink(directory.path(), &path).unwrap(),
                "directory" => std::fs::create_dir(&path).unwrap(),
                _ => unreachable!(),
            }
            assert!(matches!(
                Supervisor::bind_unix(config(directory.path()), &path),
                Err(WorkerError::InvalidUnixPath)
            ));
        }
    }

    #[test]
    fn same_euid_stale_socket_is_replaced_without_a_tcp_listener() {
        let _test_lock = TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        drop(UnixListener::bind(&path).unwrap());
        let supervisor = Supervisor::bind_unix(config(directory.path()), &path).unwrap();
        assert!(
            std::fs::symlink_metadata(&path)
                .unwrap()
                .file_type()
                .is_socket()
        );
        assert!(matches!(supervisor.local_addr(), Err(WorkerError::Io)));
        drop(supervisor);
        assert!(!path.exists());
    }

    #[test]
    fn foreign_owner_predicate_is_rejected() {
        let _test_lock = TEST_LOCK.lock().unwrap();
        let own = rustix::process::geteuid().as_raw();
        assert!(!is_same_euid_socket(UnixSocketIdentity {
            dev: 1,
            ino: 1,
            uid: own.wrapping_add(1)
        }));
    }

    #[test]
    fn quarantine_mismatch_restores_and_never_deletes_replacement() {
        let _test_lock = TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        let stale = UnixListener::bind(&path).unwrap();
        let identity = snapshot_unix_socket(&path).unwrap();
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (continue_tx, continue_rx) = mpsc::sync_channel(1);
        *AFTER_STALE_SNAPSHOT_HOOK
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap() = Some((ready_tx, continue_rx));
        let worker_path = path.clone();
        let worker =
            thread::spawn(move || quarantine_exact_unix_socket(&worker_path, identity, true));
        ready_rx.recv().unwrap();
        drop(stale);
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        continue_tx.send(()).unwrap();
        assert!(!worker.join().unwrap().unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        *AFTER_STALE_SNAPSHOT_HOOK.get().unwrap().lock().unwrap() = None;
    }

    #[test]
    fn drop_preserves_a_replacement_socket_inode() {
        let _test_lock = TEST_LOCK.lock().unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        let supervisor = Supervisor::bind_unix(config(directory.path()), &path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let replacement = UnixListener::bind(&path).unwrap();
        let replacement_inode = std::fs::symlink_metadata(&path).unwrap().ino();
        drop(supervisor);
        assert_eq!(
            std::fs::symlink_metadata(&path).unwrap().ino(),
            replacement_inode
        );
        drop(replacement);
    }
}
