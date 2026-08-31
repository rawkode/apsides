//! Debug-only controller failpoint arming over a private Unix socket.
//!
//! The parent directory is opened one component at a time with `O_NOFOLLOW`
//! and retained as a descriptor. Cleanup is consequently anchored to the
//! directory that was checked during creation, not to a later pathname lookup.
//! POSIX pathname Unix sockets cannot make a concurrent same-UID rename of the
//! configured private parent impossible between pathname `bind` and a client
//! `connect`. The supported trust boundary is therefore a caller-controlled,
//! same-owner, exact-0700 parent with no concurrently malicious same-UID
//! process. We re-check the pathname before binding and verify the bound
//! socket through the retained parent descriptor; on mismatch we never accept
//! from it and never unlink a replacement.
//!
//! This module deliberately stops at the boundary between receiving an arm
//! request and the runtime's existing [`crate::FailpointsV0`] state. It does
//! not decide when a process exits; that is owned by the controller wiring.
//!
//! The wire format is one UTF-8-free ASCII line: exactly
//! `AfterEventBeforeStatus\n`, `AfterStatusBeforeAck\n`, or
//! `AfterStateFinalizerBeforeRootFinalizer\n`. The deletion request retains
//! its one connected client and uses `REACHED\n` / `RELEASE\n` as a bounded
//! in-band handshake; it never opens a TCP port or another listener.

use std::{
    ffi::{OsStr, OsString},
    io::{self, Read, Write},
    os::{
        fd::{AsFd, OwnedFd},
        unix::{
            ffi::OsStrExt,
            net::{UnixListener, UnixStream},
        },
    },
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};

use rustix::{
    event::{PollFd, PollFlags, Timespec},
    fs::{self as rustix_fs, AtFlags, FileType, Mode, OFlags},
    io::Errno,
};
use thiserror::Error;

use crate::{FailpointV0, FailpointsV0};

/// The largest accepted request, including its terminating newline.
pub const MAX_REQUEST_BYTES: usize = 64;
const SOCKET_MODE: u32 = 0o600;
const PARENT_MODE: u32 = 0o700;
const READ_TOTAL_DEADLINE: Duration = Duration::from_millis(100);
// Fixed and total: e2e has time to record raw midpoint evidence, but neither
// a client nor the environment can extend the controller's blocked window.
const RELEASE_TOTAL_DEADLINE: Duration = Duration::from_secs(60);
const MAX_SOCKET_PATH_BYTES: usize = 100;

/// Errors from validating or servicing the debug-only arming socket.
#[derive(Debug, Error)]
pub enum FailpointUdsError {
    #[error("failpoint socket path is invalid")]
    InvalidPath,
    #[error("failpoint socket parent is not a private directory")]
    ParentNotPrivate,
    #[error("failpoint socket parent is not owned by the current user")]
    ParentOwner,
    #[error("failpoint socket parent could not be inspected")]
    ParentMetadata(#[source] io::Error),
    #[error("failpoint socket path already exists")]
    ExistingPath,
    #[error("failpoint socket path could not be created")]
    Bind(#[source] io::Error),
    #[error("failpoint socket could not be configured")]
    Configure(#[source] io::Error),
    #[error("failpoint socket request is malformed")]
    MalformedRequest,
    #[error("failpoint socket request is too large")]
    OversizeRequest,
    #[error("failpoint socket request exceeded its total deadline")]
    RequestDeadlineExceeded,
    #[error("failpoint socket request could not be read")]
    Read(#[source] io::Error),
    #[error("a failpoint socket arm has already been consumed")]
    AlreadyConsumed,
    #[error("a failpoint is already armed")]
    AlreadyArmed,
    #[error("deletion barrier release is malformed")]
    MalformedRelease,
    #[error("deletion barrier release exceeded its total deadline")]
    ReleaseDeadlineExceeded,
}

/// One accepted debug control request. This is deliberately only compiled
/// with the debug-only failpoint feature.
#[derive(Debug)]
pub enum FailpointControlV0 {
    Arm(ActionArmV0),
    DeletionBarrier(DeletionBarrierV0),
}

pub struct ActionArmV0 {
    point: FailpointV0,
    stream: UnixStream,
}

impl std::fmt::Debug for ActionArmV0 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActionArmV0")
            .finish_non_exhaustive()
    }
}

impl ActionArmV0 {
    #[must_use]
    pub fn point(&self) -> FailpointV0 {
        self.point
    }

    pub fn acknowledge(mut self) -> Result<FailpointV0, FailpointUdsError> {
        match self.stream.write_all(b"ARMED\n") {
            Ok(()) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::BrokenPipe | io::ErrorKind::ConnectionReset
                ) => {}
            Err(error) => return Err(FailpointUdsError::Read(error)),
        }
        Ok(self.point)
    }
}

/// The one-shot connected peer retained for the deletion midpoint handshake.
pub struct DeletionBarrierV0 {
    stream: UnixStream,
}

impl std::fmt::Debug for DeletionBarrierV0 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("DeletionBarrierV0")
            .finish_non_exhaustive()
    }
}

impl DeletionBarrierV0 {
    /// Confirm that the controller consumed the deletion barrier request.
    /// This is distinct from `REACHED`, which proves the later exact effect.
    pub fn acknowledge(&mut self) -> Result<(), FailpointUdsError> {
        self.stream
            .write_all(b"ARMED\n")
            .map_err(FailpointUdsError::Read)
    }

    /// Signal the verified midpoint and wait for the exact bounded release.
    pub fn reached_and_wait_for_release(self) -> Result<(), FailpointUdsError> {
        self.reached_and_wait_for_release_with_deadline(RELEASE_TOTAL_DEADLINE)
    }

    /// Like [`Self::reached_and_wait_for_release`], with an explicit bounded
    /// deadline for deterministic e2e and adversarial tests.
    pub fn reached_and_wait_for_release_with_deadline(
        mut self,
        deadline: Duration,
    ) -> Result<(), FailpointUdsError> {
        self.stream
            .write_all(b"REACHED\n")
            .map_err(FailpointUdsError::Read)?;
        read_exact_line(&mut self.stream, deadline, b"RELEASE", LineKindV0::Release)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

fn identity(stat: &rustix_fs::Stat) -> FileIdentity {
    FileIdentity {
        device: u64::try_from(stat.st_dev).unwrap_or_default(),
        inode: stat.st_ino,
    }
}

fn exact_mode(stat: &rustix_fs::Stat, mode: u32) -> bool {
    u32::from(stat.st_mode) & 0o7777 == mode
}

fn trusted_ancestor_layout(mode: u32, owner_uid: u32, current_euid: u32) -> bool {
    let writable_by_group_or_other = mode & 0o022 != 0;
    let sticky = mode & 0o1000 != 0;
    !writable_by_group_or_other || (sticky && (owner_uid == 0 || owner_uid == current_euid))
}

fn parent_error(error: Errno) -> FailpointUdsError {
    FailpointUdsError::ParentMetadata(error.into())
}

fn configure_error(error: Errno) -> FailpointUdsError {
    FailpointUdsError::Configure(error.into())
}

fn directory_open_flags() -> OFlags {
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

fn open_directory_at(parent: impl AsFd, name: &OsStr) -> Result<OwnedFd, Errno> {
    rustix_fs::openat(parent, name, directory_open_flags(), Mode::empty())
}

fn validate_ancestor(
    directory: impl AsFd,
    is_final_parent: bool,
) -> Result<FileIdentity, FailpointUdsError> {
    let stat = rustix_fs::fstat(directory).map_err(parent_error)?;
    if !FileType::from_raw_mode(stat.st_mode).is_dir() {
        return Err(FailpointUdsError::ParentNotPrivate);
    }

    if is_final_parent {
        if !exact_mode(&stat, PARENT_MODE) {
            return Err(FailpointUdsError::ParentNotPrivate);
        }
        if stat.st_uid != rustix::process::geteuid().as_raw() {
            return Err(FailpointUdsError::ParentOwner);
        }
    } else {
        let mode = u32::from(stat.st_mode);
        // A sticky system ancestor (for example /tmp) cannot remove a child
        // owned by another UID. A sticky directory owned by an unrelated UID
        // is not a trusted system boundary; non-sticky group/world writable
        // ancestors can redirect this directory layout and are also refused.
        if !trusted_ancestor_layout(mode, stat.st_uid, rustix::process::geteuid().as_raw()) {
            return Err(FailpointUdsError::ParentNotPrivate);
        }
    }
    Ok(identity(&stat))
}

fn open_or_create_directory(parent: impl AsFd, name: &OsStr) -> Result<OwnedFd, FailpointUdsError> {
    match open_directory_at(parent.as_fd(), name) {
        Ok(directory) => Ok(directory),
        Err(Errno::NOENT) => {
            match rustix_fs::mkdirat(parent.as_fd(), name, Mode::RWXU) {
                Ok(()) | Err(Errno::EXIST) => {}
                Err(error) => return Err(parent_error(error)),
            }
            let directory = open_directory_at(parent, name).map_err(parent_error)?;
            // `mkdirat(..., 0700)` never creates a transient public parent;
            // fchmod also makes the final mode exact when the caller's umask
            // is more restrictive.
            rustix_fs::fchmod(&directory, Mode::RWXU).map_err(parent_error)?;
            Ok(directory)
        }
        Err(error) => Err(parent_error(error)),
    }
}

fn parent_components(parent: &Path) -> Result<Vec<OsString>, FailpointUdsError> {
    if !parent.is_absolute() {
        return Err(FailpointUdsError::InvalidPath);
    }
    parent
        .components()
        .map(|component| match component {
            Component::RootDir => Ok(None),
            Component::Normal(component) => Ok(Some(component.to_os_string())),
            Component::CurDir | Component::ParentDir | Component::Prefix(_) => {
                Err(FailpointUdsError::InvalidPath)
            }
        })
        .filter_map(Result::transpose)
        .collect()
}

fn traverse_parent(
    parent: &Path,
    create_missing: bool,
) -> Result<(OwnedFd, FileIdentity), FailpointUdsError> {
    let components = parent_components(parent)?;
    let mut current = rustix_fs::openat(
        rustix_fs::ABS,
        Path::new("/"),
        directory_open_flags(),
        Mode::empty(),
    )
    .map_err(parent_error)?;

    if components.is_empty() {
        let identity = validate_ancestor(&current, true)?;
        return Ok((current, identity));
    }
    validate_ancestor(&current, false)?;

    for (index, component) in components.iter().enumerate() {
        let is_final_parent = index + 1 == components.len();
        let next = if create_missing {
            open_or_create_directory(&current, component)?
        } else {
            open_directory_at(&current, component).map_err(parent_error)?
        };
        validate_ancestor(&next, is_final_parent)?;
        current = next;
    }

    let identity = validate_ancestor(&current, true)?;
    Ok((current, identity))
}

fn validate_socket_path(path: &Path) -> Result<(PathBuf, OsString), FailpointUdsError> {
    let path_bytes = path.as_os_str().as_bytes();
    if !path.is_absolute()
        || path_bytes.is_empty()
        || path_bytes.len() > MAX_SOCKET_PATH_BYTES
        || path_bytes.contains(&0)
        || path_bytes.ends_with(b"/")
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(FailpointUdsError::InvalidPath);
    }
    let parent = path.parent().ok_or(FailpointUdsError::InvalidPath)?;
    let name = path.file_name().ok_or(FailpointUdsError::InvalidPath)?;
    if !matches!(
        Path::new(name).components().next(),
        Some(Component::Normal(_))
    ) {
        return Err(FailpointUdsError::InvalidPath);
    }
    Ok((parent.to_path_buf(), name.to_os_string()))
}

struct PrivateParent {
    directory: OwnedFd,
    identity: FileIdentity,
    pathname: PathBuf,
}

impl PrivateParent {
    fn open_or_create(pathname: PathBuf) -> Result<Self, FailpointUdsError> {
        let (directory, identity) = traverse_parent(&pathname, true)?;
        Ok(Self {
            directory,
            identity,
            pathname,
        })
    }

    fn assert_current_path_is_held_directory(&self) -> Result<(), FailpointUdsError> {
        let (_, current_identity) = traverse_parent(&self.pathname, false)?;
        if current_identity != self.identity {
            return Err(FailpointUdsError::ParentNotPrivate);
        }
        Ok(())
    }

    fn entry_stat(&self, name: &OsStr) -> Result<Option<rustix_fs::Stat>, FailpointUdsError> {
        match rustix_fs::statat(&self.directory, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(stat)),
            Err(Errno::NOENT) => Ok(None),
            Err(error) => Err(configure_error(error)),
        }
    }
}

struct SocketCreation {
    parent: PrivateParent,
    name: OsString,
    identity: FileIdentity,
}

impl SocketCreation {
    fn remove_if_original(&self) {
        let original_is_still_present = self
            .parent
            .entry_stat(&self.name)
            .is_ok_and(|stat| stat.is_some_and(|stat| identity(&stat) == self.identity));
        if original_is_still_present {
            let _ = rustix_fs::unlinkat(&self.parent.directory, &self.name, AtFlags::empty());
        }
    }
}

fn bound_socket_identity(
    parent: &PrivateParent,
    name: &OsStr,
) -> Result<FileIdentity, FailpointUdsError> {
    let current = parent.entry_stat(name)?.ok_or_else(|| {
        FailpointUdsError::Configure(io::Error::new(
            io::ErrorKind::NotFound,
            "bound socket disappeared",
        ))
    })?;
    if !FileType::from_raw_mode(current.st_mode).is_socket()
        || current.st_uid != rustix::process::geteuid().as_raw()
    {
        return Err(FailpointUdsError::Configure(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "bound socket does not match the retained private parent",
        )));
    }
    Ok(identity(&current))
}

fn configure_bound_socket(
    parent: &PrivateParent,
    name: &OsStr,
    socket_identity: FileIdentity,
) -> Result<(), FailpointUdsError> {
    // Unix sockets cannot be chmod'd by their descriptor on Linux. chmodat is
    // anchored to the retained parent; the identity checks before and after it
    // prevent us from treating a substituted final entry as ours.
    rustix_fs::chmodat(
        &parent.directory,
        name,
        Mode::RUSR | Mode::WUSR,
        AtFlags::empty(),
    )
    .map_err(configure_error)?;

    let current = parent.entry_stat(name)?.ok_or_else(|| {
        FailpointUdsError::Configure(io::Error::new(
            io::ErrorKind::NotFound,
            "bound socket disappeared",
        ))
    })?;
    if !FileType::from_raw_mode(current.st_mode).is_socket()
        || !exact_mode(&current, SOCKET_MODE)
        || current.st_uid != rustix::process::geteuid().as_raw()
        || identity(&current) != socket_identity
    {
        return Err(FailpointUdsError::Configure(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "failpoint socket metadata is not private",
        )));
    }
    Ok(())
}

/// A nonblocking, one-shot Unix socket listener for debug failpoint arms.
///
/// The socket is created only when the `test-failpoints` feature is enabled,
/// and that feature is rejected by the crate in release builds.
pub struct FailpointUdsListenerV0 {
    listener: Option<UnixListener>,
    socket_path: PathBuf,
    creation: SocketCreation,
}

impl std::fmt::Debug for FailpointUdsListenerV0 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FailpointUdsListenerV0")
            .field("socket_path", &self.socket_path)
            .field("consumed", &self.listener.is_none())
            .finish_non_exhaustive()
    }
}

impl FailpointUdsListenerV0 {
    /// Create the private parent and bind a fresh 0600 Unix socket.
    pub fn bind(path: impl Into<PathBuf>) -> Result<Self, FailpointUdsError> {
        let socket_path = path.into();
        let (parent_path, socket_name) = validate_socket_path(&socket_path)?;
        let parent = PrivateParent::open_or_create(parent_path)?;
        if parent.entry_stat(&socket_name)?.is_some() {
            return Err(FailpointUdsError::ExistingPath);
        }

        // `UnixListener::bind` only accepts a pathname. Revalidating that it
        // still resolves to our descriptor catches a replacement before bind;
        // the same-UID trust boundary is documented at this module's top.
        parent.assert_current_path_is_held_directory()?;
        let listener = UnixListener::bind(&socket_path).map_err(FailpointUdsError::Bind)?;
        let socket_identity = match bound_socket_identity(&parent, &socket_name) {
            Ok(identity) => identity,
            Err(error) => {
                drop(listener);
                return Err(error);
            }
        };
        let creation = SocketCreation {
            parent,
            name: socket_name,
            identity: socket_identity,
        };
        if let Err(error) =
            configure_bound_socket(&creation.parent, &creation.name, socket_identity)
        {
            drop(listener);
            creation.remove_if_original();
            return Err(error);
        }
        if let Err(error) = listener.set_nonblocking(true) {
            drop(listener);
            creation.remove_if_original();
            return Err(FailpointUdsError::Configure(error));
        }

        Ok(Self {
            listener: Some(listener),
            socket_path,
            creation,
        })
    }

    #[must_use]
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    #[must_use]
    pub fn is_consumed(&self) -> bool {
        self.listener.is_none()
    }

    /// Poll for one complete request. A client which reaches the request
    /// parser consumes this one-shot listener whether its request is accepted
    /// or rejected, so malformed input cannot leave a dangling debug socket.
    pub fn poll(&mut self) -> Result<Option<FailpointV0>, FailpointUdsError> {
        match self.poll_control()? {
            Some(FailpointControlV0::Arm(arm)) => Ok(Some(arm.acknowledge()?)),
            Some(FailpointControlV0::DeletionBarrier(_)) => {
                Err(FailpointUdsError::MalformedRequest)
            }
            None => Ok(None),
        }
    }

    /// Poll one complete request, retaining the deletion client only for its
    /// explicit midpoint handshake. Any accepted client consumes this socket.
    pub fn poll_control(&mut self) -> Result<Option<FailpointControlV0>, FailpointUdsError> {
        let listener = self
            .listener
            .as_ref()
            .ok_or(FailpointUdsError::AlreadyConsumed)?;
        let (stream, _) = match listener.accept() {
            Ok(connection) => connection,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(FailpointUdsError::Read(error)),
        };
        let point = read_request(stream);
        self.consume();
        point.map(Some)
    }

    /// Poll and atomically place the accepted arm into the existing runtime
    /// state. The listener is consumed once it accepts a client, including if
    /// that state already has an arm.
    pub fn arm_into(
        &mut self,
        state: &mut FailpointsV0,
    ) -> Result<Option<FailpointV0>, FailpointUdsError> {
        let Some(point) = self.poll()? else {
            return Ok(None);
        };
        state
            .arm(point)
            .map_err(|_| FailpointUdsError::AlreadyArmed)?;
        Ok(Some(point))
    }

    fn consume(&mut self) {
        self.listener.take();
        self.creation.remove_if_original();
    }
}

impl Drop for FailpointUdsListenerV0 {
    fn drop(&mut self) {
        self.consume();
    }
}

fn timeout_timespec(duration: Duration) -> Timespec {
    Timespec {
        tv_sec: duration.as_secs().try_into().unwrap_or(i64::MAX),
        tv_nsec: duration.subsec_nanos().into(),
    }
}

fn wait_for_input(stream: &UnixStream, deadline: Instant) -> Result<(), FailpointUdsError> {
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FailpointUdsError::RequestDeadlineExceeded);
        }
        let timeout = timeout_timespec(remaining);
        let mut fds = [PollFd::new(stream, PollFlags::IN)];
        match rustix::event::poll(&mut fds, Some(&timeout)) {
            Ok(0) => return Err(FailpointUdsError::RequestDeadlineExceeded),
            Ok(_) => return Ok(()),
            Err(Errno::INTR) => {}
            Err(error) => return Err(FailpointUdsError::Read(error.into())),
        }
    }
}

fn read_request(mut stream: UnixStream) -> Result<FailpointControlV0, FailpointUdsError> {
    stream
        .set_nonblocking(true)
        .map_err(FailpointUdsError::Read)?;
    let request = read_line(&mut stream, READ_TOTAL_DEADLINE, LineKindV0::Request)?;
    match request.as_slice() {
        b"AfterEventBeforeStatus" => Ok(FailpointControlV0::Arm(ActionArmV0 {
            point: FailpointV0::AfterEventBeforeStatus,
            stream,
        })),
        b"AfterStatusBeforeAck" => Ok(FailpointControlV0::Arm(ActionArmV0 {
            point: FailpointV0::AfterStatusBeforeAck,
            stream,
        })),
        b"AfterStateFinalizerBeforeRootFinalizer" => {
            Ok(FailpointControlV0::DeletionBarrier(DeletionBarrierV0 {
                stream,
            }))
        }
        _ => Err(FailpointUdsError::MalformedRequest),
    }
}

#[derive(Clone, Copy)]
enum LineKindV0 {
    Request,
    Release,
}

impl LineKindV0 {
    fn malformed(self) -> FailpointUdsError {
        match self {
            Self::Request => FailpointUdsError::MalformedRequest,
            Self::Release => FailpointUdsError::MalformedRelease,
        }
    }

    fn deadline(self) -> FailpointUdsError {
        match self {
            Self::Request => FailpointUdsError::RequestDeadlineExceeded,
            Self::Release => FailpointUdsError::ReleaseDeadlineExceeded,
        }
    }
}

fn read_line(
    stream: &mut UnixStream,
    total_deadline: Duration,
    kind: LineKindV0,
) -> Result<Vec<u8>, FailpointUdsError> {
    let deadline = Instant::now() + total_deadline;
    let mut request = Vec::with_capacity(MAX_REQUEST_BYTES);
    let mut byte = [0_u8; 1];
    loop {
        match stream.read(&mut byte) {
            Ok(0) => return Err(kind.malformed()),
            Ok(1) if byte[0] == b'\n' => break,
            Ok(1) => {
                request.push(byte[0]);
                if request.len() >= MAX_REQUEST_BYTES {
                    return Err(FailpointUdsError::OversizeRequest);
                }
            }
            Ok(_) => unreachable!("one-byte read cannot return more than one byte"),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                match wait_for_input(stream, deadline) {
                    Err(FailpointUdsError::RequestDeadlineExceeded) => {
                        return Err(kind.deadline());
                    }
                    Err(error) => return Err(error),
                    Ok(()) => {}
                }
            }
            Err(error) => return Err(FailpointUdsError::Read(error)),
        }
    }

    // A second byte already queued makes this more than the one fixed-size
    // line. This check never blocks and therefore cannot reset the total
    // request deadline.
    let mut extra = [0_u8; 1];
    match stream.read(&mut extra) {
        Ok(0) => {}
        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
        Ok(_) => return Err(kind.malformed()),
        Err(error) => return Err(FailpointUdsError::Read(error)),
    }
    Ok(request)
}

fn read_exact_line(
    stream: &mut UnixStream,
    total_deadline: Duration,
    expected: &[u8],
    kind: LineKindV0,
) -> Result<(), FailpointUdsError> {
    let request = read_line(stream, total_deadline, kind)?;
    if request == expected {
        Ok(())
    } else {
        Err(FailpointUdsError::MalformedRelease)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::{fs::PermissionsExt, net::UnixListener},
        time::{SystemTime, UNIX_EPOCH},
    };

    #[test]
    fn sticky_ancestor_layout_requires_a_system_or_current_owner() {
        assert!(!trusted_ancestor_layout(0o1777, 501, 502));
        assert!(trusted_ancestor_layout(0o1777, 0, 502));
        assert!(trusted_ancestor_layout(0o1777, 502, 502));
        assert!(!trusted_ancestor_layout(0o777, 0, 502));
    }

    #[test]
    fn configuration_failure_guard_preserves_a_replacement() {
        let root = std::env::temp_dir().join(format!(
            "oxideloop-failpoint-config-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        fs::create_dir(&root).expect("test root");
        let mut permissions = fs::metadata(&root)
            .expect("test root metadata")
            .permissions();
        permissions.set_mode(PARENT_MODE);
        fs::set_permissions(&root, permissions).expect("test root permissions");

        let parent_path = root.join("private");
        let parent = PrivateParent::open_or_create(parent_path.clone()).expect("private parent");
        let name = OsString::from("failpoint.sock");
        let socket_path = parent_path.join(&name);
        let listener = UnixListener::bind(&socket_path).expect("socket bind");
        let creation = SocketCreation {
            identity: bound_socket_identity(&parent, &name).expect("socket identity"),
            parent,
            name,
        };

        fs::remove_file(&socket_path).expect("remove original socket");
        fs::write(&socket_path, b"replacement").expect("replacement");
        assert!(matches!(
            configure_bound_socket(&creation.parent, &creation.name, creation.identity),
            Err(FailpointUdsError::Configure(_))
        ));
        drop(listener);
        creation.remove_if_original();
        assert_eq!(
            fs::read(&socket_path).expect("replacement remains"),
            b"replacement"
        );
        fs::remove_dir_all(&root).expect("cleanup test root");
    }
}
