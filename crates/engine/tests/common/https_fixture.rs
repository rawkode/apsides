//! A local, in-process HTTPS test server for `op_apsides_fetch`'s
//! "allowed" tests. Generates a throwaway self-signed leaf certificate
//! with `rcgen` (its default `ring`/`pem` features, matching the `ring`
//! crypto backend already chosen for `rustls`/`tokio-rustls` elsewhere in
//! this crate), accepts exactly one TLS connection on an ephemeral
//! `127.0.0.1` port, and serves one fixed, caller-supplied raw HTTP
//! response.
//!
//! This is a *fixture-mode-only* construct: the certificate it generates
//! is never trusted by anything other than a test that explicitly passes
//! its `ca_der` as an `engine::FixtureEgressOverride::trust_anchor_der`
//! with `acknowledgement == engine::FIXTURE_ACKNOWLEDGEMENT` -- see that
//! constant's doc comment for why the two are gated together. A test that
//! wants to exercise the "untrusted issuer" failure path instead simply
//! never sets `fixture_trust_anchor` at all.

use std::net::SocketAddr;
use std::sync::Arc;

use rcgen::{CertifiedKey, generate_simple_self_signed};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};

/// The DNS name every fixture certificate/server uses. Tests declare this
/// exact name in their `EgressPolicy::allowed`/`resolver_override`.
pub const FIXTURE_HOST: &str = "apsides-fixture.invalid";

pub struct HttpsFixtureServer {
    pub addr: SocketAddr,
    pub ca_der: Vec<u8>,
    handle: std::thread::JoinHandle<Result<(), String>>,
}

impl HttpsFixtureServer {
    /// Starts the server on a dedicated background thread (with its own
    /// single-threaded tokio runtime -- this crate's own
    /// `invoke_handler_blocking`/`invoke_handler_with_watchdog` each build
    /// and own their own runtime too, so the fixture server cannot share
    /// one with the code under test). Accepts exactly one connection,
    /// reads and discards the request up through its header terminator,
    /// writes `response` verbatim, and exits -- enough for every
    /// `apsides.fetch` call in this crate's own tests, each of which
    /// issues exactly one request per server instance.
    ///
    /// # Errors
    ///
    /// Returns `Err` if certificate generation or the listening socket
    /// bind fails.
    pub fn start(response: &'static [u8]) -> Result<Self, String> {
        Self::start_internal(response, true)
    }

    /// Like [`Self::start`], but ends the connection by simply dropping it
    /// (no TLS `close_notify`) after writing `response`, rather than a
    /// clean `tls.shutdown()`. Used by the tests confirming `hyper` still
    /// completes a `Content-Length`-delimited response correctly without
    /// one (review round 5, B2) -- the crate's old hand-rolled client,
    /// which always read to EOF, could not have passed that case.
    ///
    /// # Errors
    ///
    /// See [`Self::start`].
    pub fn start_without_close_notify(response: &'static [u8]) -> Result<Self, String> {
        Self::start_internal(response, false)
    }

    fn start_internal(response: &'static [u8], send_close_notify: bool) -> Result<Self, String> {
        let CertifiedKey { cert, signing_key } =
            generate_simple_self_signed(vec![FIXTURE_HOST.to_owned()])
                .map_err(|err| format!("failed to generate the fixture certificate: {err}"))?;
        let cert_der = cert.der().clone();
        let ca_der = cert_der.to_vec();
        let key_der =
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(signing_key.serialize_der()));

        let listener_std = std::net::TcpListener::bind("127.0.0.1:0")
            .map_err(|err| format!("failed to bind the fixture listener: {err}"))?;
        listener_std
            .set_nonblocking(true)
            .map_err(|err| format!("failed to set the fixture listener non-blocking: {err}"))?;
        let addr = listener_std
            .local_addr()
            .map_err(|err| format!("failed to read the fixture listener's local address: {err}"))?;

        let handle = std::thread::spawn(move || -> Result<(), String> {
            // Discarded deliberately: only fails if a provider was already
            // installed (by another test in this binary), which is fine.
            let _ignored = rustls::crypto::ring::default_provider().install_default();

            let server_config = rustls::ServerConfig::builder()
                .with_no_client_auth()
                .with_single_cert(vec![cert_der], key_der)
                .map_err(|err| format!("failed to build the fixture TLS server config: {err}"))?;
            let acceptor = TlsAcceptor::from(Arc::new(server_config));

            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|err| format!("failed to build the fixture server's tokio runtime: {err}"))?;

            rt.block_on(async move {
                let listener = TcpListener::from_std(listener_std)
                    .map_err(|err| format!("failed to adopt the fixture listener: {err}"))?;
                let (tcp, _peer) = listener
                    .accept()
                    .await
                    .map_err(|err| format!("fixture accept failed: {err}"))?;
                let mut tls = acceptor
                    .accept(tcp)
                    .await
                    .map_err(|err| format!("fixture TLS handshake failed: {err}"))?;

                let mut buf = [0_u8; 4096];
                loop {
                    let read = tls
                        .read(&mut buf)
                        .await
                        .map_err(|err| format!("fixture request read failed: {err}"))?;
                    if read == 0 || buf[..read].windows(4).any(|window| window == b"\r\n\r\n") {
                        break;
                    }
                }

                tls.write_all(response)
                    .await
                    .map_err(|err| format!("fixture response write failed: {err}"))?;
                if send_close_notify {
                    tls.shutdown()
                        .await
                        .map_err(|err| format!("fixture TLS shutdown failed: {err}"))?;
                }
                // Without `send_close_notify`, `tls` (and the underlying
                // `TcpStream`) is simply dropped here at the end of this
                // async block, closing the connection without a TLS
                // `close_notify`.
                Ok(())
            })
        });

        Ok(Self {
            addr,
            ca_der,
            handle,
        })
    }

    /// Joins the server thread, surfacing any server-side failure (bind,
    /// handshake, read/write) as a real test failure instead of silently
    /// dropping it.
    ///
    /// # Errors
    ///
    /// Returns `Err` if the server thread panicked or reported its own
    /// error.
    pub fn join(self) -> Result<(), String> {
        match self.handle.join() {
            Ok(result) => result,
            Err(_panic) => Err("fixture server thread panicked".to_owned()),
        }
    }
}
