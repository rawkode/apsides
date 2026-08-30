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

use oxideloop_renderer_protocol::{
    FrameV0, ProtocolError, RequestV0, ResponseStatusV0, ResponseV0, read_frame, write_frame,
};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum RendererClientError {
    #[error("renderer is unavailable")]
    RendererLost,
    #[error("renderer request is invalid")]
    InvalidRequest,
    #[error("renderer address must be exactly 127.0.0.1")]
    NonLoopbackAddress,
}

/// A client bound to one explicitly configured renderer loopback endpoint.
#[derive(Debug)]
pub struct RendererClient {
    address: SocketAddr,
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
        Ok(Self { address })
    }

    #[must_use]
    pub fn address(&self) -> SocketAddr {
        self.address
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
        validate_loopback_address(self.address)?;
        request
            .validate()
            .map_err(|_| RendererClientError::InvalidRequest)?;
        let deadline = Instant::now() + Duration::from_millis(u64::from(request.duration_millis));
        let mut stream = TcpStream::connect_timeout(&self.address, remaining(deadline)?)
            .map_err(|_| RendererClientError::RendererLost)?;
        stream
            .set_nonblocking(true)
            .map_err(|_| RendererClientError::RendererLost)?;
        write_frame(
            DeadlineStream::new(&mut stream, deadline),
            &FrameV0::Request(request.clone()),
        )
        .map_err(map_protocol_failure)?;
        stream
            .shutdown(Shutdown::Write)
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
