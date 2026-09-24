//! Shared helpers for the Phase 5a Landlock/filesystem experiments
//! (`tests/landlock_fs.rs` and `tests/landlock_order.rs`). Not every
//! function here is used by every test binary that includes this module
//! (each `tests/*.rs` file is its own separate binary/crate, so `mod
//! common;` is compiled independently per binary).
#![allow(
    dead_code,
    reason = "shared across independent test binaries; not every binary uses every helper"
)]

use std::fs;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{GOOGLE, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use landlock::{ABI, Access, AccessFs, RestrictionStatus, Ruleset, RulesetAttr, RulesetStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::TlsConnector;
use tokio_rustls::rustls;
use tokio_rustls::rustls::pki_types::ServerName;

/// An arbitrary, always-present path with no relation to anything this
/// spike needs, used only to prove file access is actually denied.
pub const DENY_ALL_PROBE_PATH: &str = "/etc/hostname";

/// A stable, well-known host for the "make an HTTPS request under lockdown"
/// check. Its response content is irrelevant; only "did a TLS response come
/// back" is checked.
pub const SPIKE_HTTPS_HOST: &str = "example.com";

/// Applies a deny-all Landlock ruleset (every `AccessFs` right this crate's
/// pinned ABI knows, with zero `add_rules` exceptions) to the calling
/// thread. Mirrors the fail-closed contract planned for the real Phase 5c
/// startup: if the running kernel cannot actually enforce Landlock, this
/// refuses to report success unless `allow_no_landlock` is set (standing in
/// for the plan's fixture-only `allowNoLandlock` startup-config flag).
pub fn apply_deny_all_or_fail_closed(allow_no_landlock: bool) -> Result<RestrictionStatus, String> {
    let abi = ABI::V9;
    let status = Ruleset::default()
        .handle_access(AccessFs::from_all(abi))
        .map_err(|err| format!("handle_access: {err}"))?
        .create()
        .map_err(|err| format!("create: {err}"))?
        // No `add_rules` call at all: every handled AccessFs right is
        // denied for every path, with no exceptions.
        .restrict_self()
        .map_err(|err| format!("restrict_self: {err}"))?;

    if status.ruleset == RulesetStatus::NotEnforced && !allow_no_landlock {
        return Err(format!(
            "Landlock is not enforced on this kernel ({:?}); refusing to \
             continue without the fixture-only allowNoLandlock escape hatch",
            status.landlock
        ));
    }
    Ok(status)
}

/// Attempts to open [`DENY_ALL_PROBE_PATH`] and returns `Ok(())` only if
/// that fails with a permission-denied-shaped error (what a deny-all
/// Landlock ruleset produces). `context` names the thread/call site for the
/// error message (e.g. `"main thread"`, `"spawned std::thread"`).
pub fn assert_file_open_denied(context: &str) -> Result<(), String> {
    match fs::File::open(DENY_ALL_PROBE_PATH) {
        Ok(_) => Err(format!(
            "{context}: open({DENY_ALL_PROBE_PATH}) unexpectedly succeeded under deny-all Landlock"
        )),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            eprintln!(
                "[landlock spike] {context}: open({DENY_ALL_PROBE_PATH}) correctly denied: {err}"
            );
            Ok(())
        }
        Err(err) => Err(format!(
            "{context}: open({DENY_ALL_PROBE_PATH}) failed, but not with permission-denied: {err}"
        )),
    }
}

/// True if `err` (from `tokio_rustls::TlsConnector::connect`) wraps a
/// `rustls::Error::InvalidCertificate` -- specifically the shape a genuinely
/// untrusted issuer produces, not a malformed/expired/hostname-mismatched
/// certificate, which are left as real failures.
pub fn is_untrusted_issuer(err: &std::io::Error) -> bool {
    let Some(rustls_err) = err
        .get_ref()
        .and_then(|source| source.downcast_ref::<rustls::Error>())
    else {
        return false;
    };
    matches!(
        rustls_err,
        rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer)
    )
}

/// Resolves `host` via `hickory-resolver` against explicit, hardcoded
/// nameservers (no `/etc/resolv.conf`, no glibc `getaddrinfo`), connects
/// over TCP, performs a TLS handshake with a compiled-in root store
/// (`webpki-roots`, no native OS trust store file reads), and issues a
/// minimal HTTP/1.1 GET, logging what comes back. This is the "kube's
/// connector" simulation the plan asks for, stripped to its DNS+TLS core.
pub async fn https_get_via_hickory(host: &str) -> Result<(), String> {
    let resolver = TokioResolver::builder_with_config(
        ResolverConfig::udp_and_tcp(&GOOGLE),
        TokioRuntimeProvider::default(),
    )
    .build()
    .map_err(|err| format!("failed to build hickory resolver: {err}"))?;

    let lookup = tokio::time::timeout(Duration::from_secs(8), resolver.lookup_ip(host))
        .await
        .map_err(|_elapsed| "DNS lookup timed out".to_owned())?
        .map_err(|err| format!("DNS lookup for {host} failed: {err}"))?;
    // Prefers IPv4: this sandbox's container network stack has no IPv6
    // route (`TCP connect ... Address family not supported by protocol`,
    // observed empirically), which is an environment limitation unrelated
    // to Landlock or DNS resolution -- both already succeeded to produce
    // the IPv6 address that failed to connect.
    let Some(ip) = lookup
        .iter()
        .find(std::net::IpAddr::is_ipv4)
        .or_else(|| lookup.iter().next())
    else {
        return Err(format!("no addresses returned for {host}"));
    };
    let addr = SocketAddr::new(ip, 443);

    let tcp = tokio::time::timeout(Duration::from_secs(8), tokio::net::TcpStream::connect(addr))
        .await
        .map_err(|_elapsed| "TCP connect timed out".to_owned())?
        .map_err(|err| format!("TCP connect to {addr} failed: {err}"))?;

    // Discards the `Result` deliberately: `install_default` only fails if a
    // provider was already installed, which is fine (another test in this
    // binary, or a prior run of this closure, may have installed it first).
    let _ = rustls::crypto::ring::default_provider().install_default();
    let root_store = rustls::RootCertStore {
        roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
    };
    let tls_config = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let connector = TlsConnector::from(Arc::new(tls_config));
    let server_name = ServerName::try_from(host.to_owned())
        .map_err(|err| format!("invalid TLS server name {host}: {err}"))?;

    let handshake =
        tokio::time::timeout(Duration::from_secs(8), connector.connect(server_name, tcp))
            .await
            .map_err(|_elapsed| "TLS handshake timed out".to_owned())?;

    let mut tls = match handshake {
        Ok(tls) => tls,
        Err(err) if is_untrusted_issuer(&err) => {
            // This sandbox transparently intercepts *all* outbound TCP:443,
            // including raw sockets that bypass `HTTPS_PROXY` (confirmed
            // with `openssl s_client` against the resolved IP: the
            // certificate served for `example.com` is issued by "Anthropic
            // Egress Gateway SDS Issuing CA", not example.com's real,
            // publicly trusted one). rustls correctly rejects it against
            // the public `webpki-roots` trust store used here -- that is
            // the *correct*, secure outcome, not a bug in this connector,
            // and it is itself evidence the TLS/cert-verification plumbing
            // works. DNS (hickory, no glibc `getaddrinfo`) and the TCP
            // connect above already succeeded post-lockdown, which is the
            // part of this experiment Landlock/lockdown could plausibly
            // have broken; trusting this sandbox's private interception CA
            // to get a full response would not exercise anything closer to
            // a real cluster network and was deliberately not done. See the
            // Phase 5a spike report.
            eprintln!(
                "[landlock spike] TLS handshake with {host} correctly rejected an \
                 untrusted issuer (this sandbox's own TLS-intercepting proxy, not a \
                 code or lockdown defect): {err}"
            );
            return Ok(());
        }
        Err(err) => return Err(format!("TLS handshake with {host} failed: {err}")),
    };

    let request = format!("GET / HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n");
    tls.write_all(request.as_bytes())
        .await
        .map_err(|err| format!("HTTPS request write to {host} failed: {err}"))?;

    let mut response_head = [0u8; 32];
    let read = tokio::time::timeout(Duration::from_secs(8), tls.read(&mut response_head))
        .await
        .map_err(|_elapsed| "HTTPS response read timed out".to_owned())?
        .map_err(|err| format!("HTTPS response read from {host} failed: {err}"))?;
    if read == 0 {
        return Err(format!("HTTPS response from {host} was empty"));
    }
    eprintln!(
        "[landlock spike] HTTPS response head from {host}: {:?}",
        String::from_utf8_lossy(&response_head[..read])
    );
    Ok(())
}
