//! Network primitives for `op_apsides_fetch` (see `ops.rs`): the "accept
//! global unicast only" address classifier, a reusable DNS resolver handle
//! (real `hickory-resolver`, built once by the caller before any Landlock
//! lockdown -- see [`EgressResolver`] -- or a fixture-mode `host -> addrs`
//! override), address vetting + first-successful-connect dialing across
//! every resolved candidate, and an HTTP/1.1-over-TLS request built on
//! `hyper`'s low-level connection API over a dedicated `rustls`
//! `ClientConfig` (webpki roots, no client auth, no OS trust store, no OS
//! proxy configuration).
//!
//! Kept separate from `ops.rs` so the address classifier -- the part of
//! this module with genuine security weight -- can be unit-tested in
//! isolation, with no V8/op machinery involved at all.

use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::{Arc, Once};
use std::time::{Duration, Instant};

use bytes::Bytes;
use hickory_resolver::TokioResolver;
use hickory_resolver::config::{LookupIpStrategy, ResolveHosts, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use http_body_util::{BodyExt, Full, Limited};
use hyper::client::conn::http1;
use hyper::header::{HeaderName, HeaderValue};
use hyper_util::rt::TokioIo;
use thiserror::Error;
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::{CertificateDer, ServerName};

/// True only for a global-unicast address, in the sense of the IANA
/// IPv4/IPv6 special-purpose address registries (equivalent to the
/// unstable `Ipv4Addr::is_global`/`Ipv6Addr::is_global`, reimplemented
/// here since those are not yet stable), plus the embedded/transition
/// forms that can smuggle a non-global address through an otherwise
/// "global" outer address: IPv4-mapped and IPv4-compatible IPv6, the
/// NAT64 well-known prefixes, 6to4, and Teredo. This is the full reject
/// list `op_apsides_fetch` applies to every resolved candidate address
/// before ever connecting to it -- see the Apsides v0.3 plan, Phase 5b,
/// and this module's own table-driven tests for every named range.
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
    let o = ip.octets();
    // 0.0.0.0/8 ("this network" -- is_unspecified() above only catches
    // 0.0.0.0 itself, not the rest of the /8).
    if o[0] == 0 {
        return false;
    }
    // RFC1918 private ranges.
    if o[0] == 10 || (o[0] == 172 && o[1] >= 16 && o[1] <= 31) || (o[0] == 192 && o[1] == 168) {
        return false;
    }
    // CGNAT, RFC 6598, 100.64.0.0/10.
    if o[0] == 100 && o[1] >= 64 && o[1] <= 127 {
        return false;
    }
    // Reserved, 240.0.0.0/4 (also covers the rest of 255.0.0.0/8 beyond
    // the exact broadcast address already caught above).
    if o[0] >= 240 {
        return false;
    }
    // Benchmarking, RFC 2544, 198.18.0.0/15.
    if o[0] == 198 && (o[1] == 18 || o[1] == 19) {
        return false;
    }
    // IETF protocol assignments, 192.0.0.0/24.
    if o[0] == 192 && o[1] == 0 && o[2] == 0 {
        return false;
    }
    // Documentation, TEST-NET-1/2/3.
    if (o[0] == 192 && o[1] == 0 && o[2] == 2)
        || (o[0] == 198 && o[1] == 51 && o[2] == 100)
        || (o[0] == 203 && o[1] == 0 && o[2] == 113)
    {
        return false;
    }
    // 6to4 relay anycast, 192.88.99.0/24 (deprecated, RFC 7526).
    if o[0] == 192 && o[1] == 88 && o[2] == 99 {
        return false;
    }
    true
}

const fn is_global_unicast_v6(ip: Ipv6Addr) -> bool {
    if ip.is_loopback() || ip.is_multicast() || ip.is_unspecified() {
        return false;
    }
    let s = ip.segments();
    // Link-local, fe80::/10.
    if s[0] & 0xffc0 == 0xfe80 {
        return false;
    }
    // Unique local (ULA), fc00::/7.
    if s[0] & 0xfe00 == 0xfc00 {
        return false;
    }
    // Deprecated site-local, fec0::/10 (a subset of the ULA mask above on
    // some readings, but its own historical registry entry; checked
    // explicitly for clarity and to match the review's exact list).
    if s[0] & 0xffc0 == 0xfec0 {
        return false;
    }
    // Documentation, 2001:db8::/32.
    if s[0] == 0x2001 && s[1] == 0x0db8 {
        return false;
    }
    // Discard-only, 100::/64.
    if s[0] == 0x0100 && s[1] == 0 && s[2] == 0 && s[3] == 0 {
        return false;
    }
    // 6to4, 2002::/16.
    if s[0] == 0x2002 {
        return false;
    }
    // Teredo, 2001:0000::/32.
    if s[0] == 0x2001 && s[1] == 0x0000 {
        return false;
    }
    // IPv4-mapped, ::ffff:0:0/96.
    if ip.to_ipv4_mapped().is_some() {
        return false;
    }
    // IPv4-compatible (deprecated), ::0.0.0.0/96 -- any address whose
    // first 96 bits are zero (this also matches `::`/`::1`, already
    // caught above; the overlap is harmless).
    if s[0] == 0 && s[1] == 0 && s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0 {
        return false;
    }
    // NAT64 well-known prefixes, 64:ff9b::/96 and 64:ff9b:1::/48.
    if s[0] == 0x0064 && s[1] == 0xff9b && ((s[2] == 0 && s[3] == 0 && s[4] == 0 && s[5] == 0) || s[2] == 0x0001)
    {
        return false;
    }
    true
}

#[derive(Debug, Error)]
pub enum ResolveError {
    #[error("no override addresses are configured for host {0:?}")]
    NoOverride(String),
    #[error("no resolver was configured, and no fixture override matched this host")]
    NoResolverConfigured,
    #[error("failed to build the DNS resolver: {0}")]
    ResolverBuild(String),
    #[error("DNS lookup for {0:?} timed out")]
    Timeout(String),
    #[error("DNS lookup for {0:?} failed: {1}")]
    LookupFailed(String, String),
    #[error("DNS lookup for {0:?} returned no addresses")]
    NoAddresses(String),
}

/// A reusable DNS resolver handle for `apsides.fetch`.
///
/// Built **once**, by the caller, before any Landlock lockdown is applied
/// (`hickory-resolver`'s socket setup needs network access that a later
/// deny-all lockdown would refuse) -- not per invocation, and not per
/// `apsides.fetch` call. One `EgressResolver` is shared (via `Arc`) across
/// every `HandlerInvocation` for the process's lifetime.
///
/// This crate deliberately offers **no** constructor that reads the host's
/// system DNS configuration (`/etc/resolv.conf`): `nameservers` must be
/// supplied explicitly by the caller (e.g. the cluster's DNS service
/// address, or a public resolver such as `hickory_resolver::config::GOOGLE`),
/// matching the same "no glibc `getaddrinfo`, no `/etc/resolv.conf`" stance
/// this crate's own tests already establish for the same reason: a
/// container that has already applied its own filesystem lockdown, or that
/// simply has no meaningful `/etc/resolv.conf`, must not depend on one
/// existing.
///
/// Internally pins `ResolverOpts::use_hosts_file = ResolveHosts::Never`
/// (`/etc/hosts` is never consulted either) and
/// `ResolverOpts::ip_strategy = LookupIpStrategy::Ipv4thenIpv6`, so a
/// lookup's results are always IPv4 addresses first, then IPv6 -- see
/// [`resolve_host`]/`ops::fetch`'s "try every vetted candidate in order"
/// dialing.
pub struct EgressResolver(TokioResolver);

impl EgressResolver {
    /// # Errors
    ///
    /// Returns `Err` if the resolver fails to build (e.g. `nameservers` is
    /// empty).
    pub fn new(nameservers: ResolverConfig) -> Result<Self, ResolveError> {
        let mut builder =
            TokioResolver::builder_with_config(nameservers, TokioRuntimeProvider::default());
        builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4thenIpv6;
        builder.options_mut().use_hosts_file = ResolveHosts::Never;
        builder
            .build()
            .map(Self)
            .map_err(|err| ResolveError::ResolverBuild(err.to_string()))
    }
}

/// Resolves `host` to every candidate [`IpAddr`] hickory returns (IPv4
/// first, per [`EgressResolver::new`]'s pinned strategy), or to a
/// fixture-mode `overrides` entry when one is configured -- no real DNS
/// lookup at all in that case. Bounded by `deadline`: a real lookup is
/// wrapped in `tokio::time::timeout`, so a slow/hanging resolver cannot
/// silently consume an invocation's whole wall-clock budget.
///
/// Returns every candidate, not just the first: `ops::fetch` vets and
/// dials each in turn (see [`is_global_unicast`]), since trusting only the
/// first answer would make a fetch fail on an IPv4-only network whenever a
/// AAAA record happened to sort first, and would give a hostile or
/// misconfigured answer with a bogus first record the power to block a
/// request a later, legitimate record would have served.
///
/// # Errors
///
/// Returns `Err` if `overrides` is `Some` and has no (non-empty) entry for
/// `host`; if neither `overrides` nor `resolver` is available; if the
/// lookup times out; if the lookup itself fails; or if the lookup succeeds
/// with zero addresses.
pub async fn resolve_host(
    host: &str,
    overrides: Option<&HashMap<String, Vec<IpAddr>>>,
    resolver: Option<&EgressResolver>,
    deadline: Instant,
) -> Result<Vec<IpAddr>, ResolveError> {
    if let Some(map) = overrides {
        let addrs = map.get(host).cloned().unwrap_or_default();
        return if addrs.is_empty() {
            Err(ResolveError::NoOverride(host.to_owned()))
        } else {
            Ok(addrs)
        };
    }

    let resolver = resolver.ok_or(ResolveError::NoResolverConfigured)?;
    let budget = remaining(deadline).map_err(|()| ResolveError::Timeout(host.to_owned()))?;
    let lookup = tokio::time::timeout(budget, resolver.0.lookup_ip(host))
        .await
        .map_err(|_elapsed| ResolveError::Timeout(host.to_owned()))?
        .map_err(|err| ResolveError::LookupFailed(host.to_owned(), err.to_string()))?;
    let addrs: Vec<IpAddr> = lookup.iter().collect();
    if addrs.is_empty() {
        return Err(ResolveError::NoAddresses(host.to_owned()));
    }
    Ok(addrs)
}

#[derive(Debug, Error)]
pub enum DialError {
    #[error("none of the resolved addresses for this host are global unicast addresses")]
    NoGlobalUnicastAddress,
    #[error("connect timed out")]
    Timeout,
    #[error("every vetted address failed to connect: {0}")]
    AllAttemptsFailed(String),
}

/// Tries every `addrs` entry in order (already IPv4-first, per
/// [`EgressResolver`]/fixture-override ordering), skipping any that fail
/// [`is_global_unicast`] unless `skip_vetting` is set, and connects to the
/// first one that succeeds within `deadline`. Never dials an address that
/// failed vetting.
///
/// `skip_vetting`: only ever `true` for the fixture-mode bundle described
/// on `FixtureEgressOverride` in `lib.rs` -- a local HTTPS test server
/// binds to a loopback address by construction, which
/// [`is_global_unicast`] would otherwise always reject. The caller
/// (`ops::fetch`) sets this only when `EgressPolicy::fixture_trust_anchor`
/// is set, which is itself gated behind the
/// `FIXTURE_ACKNOWLEDGEMENT` check.
///
/// # Errors
///
/// Returns `Err` if no candidate passes vetting at all, if the deadline is
/// reached, or if every vetted candidate's connection attempt fails.
async fn dial_first_vetted(
    addrs: &[IpAddr],
    port: u16,
    deadline: Instant,
    skip_vetting: bool,
) -> Result<TcpStream, DialError> {
    let mut any_vetted = false;
    let mut last_error: Option<String> = None;
    for &ip in addrs {
        if !skip_vetting && !is_global_unicast(ip) {
            continue;
        }
        any_vetted = true;
        let addr = SocketAddr::new(ip, port);
        let budget = remaining(deadline).map_err(|()| DialError::Timeout)?;
        match tokio::time::timeout(budget, TcpStream::connect(addr)).await {
            Ok(Ok(tcp)) => return Ok(tcp),
            Ok(Err(err)) => last_error = Some(format!("{addr}: {err}")),
            Err(_elapsed) => return Err(DialError::Timeout),
        }
    }
    if !any_vetted {
        return Err(DialError::NoGlobalUnicastAddress);
    }
    Err(DialError::AllAttemptsFailed(
        last_error.unwrap_or_else(|| "no candidates attempted".to_owned()),
    ))
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
    #[error(transparent)]
    Dial(#[from] DialError),
    #[error("failed to install the TLS crypto provider: {0}")]
    CryptoProvider(String),
    #[error("invalid TLS server name {0:?}: {1}")]
    InvalidServerName(String, String),
    #[error("TLS handshake with {0:?} failed: {1}")]
    TlsHandshake(String, String),
    #[error("invalid HTTP method {0:?}")]
    InvalidMethod(String),
    #[error("invalid request header {0:?}")]
    InvalidHeader(String),
    #[error("HTTP/1.1 handshake failed: {0}")]
    HttpHandshake(String),
    #[error("request send failed: {0}")]
    RequestSend(String),
    #[error("response headers exceed the {0}-header/{1}-byte cap")]
    ResponseHeadersTooLarge(usize, usize),
    #[error("a response header value was not valid header text")]
    InvalidResponseHeader,
    #[error("response body exceeds the {0} byte cap")]
    ResponseTooLarge(usize),
    #[error("response body read failed: {0}")]
    ResponseBody(String),
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
    /// Absolute deadline for the *whole* request (resolve + connect +
    /// handshake + write + read), derived once from `Limits::timeout` at
    /// the start of the invocation -- "total time <= timeout" from the
    /// plan, not a per-step budget.
    pub deadline: Instant,
}

const MAX_RESPONSE_HEADER_COUNT: usize = 100;
const MAX_RESPONSE_HEADER_BYTES: usize = 64 * 1024;

static CRYPTO_PROVIDER_INIT: Once = Once::new();

fn ensure_crypto_provider_installed() {
    CRYPTO_PROVIDER_INIT.call_once(|| {
        // `install_default` only fails if a provider was already installed
        // (e.g. by another isolate's fetch call, or a test); either way a
        // provider ends up installed, which is all this needs.
        let _ignored = rustls::crypto::ring::default_provider().install_default();
    });
}

fn remaining(deadline: Instant) -> Result<Duration, ()> {
    let now = Instant::now();
    if now >= deadline { Err(()) } else { Ok(deadline - now) }
}

/// Vets and dials the first reachable address in `candidate_ips`,
/// performs a TLS handshake with SNI (and the outgoing `Host` header) set
/// to `declared_host` -- never a caller-supplied value -- and issues one
/// HTTP/1.1 request via `hyper`'s low-level `client::conn::http1` API.
/// `method`/`headers` are validated as real HTTP tokens/header
/// name-value pairs (`hyper::Method`/`HeaderName`/`HeaderValue`), which
/// rejects CR/LF and other request-line/header injection structurally --
/// there is no string concatenation of the wire request anywhere in this
/// function. The caller (`ops::fetch`) has already rejected a
/// caller-supplied `Host`/`Content-Length`/`Transfer-Encoding`/etc. header
/// and enforced the request body cap before calling this; a response
/// (including a 3xx redirect, which this crate does not follow -- see the
/// module doc comment on that decision in `lib.rs`) is returned to the
/// caller as-is.
///
/// # Errors
///
/// See [`HttpsError`]'s variants.
#[allow(
    clippy::too_many_arguments,
    reason = "each argument is a distinct, independently-validated request facet; bundling them into a struct would not reduce the real complexity here"
)]
#[allow(
    clippy::too_many_lines,
    reason = "connect -> TLS handshake -> HTTP/1.1 handshake -> send -> read headers -> read body is one inherently sequential, hard-timeout-wrapped pipeline; splitting it would only move state across an artificial function boundary"
)]
pub async fn https_request(
    candidate_ips: &[IpAddr],
    port: u16,
    declared_host: &str,
    method: &str,
    path_and_query: &str,
    headers: &[(String, String)],
    body: Vec<u8>,
    limits: &FetchLimits,
    extra_trust_anchor: Option<&FixtureTrustAnchor>,
) -> Result<HttpsResponse, HttpsError> {
    ensure_crypto_provider_installed();

    // Skipping address vetting is tied to `extra_trust_anchor` being set,
    // not a separate flag: both are the same fixture-mode-only relaxation
    // (see `FixtureEgressOverride` in `lib.rs`), and threading through a
    // second boolean that must always agree with the first would only add
    // a way for them to disagree.
    let skip_vetting = extra_trust_anchor.is_some();
    let tcp = dial_first_vetted(candidate_ips, port, limits.deadline, skip_vetting).await?;

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

    let tls = tokio::time::timeout(
        remaining(limits.deadline).map_err(|()| HttpsError::Timeout)?,
        connector.connect(server_name, tcp),
    )
    .await
    .map_err(|_elapsed| HttpsError::Timeout)?
    .map_err(|err| HttpsError::TlsHandshake(declared_host.to_owned(), err.to_string()))?;

    let io = TokioIo::new(tls);
    let (mut sender, connection) = tokio::time::timeout(
        remaining(limits.deadline).map_err(|()| HttpsError::Timeout)?,
        http1::handshake(io),
    )
    .await
    .map_err(|_elapsed| HttpsError::Timeout)?
    .map_err(|err| HttpsError::HttpHandshake(err.to_string()))?;

    // hyper's http1 client requires its connection-driving future to be
    // polled concurrently with `sender`; this is the standard idiom.
    // Fully owned (no borrow of anything above), so this is Send + 'static.
    tokio::spawn(async move {
        let _ignored = connection.await;
    });

    let http_method = hyper::Method::from_bytes(method.as_bytes())
        .map_err(|_err| HttpsError::InvalidMethod(method.to_owned()))?;

    let mut request_builder = hyper::Request::builder()
        .method(http_method)
        .uri(path_and_query)
        .header(hyper::header::HOST, declared_host);
    for (name, value) in headers {
        let header_name = HeaderName::from_bytes(name.as_bytes())
            .map_err(|_err| HttpsError::InvalidHeader(name.clone()))?;
        let header_value = HeaderValue::from_bytes(value.as_bytes())
            .map_err(|_err| HttpsError::InvalidHeader(value.clone()))?;
        request_builder = request_builder.header(header_name, header_value);
    }
    let request = request_builder
        .body(Full::new(Bytes::from(body)))
        .map_err(|err| HttpsError::InvalidHeader(err.to_string()))?;

    let response = tokio::time::timeout(
        remaining(limits.deadline).map_err(|()| HttpsError::Timeout)?,
        sender.send_request(request),
    )
    .await
    .map_err(|_elapsed| HttpsError::Timeout)?
    .map_err(|err| {
        // `hyper` enforces its own internal header-size limit while
        // parsing the response, independently of the manual header
        // count/byte cap this function enforces afterward on whatever it
        // *did* get back -- a response whose headers are large enough to
        // hit hyper's own limit never reaches that later check at all, so
        // it is mapped to the same `ResponseHeadersTooLarge` code here
        // (review round 6, non-blocking item).
        if err.is_parse_too_large() {
            HttpsError::ResponseHeadersTooLarge(MAX_RESPONSE_HEADER_COUNT, MAX_RESPONSE_HEADER_BYTES)
        } else {
            HttpsError::RequestSend(err.to_string())
        }
    })?;

    let status = response.status().as_u16();

    let mut headers_out = Vec::new();
    let mut header_bytes = 0_usize;
    for (name, value) in response.headers() {
        if headers_out.len() >= MAX_RESPONSE_HEADER_COUNT {
            return Err(HttpsError::ResponseHeadersTooLarge(
                MAX_RESPONSE_HEADER_COUNT,
                MAX_RESPONSE_HEADER_BYTES,
            ));
        }
        let value_str = value
            .to_str()
            .map_err(|_err| HttpsError::InvalidResponseHeader)?;
        header_bytes += name.as_str().len() + value_str.len();
        if header_bytes > MAX_RESPONSE_HEADER_BYTES {
            return Err(HttpsError::ResponseHeadersTooLarge(
                MAX_RESPONSE_HEADER_COUNT,
                MAX_RESPONSE_HEADER_BYTES,
            ));
        }
        headers_out.push((name.as_str().to_owned(), value_str.to_owned()));
    }

    let limited_body = Limited::new(response.into_body(), limits.max_response_bytes);
    let collected = tokio::time::timeout(
        remaining(limits.deadline).map_err(|()| HttpsError::Timeout)?,
        limited_body.collect(),
    )
    .await
    .map_err(|_elapsed| HttpsError::Timeout)?
    .map_err(|err| {
        if err.is::<http_body_util::LengthLimitError>() {
            HttpsError::ResponseTooLarge(limits.max_response_bytes)
        } else {
            HttpsError::ResponseBody(err.to_string())
        }
    })?;
    let body = collected.to_bytes().to_vec();

    Ok(HttpsResponse {
        status,
        headers: headers_out,
        body,
    })
}

#[cfg(test)]
mod tests {
    use super::is_global_unicast;
    use std::net::IpAddr;

    /// `s.parse()` on a fixed literal below never actually fails; returning
    /// `Result` and propagating with `?` (rather than `.unwrap()`/
    /// `.expect()`/`panic!`, all denied by the workspace's clippy lints)
    /// still gives a real, readable test failure if one of these literals
    /// is ever mistyped.
    fn addr(s: &str) -> Result<IpAddr, String> {
        s.parse().map_err(|err| format!("bad literal {s:?}: {err}"))
    }

    /// Table-driven: `(literal, expected is_global_unicast)`, one row per
    /// named range in the review's reject list plus a handful of ordinary
    /// public addresses and boundary cases either side of a range.
    #[test]
    fn classifies_every_named_range() -> Result<(), String> {
        let cases: &[(&str, bool)] = &[
            // Ordinary public addresses.
            ("8.8.8.8", true),
            ("1.1.1.1", true),
            ("2606:4700:4700::1111", true),
            // IPv4 loopback / unspecified / link-local / multicast / broadcast.
            ("127.0.0.1", false),
            ("127.255.255.255", false),
            ("0.0.0.0", false),
            ("0.1.2.3", false),
            ("169.254.1.1", false),
            ("224.0.0.1", false),
            ("255.255.255.255", false),
            // RFC1918 private, with boundaries.
            ("10.0.0.1", false),
            ("172.16.0.1", false),
            ("172.31.255.255", false),
            ("172.32.0.1", true),
            ("172.15.255.255", true),
            ("192.168.1.1", false),
            // CGNAT, with boundaries.
            ("100.64.0.1", false),
            ("100.127.255.255", false),
            ("100.63.255.255", true),
            ("100.128.0.0", true),
            // Reserved 240.0.0.0/4.
            ("240.0.0.1", false),
            ("255.0.0.1", false),
            // Benchmarking 198.18.0.0/15.
            ("198.18.0.1", false),
            ("198.19.255.255", false),
            ("198.17.255.255", true),
            ("198.20.0.0", true),
            // IETF protocol assignments.
            ("192.0.0.1", false),
            // Documentation (TEST-NET-1/2/3).
            ("192.0.2.1", false),
            ("198.51.100.1", false),
            ("203.0.113.1", false),
            // 6to4 relay anycast.
            ("192.88.99.1", false),
            // IPv6 loopback / unspecified / multicast.
            ("::1", false),
            ("::", false),
            ("ff02::1", false),
            // IPv6 link-local / ULA / deprecated site-local.
            ("fe80::1", false),
            ("fc00::1", false),
            ("fd12:3456:789a::1", false),
            ("fec0::1", false),
            // IPv6 documentation.
            ("2001:db8::1", false),
            // Discard-only.
            ("100::1", false),
            // 6to4.
            ("2002::1", false),
            // Teredo.
            ("2001:0:1234::1", false),
            // Embedded IPv4 forms.
            ("::ffff:8.8.8.8", false),
            ("::8.8.8.8", false),
            ("64:ff9b::8.8.8.8", false),
            ("64:ff9b:1::8.8.8.8", false),
        ];

        for (literal, expected) in cases {
            let ip = addr(literal)?;
            let actual = is_global_unicast(ip);
            if actual != *expected {
                return Err(format!(
                    "is_global_unicast({literal}) = {actual}, expected {expected}"
                ));
            }
        }
        Ok(())
    }

    /// review round 5, B4: every resolved candidate is tried in order,
    /// not just the first -- a vetted-but-unreachable candidate must be
    /// skipped, not abort the whole dial. Uses two distinct loopback
    /// addresses sharing one port: nothing listens on `127.0.0.1` at that
    /// port, and a real listener is bound to `127.0.0.2`.
    #[tokio::test]
    async fn dial_first_vetted_skips_an_unreachable_candidate_and_uses_the_next() -> Result<(), String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.2:0")
            .await
            .map_err(|err| format!("failed to bind the reachable candidate: {err}"))?;
        let port = listener
            .local_addr()
            .map_err(|err| format!("failed to read the reachable candidate's address: {err}"))?
            .port();
        let accept_task = tokio::spawn(async move {
            let _ignored = listener.accept().await;
        });

        let candidates = [
            addr("127.0.0.1")?, // vetted (loopback is filtered only via `skip_vetting=false`... see below), but unreachable
            addr("127.0.0.2")?,
        ];
        let deadline = super::Instant::now() + super::Duration::from_secs(5);
        // `skip_vetting: true`: loopback addresses would otherwise fail
        // `is_global_unicast` (covered by `classifies_every_named_range`
        // above) before ever reaching the dial loop this test exists to
        // exercise -- that classification is deliberately bypassed here,
        // the same way fixture-mode `apsides.fetch` calls bypass it, so
        // this test is purely about "try every candidate in order, skip a
        // failed one", independent of address classification.
        let result = super::dial_first_vetted(&candidates, port, deadline, true).await;
        let connected = result.is_ok();
        accept_task.abort();

        if !connected {
            return Err(format!(
                "expected the second, reachable candidate to succeed after the first failed, got {result:?}"
            ));
        }
        Ok(())
    }
}
