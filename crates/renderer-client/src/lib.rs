#![forbid(unsafe_code)]

//! A deliberately serial, one-frame client for the loopback renderer worker.
//!
//! A mutable client permits exactly one outstanding call.  It always binds a
//! response to the submitted request, and treats disconnects and malformed or
//! mismatched responses as renderer loss rather than an application result.

use std::{
    io::{self, Read, Write},
    net::{Shutdown, SocketAddr, TcpStream},
    thread,
    time::{Duration, Instant},
};
#[cfg(unix)]
use std::{
    os::unix::{
        ffi::OsStrExt,
        fs::{FileTypeExt, MetadataExt},
    },
    path::{Path, PathBuf},
};

use oxideloop_renderer_protocol::{
    FrameV0, ProtocolError, RequestV0, ResponseStatusV0, ResponseV0, read_frame, write_frame,
};
#[cfg(unix)]
use socket2::{Domain, SockAddr, Socket, Type};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RendererClientError {
    #[error("renderer is unavailable")]
    RendererLost,
    #[error("renderer request is invalid")]
    InvalidRequest,
    #[error("renderer address must be exactly 127.0.0.1")]
    NonLoopbackAddress,
    #[error("renderer unix socket path is invalid")]
    InvalidUnixPath,
}

#[derive(Debug)]
enum Transport {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(Socket),
}
impl Transport {
    fn set_nonblocking(&self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.set_nonblocking(true),
            #[cfg(unix)]
            Self::Unix(s) => s.set_nonblocking(true),
        }
    }
    fn shutdown(&self) -> io::Result<()> {
        match self {
            Self::Tcp(s) => s.shutdown(Shutdown::Write),
            #[cfg(unix)]
            Self::Unix(s) => s.shutdown(Shutdown::Write),
        }
    }
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

#[cfg(unix)]
fn validate_unix_path(path: &Path) -> Result<PathBuf, RendererClientError> {
    if !path.is_absolute()
        || path.as_os_str().is_empty()
        || path.as_os_str().len() > 100
        || path.as_os_str().as_bytes().contains(&0)
    {
        return Err(RendererClientError::InvalidUnixPath);
    }
    if let Ok(meta) = std::fs::symlink_metadata(path)
        && (meta.file_type().is_symlink() || !meta.file_type().is_socket())
    {
        return Err(RendererClientError::InvalidUnixPath);
    }
    Ok(path.to_owned())
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct UnixIdentity {
    dev: u64,
    ino: u64,
    file_type: u32,
}
#[cfg(unix)]
fn unix_identity(meta: &std::fs::Metadata) -> UnixIdentity {
    UnixIdentity {
        dev: meta.dev(),
        ino: meta.ino(),
        file_type: meta.mode() & libc_file_type_mask(),
    }
}

// `S_IFMT`. Keeping this local avoids accepting permissions as identity while
// still requiring the filesystem object type to remain exact.
#[cfg(unix)]
const fn libc_file_type_mask() -> u32 {
    0o170_000
}

#[cfg(unix)]
#[derive(Clone, Debug)]
struct UnixPathSnapshot {
    endpoint: UnixIdentity,
    parents: Vec<(PathBuf, UnixIdentity)>,
}

#[cfg(unix)]
fn snapshot_unix_path(path: &Path) -> Result<UnixPathSnapshot, RendererClientError> {
    validate_unix_path(path)?;
    let meta = std::fs::symlink_metadata(path).map_err(|_| RendererClientError::InvalidUnixPath)?;
    if meta.file_type().is_symlink() || !meta.file_type().is_socket() {
        return Err(RendererClientError::InvalidUnixPath);
    }
    let mut parents = Vec::new();
    let mut parent = path.parent();
    while let Some(component) = parent {
        let meta = std::fs::symlink_metadata(component)
            .map_err(|_| RendererClientError::InvalidUnixPath)?;
        if meta.file_type().is_symlink() || !meta.file_type().is_dir() {
            return Err(RendererClientError::InvalidUnixPath);
        }
        parents.push((component.to_owned(), unix_identity(&meta)));
        parent = component.parent();
    }
    Ok(UnixPathSnapshot {
        endpoint: unix_identity(&meta),
        parents,
    })
}
#[cfg(unix)]
fn snapshot_matches(path: &Path, snapshot: &UnixPathSnapshot) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|meta| {
        meta.file_type().is_socket()
            && !meta.file_type().is_symlink()
            && unix_identity(&meta) == snapshot.endpoint
    }) && snapshot.parents.iter().all(|(parent, identity)| {
        std::fs::symlink_metadata(parent).is_ok_and(|meta| {
            meta.file_type().is_dir()
                && !meta.file_type().is_symlink()
                && unix_identity(&meta) == *identity
        })
    })
}

#[cfg(test)]
type UnixConnectHook = (
    std::sync::mpsc::SyncSender<()>,
    std::sync::mpsc::Receiver<()>,
);
#[cfg(test)]
type UnixConnectHookSlot = std::sync::OnceLock<std::sync::Mutex<Option<UnixConnectHook>>>;
#[cfg(test)]
static AFTER_UNIX_CONNECT_HOOK: UnixConnectHookSlot = std::sync::OnceLock::new();

#[cfg(test)]
fn run_after_unix_connect_hook() {
    let hook = AFTER_UNIX_CONNECT_HOOK.get_or_init(|| std::sync::Mutex::new(None));
    let guard = hook.lock().expect("unix connect hook lock");
    if let Some((ready, continue_call)) = guard.as_ref() {
        ready.send(()).expect("unix connect hook receiver");
        continue_call.recv().expect("unix connect hook sender");
    }
}

/// A client bound to one explicitly configured renderer loopback endpoint.
#[derive(Debug)]
pub struct RendererClient {
    endpoint: Endpoint,
}

#[derive(Debug, Clone)]
enum Endpoint {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(PathBuf),
}

impl RendererClient {
    /// Construct a client only for the worker's explicit loopback endpoint.
    ///
    /// # Errors
    ///
    /// Returns [`RendererClientError::NonLoopbackAddress`] unless `address` is
    /// exactly IPv4 `127.0.0.1`.
    pub fn new(address: SocketAddr) -> Result<Self, RendererClientError> {
        validate_loopback_address(address)?;
        Ok(Self {
            endpoint: Endpoint::Tcp(address),
        })
    }

    #[must_use]
    pub fn tcp_address(&self) -> Option<SocketAddr> {
        match self.endpoint {
            Endpoint::Tcp(address) => Some(address),
            #[cfg(unix)]
            Endpoint::Unix(_) => None,
        }
    }

    #[cfg(unix)]
    /// Construct a Unix-domain client for an absolute filesystem socket path.
    ///
    /// Calls snapshot the socket and every parent directory before connecting,
    /// then require the exact same objects after the connection and before any
    /// request byte is written. This v0 check depends on those directories being
    /// trusted against hostile rename/unlink activity: it detects a replacement
    /// visible at either observation, but cannot prove that an attacker did not
    /// replace and restore an inode between observations (the filesystem ABA
    /// boundary), nor authenticate the process behind an otherwise stable socket.
    ///
    /// # Errors
    /// Returns [`RendererClientError::InvalidUnixPath`] for malformed paths.
    pub fn new_unix(path: impl AsRef<Path>) -> Result<Self, RendererClientError> {
        let path = validate_unix_path(path.as_ref())?;
        Ok(Self {
            endpoint: Endpoint::Unix(path),
        })
    }

    /// Submit one request and wait for its exactly matching response.
    ///
    /// The `&mut self` receiver is intentional: controller code cannot issue a
    /// second request through this client while the first owns the transport.
    ///
    /// # Errors
    ///
    /// Returns [`RendererClientError::InvalidRequest`] before connecting for an
    /// invalid protocol request. All transport, timeout, framing, and identity
    /// failures are conservatively surfaced as renderer loss.
    pub fn call(&mut self, request: &RequestV0) -> Result<ResponseV0, RendererClientError> {
        request
            .validate()
            .map_err(|_| RendererClientError::InvalidRequest)?;
        #[cfg(unix)]
        let unix_snapshot = if let Endpoint::Unix(path) = &self.endpoint {
            Some(snapshot_unix_path(path).map_err(|_| RendererClientError::RendererLost)?)
        } else {
            None
        };
        let deadline = Instant::now() + Duration::from_millis(u64::from(request.duration_millis));
        let mut stream = match &self.endpoint {
            Endpoint::Tcp(address) => Transport::Tcp(
                TcpStream::connect_timeout(address, remaining(deadline)?)
                    .map_err(|_| RendererClientError::RendererLost)?,
            ),
            #[cfg(unix)]
            Endpoint::Unix(path) => {
                let socket = Socket::new(Domain::UNIX, Type::STREAM, None)
                    .map_err(|_| RendererClientError::RendererLost)?;
                let address =
                    SockAddr::unix(path).map_err(|_| RendererClientError::RendererLost)?;
                socket
                    .connect_timeout(&address, remaining(deadline)?)
                    .map_err(|_| RendererClientError::RendererLost)?;
                #[cfg(test)]
                run_after_unix_connect_hook();
                if !snapshot_matches(
                    path,
                    unix_snapshot
                        .as_ref()
                        .ok_or(RendererClientError::RendererLost)?,
                ) {
                    return Err(RendererClientError::RendererLost);
                }
                Transport::Unix(socket)
            }
        };
        stream
            .set_nonblocking()
            .map_err(|_| RendererClientError::RendererLost)?;
        write_frame(
            DeadlineStream::new(&mut stream, deadline),
            &FrameV0::Request(request.clone()),
        )
        .map_err(map_protocol_failure)?;
        stream
            .shutdown()
            .map_err(|_| RendererClientError::RendererLost)?;
        let frame =
            read_frame(DeadlineStream::new(&mut stream, deadline)).map_err(map_protocol_failure)?;
        let FrameV0::Response(response) = frame else {
            return Err(RendererClientError::RendererLost);
        };
        response.matches(request).map_err(map_protocol_failure)?;
        match response.status {
            ResponseStatusV0::Ok | ResponseStatusV0::Recoverable => Ok(response),
            ResponseStatusV0::Fatal | ResponseStatusV0::Protocol => {
                Err(RendererClientError::RendererLost)
            }
        }
    }
}

fn validate_loopback_address(address: SocketAddr) -> Result<(), RendererClientError> {
    match address {
        SocketAddr::V4(value)
            if value.ip().is_loopback() && value.ip().octets() == [127, 0, 0, 1] =>
        {
            Ok(())
        }
        SocketAddr::V4(_) | SocketAddr::V6(_) => Err(RendererClientError::NonLoopbackAddress),
    }
}

fn remaining(deadline: Instant) -> Result<Duration, RendererClientError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|duration| !duration.is_zero())
        .ok_or(RendererClientError::RendererLost)
}

fn map_protocol_failure(_: ProtocolError) -> RendererClientError {
    RendererClientError::RendererLost
}

/// Applies the remaining duration before every read and write. This prevents a
/// slow peer from extending a request indefinitely by making partial progress.
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

#[cfg(all(test, unix))]
mod unix_tests {
    use super::*;
    use std::{os::unix::net::UnixListener, sync::mpsc, thread, time::Duration};

    const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn request() -> RequestV0 {
        RequestV0 {
            request_id: oxideloop_renderer_protocol::RequestIdV0 { bytes: [7; 16] },
            phase: oxideloop_renderer_protocol::PhaseV0::Render,
            bundle_digest: DIGEST.into(),
            module_digest: Some(DIGEST.into()),
            duration_millis: 250,
            payload: b"{}".to_vec(),
        }
    }

    #[test]
    fn replacement_after_connect_is_lost_before_any_request_reaches_substitute() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("renderer.sock");
        let original = UnixListener::bind(&path).unwrap();
        let original_server = thread::spawn(move || {
            let (mut stream, _) = original.accept().unwrap();
            let mut byte = [0_u8; 1];
            assert_eq!(
                stream.read(&mut byte).unwrap(),
                0,
                "original got request bytes"
            );
        });
        let (ready_tx, ready_rx) = mpsc::sync_channel(1);
        let (continue_tx, continue_rx) = mpsc::sync_channel(1);
        *AFTER_UNIX_CONNECT_HOOK
            .get_or_init(|| std::sync::Mutex::new(None))
            .lock()
            .unwrap() = Some((ready_tx, continue_rx));

        let client_path = path.clone();
        let caller = thread::spawn(move || {
            let mut client = RendererClient::new_unix(client_path).unwrap();
            client.call(&request())
        });
        ready_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        std::fs::remove_file(&path).unwrap();
        let substitute = UnixListener::bind(&path).unwrap();
        continue_tx.send(()).unwrap();
        assert!(matches!(
            caller.join().unwrap(),
            Err(RendererClientError::RendererLost)
        ));
        substitute.set_nonblocking(true).unwrap();
        assert!(
            matches!(substitute.accept(), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        original_server.join().unwrap();
        *AFTER_UNIX_CONNECT_HOOK.get().unwrap().lock().unwrap() = None;
    }
}
