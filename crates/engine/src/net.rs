//! Network primitives for `op_apsides_fetch` (see `ops.rs`): the "accept
//! global unicast only" address classifier, a DNS resolution helper that is
//! either a real `hickory-resolver` lookup or a fixture-mode `host -> addr`
//! override, and a minimal hand-rolled HTTP/1.1-over-TLS client built on a
//! dedicated `rustls::ClientConfig` (webpki roots, no client auth, no OS
//! trust store, no OS proxy configuration).
//!
//! Kept separate from `ops.rs` so the address classifier -- the part of
//! this module with genuine security weight -- can be unit-tested in
//! isolation, with no V8/op machinery involved at all.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{GOOGLE, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use thiserror::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};

/// True only for a global-unicast address: not loopback, link-local,
/// RFC1918-private, unique-local (IPv6 ULA), CGNAT (100.64.0.0/10),
/// "this network" (0.0.0.0/8), multicast/broadcast, or an
/// IPv4-mapped/NAT64 IPv6 address. This is the full reject list
/// `op_apsides_fetch` applies to a resolved address before ever connecting
/// to it -- see the Apsides v0.3 plan, Phase 5b.
///
/// Deliberately conservative (an explicit reject list, not an attempt at a
/// full IANA special-purpose-registry allowlist): anything not positively
/// identified as one of these reserved/non-global classes is treated as
/// global and allowed through, so this function is re-checked, not
/// extended by removing checks, if a new reject class is ever needed.
#[must_use]
pub const fn is_global_unicast(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_global_unicast_v4(v4),
        IpAddr::V6(v6) => is_global_unicast_v6(v6),
    }
}

const fn is_global_unicast_v4(ip: Ipv4Addr) -> bool {
    if ip.is_loopback()
        || ip.is_link_local()
        || ip.is_multicast()
        || ip.is_broadcast()
        || ip.is_unspecified()
    {
        return false;
    }
    let octets = ip.octets();
    // 0.0.0.0/8 ("this network" -- is_unspecified() above only catches
    // 0.0.0.0 itself, not the rest of the /8).
    if octets[0] == 0 {
        return false;
    }
    // RFC1918 private ranges.
    if octets[0] == 10 || (octets[0] == 172 && octets[1] >= 16 && octets[1] <= 31) {
        return false;
    }
    if octets[0] == 192 && octets[1] == 168 {
        return false;
    }
    // CGNAT, RFC 6598.
    if octets[0] == 100 && octets[1] >= 64 && octets[1] <= 127 {
        return false;
    }
    true
}

const fn is_global_unicast_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_multicast() || ip.is_unspecified() {
        return false;
    }
    if ip.to_ipv4_mapped().is_some() {
        return false;
    }
    let segments = ip.segments();
    // Link-local, fe80::/10.
    if segments[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    // Unique local (ULA), fc00::/7.
    if segments[0] & 0xfe00 == 0xfc00 {
        return false;
    }
    // NAT64 well-known prefix, 64:ff9b::/96.
    if segments[0] == 0x0064
        && segments[1] == 0xff9b
        && segments[2] == 0
        && segments[3] == 0
        && segments[4] == 0
        && segments[5] == 0
    {
        return false;
    }
    true
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("no override address is configured for host {0:?}")]
    NoOverride(String),
    #[error("failed to build the DNS resolver: {0}")]
    ResolverBuild(String),
    #[error("DNS lookup for {0:?} failed: {1}")]
    LookupFailed(String, String),
    #[error("DNS lookup for {0:?} returned no addresses")]
    NoAddresses(String),
}

/// Resolves `host` to a single [`IpAddr`], either from `overrides` (fixture
/// mode/tests: no real DNS lookup at all) or via one `hickory-resolver`
/// lookup against explicit, hardcoded nameservers -- never glibc
/// `getaddrinfo`/`/etc/resolv.conf`. "Resolve once": exactly one lookup,
/// whose result is the address `op_apsides_fetch` both classifies and
/// connects to, so there is no time-of-check/time-of-use gap between the
/// address that was vetted and the address that is dialed.
///
/// # Errors
///
/// Returns `Err` if `overrides` is `Some` and has no entry for `host`, if
/// the resolver cannot be built, if the lookup itself fails, or if the
/// lookup succeeds with zero addresses.
pub async fn resolve_host(
    host: &str,
    overrides: Option<&HashMap<String, IpAddr>>,
) -> Result<IpAddr, ResolveError> {
    if let Some(map) = overrides {
        return map
            .get(host)
            .copied()
            .ok_or_else(|| ResolveError::NoOverride(host.to_owned()));
    }

    let resolver = TokioResolver::builder_with_config(
        ResolverConfig::udp_and_tcp(&GOOGLE),
        TokioRuntimeProvider::default(),
    )
    .build()
    .map_err(|err| ResolveError::ResolverBuild(err.to_string()))?;
    let lookup = resolver
        .lookup_ip(host)
        .await
        .map_err(|err| ResolveError::LookupFailed(host.to_owned(), err.to_string()))?;
    lookup
        .iter()
        .next()
        .ok_or_else(|| ResolveError::NoAddresses(host.to_owned()))
}

/// A fixture-only extra TLS trust anchor (a self-signed test CA's DER
/// certificate), added to the webpki root store for exactly one request.
/// Never derived from anything an isolate/handler can influence -- see
/// `FixtureEgressOverride` in `lib.rs` for the acknowledgement gate that
/// controls when this is ever constructed at all.
pub struct FixtureTrustAnchor {
    pub der: Vec<u8>,
}

#[derive(Debug, Error)]
pub enum HttpsError {
    #[error("TCP connect to {0} failed: {1}")]
    Connect(SocketAddr, String),
    #[error("failed to install the TLS crypto provider: {0}")]
    CryptoProvider(String),
    #[error("invalid TLS server name {0:?}: {1}")]
    InvalidServerName(String, String),
    #[error("TLS handshake with {0:?} failed: {1}")]
    TlsHandshake(String, String),
    #[error("request write failed: {0}")]
    Write(String),
    #[error("response read failed: {0}")]
    Read(String),
    #[error("malformed HTTP response status line")]
    MalformedStatusLine,
    #[error("malformed HTTP response headers")]
    MalformedHeaders,
    #[error("response exceeds the {0} byte cap")]
    ResponseTooLarge(usize),
    #[error("server responded with a redirect status, which is never followed")]
    RedirectNotFollowed,
    #[error("request timed out")]
    Timeout,
}

pub struct HttpsResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

pub struct FetchLimits {
    pub max_response_bytes: usize,
    /// Absolute deadline for the *whole* request (connect + handshake +
    /// write + read), derived once from `Limits::timeout` at the start of
    /// the invocation -- "total time <= timeout" from the plan, not a
    /// per-step budget.
    pub deadline: Instant,
}

static CRYPTO_PROVIDER_INIT: Once = Once::new();

fn ensure_crypto_provider_installed() {
    CRYPTO_PROVIDER_INIT.call_once(|| {
        // `install_default` only fails if a provider was already installed
        // (e.g. by another isolate's fetch call, or a test); either way a
        // provider ends up installed, which is all this needs.
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn remaining(deadline: Instant) -> Result<Duration, HttpsError> {
    let now = Instant::now();
    if now >= deadline {
        return Err(HttpsError::Timeout);
    }
    Ok(deadline - now)
}

/// Connects to the already-vetted `addr`, performs a TLS handshake with SNI
/// (and the outgoing `Host` header) set to `declared_host` -- never a
/// caller-supplied value -- and issues one HTTP/1.1 request. Never follows
/// a redirect response; the caller (`ops::op_apsides_fetch`) has already
/// rejected a caller-supplied `Host` header and enforced the request body
/// cap before calling this.
///
/// Reads the response with `Connection: close` and to EOF rather than
/// honoring `Transfer-Encoding: chunked`; sufficient for this crate's own
/// fixture test server (which always sends a fixed `Content-Length`), but a
/// real chunked-encoded server's body would not be decoded correctly. Noted
/// as a known limitation of this minimal client, not implemented here to
/// keep the hand-rolled client small -- see the Phase 5b report.
///
/// # Errors
///
/// See [`HttpsError`]'s variants.
#[allow(clippy::too_many_arguments, reason = "each argument is a distinct, independently-validated request facet; bundling them into a struct would not reduce the real complexity here")]
pub async fn https_request(
    addr: SocketAddr,
    declared_host: &str,
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
    body: &[u8],
    limits: &FetchLimits,
    extra_trust_anchor: Option<&FixtureTrustAnchor>,
) -> Result<HttpsResponse, HttpsError> {
    ensure_crypto_provider_installed();

    let tcp = tokio::time::timeout(
        remaining(limits.deadline)?,
        tokio::net::TcpStream::connect(addr),
    )
    .await
    .map_err(|_elapsed| HttpsError::Timeout)?
    .map_err(|err| HttpsError::Connect(addr, err.to_string()))?;

    let mut root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    if let Some(anchor) = extra_trust_anchor {
        root_store
            .add(CertificateDer::from(anchor.der.clone()))
            .map_err(|err| HttpsError::CryptoProvider(err.to_string()))?;
    }
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name = ServerName::try_from(declared_host.to_owned())
        .map_err(|err| HttpsError::InvalidServerName(declared_host.to_owned(), err.to_string()))?;

    let mut tls = tokio::time::timeout(
        remaining(limits.deadline)?,
        connector.connect(server_name, tcp),
    )
    .await
    .map_err(|_elapsed| HttpsError::Timeout)?
    .map_err(|err| HttpsError::TlsHandshake(declared_host.to_owned(), err.to_string()))?;

    let mut request = format!("{method} {path_and_query} HTTP/1.1\r\nHost: {declared_host}\r\nConnection: close\r\n");
    for (name, value) in headers {
        request.push_str(name);
        request.push_str(": ");
        request.push_str(value);
        request.push_str("\r\n");
    }
    if !body.is_empty() {
        let _ignored_write_result = write!(request, "Content-Length: {}\r\n", body.len());
    }
    request.push_str("\r\n");

    tokio::time::timeout(remaining(limits.deadline)?, async {
        tls.write_all(request.as_bytes()).await?;
        if !body.is_empty() {
            tls.write_all(body).await?;
        }
        tls.flush().await
    })
    .await
    .map_err(|_elapsed| HttpsError::Timeout)?
    .map_err(|err| HttpsError::Write(err.to_string()))?;

    let mut buf = Vec::new();
    let mut chunk = [0_u8; 8192];
    loop {
        let read = tokio::time::timeout(remaining(limits.deadline)?, tls.read(&mut chunk))
            .await
            .map_err(|_elapsed| HttpsError::Timeout)?
            .map_err(|err| HttpsError::Read(err.to_string()))?;
        if read == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..read]);
        if buf.len() > limits.max_response_bytes {
            return Err(HttpsError::ResponseTooLarge(limits.max_response_bytes));
        }
    }

    parse_http_response(&buf, limits.max_response_bytes)
}

fn find_header_end(raw: &[u8]) -> Option<usize> {
    raw.windows(4).position(|w| w == b"\r\n\r\n").map(|pos| pos + 4)
}

fn parse_http_response(raw: &[u8], max_response_bytes: usize) -> Result<HttpsResponse, HttpsError> {
    let header_end = find_header_end(raw).ok_or(HttpsError::MalformedHeaders)?;
    let head =
        std::str::from_utf8(&raw[..header_end]).map_err(|_utf8_err| HttpsError::MalformedHeaders)?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or(HttpsError::MalformedStatusLine)?;
    let mut parts = status_line.splitn(3, ' ');
    let _http_version = parts.next().ok_or(HttpsError::MalformedStatusLine)?;
    let status_code_str = parts.next().ok_or(HttpsError::MalformedStatusLine)?;
    let status: u16 = status_code_str
        .parse()
        .map_err(|_parse_err| HttpsError::MalformedStatusLine)?;

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpsError::MalformedHeaders);
        };
        headers.push((name.trim().to_owned(), value.trim().to_owned()));
    }

    if matches!(status, 301 | 302 | 303 | 307 | 308) {
        return Err(HttpsError::RedirectNotFollowed);
    }

    let body = raw[header_end..].to_vec();
    if body.len() > max_response_bytes {
        return Err(HttpsError::ResponseTooLarge(max_response_bytes));
    }
    Ok(HttpsResponse {
        status,
        headers,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::{is_global_unicast, parse_http_response};
    use std::net::IpAddr;

    /// `s.parse()` on a fixed literal below never actually fails; returning
    /// `Result` and propagating with `?` (rather than `.unwrap()`/
    /// `.expect()`/`panic!`, all denied by the workspace's clippy lints)
    /// still gives a real, readable test failure if one of these literals
    /// is ever mistyped.
    fn addr(s: &str) -> Result<IpAddr, String> {
        s.parse().map_err(|err| format!("bad literal {s:?}: {err}"))
    }

    #[test]
    fn accepts_ordinary_public_addresses() -> Result<(), String> {
        assert!(is_global_unicast(addr("8.8.8.8")?));
        assert!(is_global_unicast(addr("1.1.1.1")?));
        assert!(is_global_unicast(addr("2606:4700:4700::1111")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv4_loopback() -> Result<(), String> {
        assert!(!is_global_unicast(addr("127.0.0.1")?));
        assert!(!is_global_unicast(addr("127.255.255.255")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv4_rfc1918_private() -> Result<(), String> {
        assert!(!is_global_unicast(addr("10.0.0.1")?));
        assert!(!is_global_unicast(addr("172.16.0.1")?));
        assert!(!is_global_unicast(addr("172.31.255.255")?));
        assert!(
            is_global_unicast(addr("172.32.0.1")?),
            "172.32/16 is public, outside 172.16-31/12"
        );
        assert!(!is_global_unicast(addr("192.168.1.1")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv4_link_local() -> Result<(), String> {
        assert!(!is_global_unicast(addr("169.254.1.1")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv4_cgnat() -> Result<(), String> {
        assert!(!is_global_unicast(addr("100.64.0.1")?));
        assert!(!is_global_unicast(addr("100.127.255.255")?));
        assert!(
            is_global_unicast(addr("100.63.255.255")?),
            "just below the CGNAT range"
        );
        assert!(
            is_global_unicast(addr("100.128.0.0")?),
            "just above the CGNAT range"
        );
        Ok(())
    }

    #[test]
    fn rejects_ipv4_this_network_and_unspecified() -> Result<(), String> {
        assert!(!is_global_unicast(addr("0.0.0.0")?));
        assert!(!is_global_unicast(addr("0.1.2.3")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv4_multicast_and_broadcast() -> Result<(), String> {
        assert!(!is_global_unicast(addr("224.0.0.1")?));
        assert!(!is_global_unicast(addr("255.255.255.255")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv6_loopback_and_unspecified() -> Result<(), String> {
        assert!(!is_global_unicast(addr("::1")?));
        assert!(!is_global_unicast(addr("::")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv6_link_local() -> Result<(), String> {
        assert!(!is_global_unicast(addr("fe80::1")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv6_unique_local() -> Result<(), String> {
        assert!(!is_global_unicast(addr("fc00::1")?));
        assert!(!is_global_unicast(addr("fd12:3456:789a::1")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv6_multicast() -> Result<(), String> {
        assert!(!is_global_unicast(addr("ff02::1")?));
        Ok(())
    }

    #[test]
    fn rejects_ipv4_mapped_and_nat64_ipv6() -> Result<(), String> {
        assert!(!is_global_unicast(addr("::ffff:8.8.8.8")?));
        assert!(!is_global_unicast(addr("64:ff9b::8.8.8.8")?));
        Ok(())
    }

    #[test]
    fn parses_an_ordinary_response() -> Result<(), String> {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello";
        let response = parse_http_response(raw, 1024).map_err(|err| err.to_string())?;
        assert_eq!(response.status, 200);
        assert_eq!(response.body, b"hello");
        assert!(
            response
                .headers
                .iter()
                .any(|(k, v)| k == "Content-Type" && v == "text/plain")
        );
        Ok(())
    }

    #[test]
    fn rejects_a_redirect_response() {
        let raw = b"HTTP/1.1 302 Found\r\nLocation: https://example.com/\r\n\r\n";
        let result = parse_http_response(raw, 1024);
        assert!(matches!(result, Err(super::HttpsError::RedirectNotFollowed)));
    }

    #[test]
    fn rejects_a_response_over_the_cap() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        let result = parse_http_response(raw, 2);
        assert!(matches!(result, Err(super::HttpsError::ResponseTooLarge(2))));
    }
}
