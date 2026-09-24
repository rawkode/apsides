//! Builds the production `kube::Client` used to reach the proxy Service
//! (see the Apsides v0.3 plan, Phase 5c). The proxy is always reached by
//! hostname (`apsides-proxy-<generation>.<namespace>.svc.<cluster-domain>`),
//! so DNS resolution is needed on every (re)connect -- but `kube-client`'s
//! default connector (`hyper_util::client::legacy::connect::HttpConnector`,
//! built with its default `GaiResolver`) resolves via glibc `getaddrinfo`,
//! which reads `/etc/resolv.conf`, `/etc/hosts` and `/etc/nsswitch.conf`.
//! Under this crate's Landlock lockdown (applied once, at startup, before
//! any of these files can be read again -- see `startup`), that resolver
//! would fail closed on the very first reconnect.
//!
//! [`build_client`] instead builds an `HttpConnector` with a custom
//! resolver ([`HickoryResolve`]) backed by `hickory-resolver`, using the
//! nameservers `startup::start` already read from `/etc/resolv.conf` before
//! lockdown (see `production::read_production_startup_files`), with
//! `ResolveHosts::Never` (`/etc/hosts` is never consulted either). TLS --
//! `kube-client`'s own rustls mTLS setup, webpki roots vs
//! `kube::Config::root_cert`, client identity from
//! `kube::Config::auth_info` -- is entirely unmodified:
//! [`kube::client::ConfigExt::rustls_https_connector_with_connector`] wraps
//! this crate's base connector with exactly the same TLS layer
//! `kube::Client::try_from(config)` would have used on the default
//! connector.
//!
//! The rest of the composition (`base_uri_layer`, `auth_layer`,
//! `extra_headers_layer`, a `hyper_timeout::TimeoutConnector`-bounded
//! `hyper_util::client::legacy::Client`, `kube::Client::new`) mirrors
//! `kube::Client::new`'s own documented "advanced use" example exactly
//! (see that method's rustdoc) -- the smallest change that gets a
//! hickory-backed resolver under `kube-client`'s own TLS/auth stack,
//! instead of reimplementing that stack.

use std::net::SocketAddr;
use std::sync::Arc;

use hickory_resolver::TokioResolver;
use hickory_resolver::config::{LookupIpStrategy, ResolveHosts, ResolverConfig};
use hickory_resolver::net::runtime::TokioRuntimeProvider;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::client::legacy::connect::dns::Name;
use hyper_util::rt::TokioExecutor;
use kube::client::ConfigExt as _;
use kube::{Client, Config};
use thiserror::Error;
use tower::{BoxError, Service, ServiceBuilder};

use crate::RuntimeError;

#[derive(Debug, Error)]
enum ResolveError {
    #[error("DNS lookup for {0:?} failed: {1}")]
    LookupFailed(String, String),
    #[error("DNS lookup for {0:?} returned no addresses")]
    NoAddresses(String),
}

/// A `hyper_util` DNS [`Service`] (see that crate's
/// `client::legacy::connect::dns` module docs: "a resolver is just a
/// `Service<Name, Response = impl Iterator<Item = SocketAddr>>`") backed by
/// a `hickory_resolver::TokioResolver`. The port in each returned
/// `SocketAddr` is always `0`; `hyper_util`'s `HttpConnector` overwrites it
/// with the URI's own port afterward (the same contract its own
/// `GaiResolver` follows).
#[derive(Clone)]
struct HickoryResolve {
    resolver: Arc<TokioResolver>,
}

impl Service<Name> for HickoryResolve {
    type Response = std::vec::IntoIter<SocketAddr>;
    type Error = ResolveError;
    type Future = std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<Self::Response, Self::Error>> + Send>,
    >;

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn call(&mut self, name: Name) -> Self::Future {
        let resolver = Arc::clone(&self.resolver);
        Box::pin(async move {
            let host = name.as_str().to_owned();
            let lookup = resolver
                .lookup_ip(&host)
                .await
                .map_err(|error| ResolveError::LookupFailed(host.clone(), error.to_string()))?;
            let addrs: Vec<SocketAddr> = lookup.iter().map(|ip| SocketAddr::new(ip, 0)).collect();
            if addrs.is_empty() {
                return Err(ResolveError::NoAddresses(host));
            }
            Ok(addrs.into_iter())
        })
    }
}

/// Builds a `TokioResolver` from `resolver_config`. Pinned the same way
/// `engine::EgressResolver::build` pins the handler-egress resolver:
/// IPv4-first, and `/etc/hosts` never consulted (`ResolveHosts::Never`).
///
/// Must be called from inside the tokio runtime that will drive it (see
/// `hickory_resolver`'s own `TokioRuntimeProvider` docs, and
/// `engine::EgressResolver`'s doc comment on the same requirement) --
/// `production::run_production` calls this after `startup::start` has
/// already built and entered that runtime, and this resolver, unlike
/// `engine::EgressResolver`'s per-invocation one, lives for that one
/// long-running runtime's whole lifetime, so there is no cross-runtime
/// affinity hazard here.
fn build_resolver(resolver_config: ResolverConfig) -> Result<TokioResolver, RuntimeError> {
    let mut builder =
        TokioResolver::builder_with_config(resolver_config, TokioRuntimeProvider::default());
    builder.options_mut().ip_strategy = LookupIpStrategy::Ipv4thenIpv6;
    builder.options_mut().use_hosts_file = ResolveHosts::Never;
    builder
        .build()
        .map_err(|error| RuntimeError::Configuration(format!("proxy DNS resolver: {error}")))
}

/// Builds the production `kube::Client`. `resolver_config`'s nameservers
/// must already have been read from `/etc/resolv.conf` before Landlock
/// lockdown (see `production::read_production_startup_files`); this
/// function performs no filesystem access at all -- and, once built,
/// nothing this client does (reconnects included) does either.
///
/// # Errors
///
/// Returns an error if the DNS resolver, TLS connector, or auth/header
/// layers fail to build from `kube_config`.
pub(crate) fn build_client(
    kube_config: &Config,
    resolver_config: ResolverConfig,
) -> Result<Client, RuntimeError> {
    let resolver = Arc::new(build_resolver(resolver_config)?);
    let mut base_connector = HttpConnector::new_with_resolver(HickoryResolve { resolver });
    base_connector.enforce_http(false);

    let https_connector = kube_config
        .rustls_https_connector_with_connector(base_connector)
        .map_err(|error| RuntimeError::Configuration(format!("proxy TLS connector: {error}")))?;

    let mut timeout_connector = hyper_timeout::TimeoutConnector::new(https_connector);
    timeout_connector.set_connect_timeout(kube_config.connect_timeout);
    timeout_connector.set_read_timeout(kube_config.read_timeout);
    timeout_connector.set_write_timeout(kube_config.write_timeout);

    let hyper_client: hyper_util::client::legacy::Client<_, kube::client::Body> =
        hyper_util::client::legacy::Client::builder(TokioExecutor::new()).build(timeout_connector);

    let auth_layer = kube_config
        .auth_layer()
        .map_err(|error| RuntimeError::Configuration(format!("proxy auth layer: {error}")))?;
    let extra_headers_layer = kube_config
        .extra_headers_layer()
        .map_err(|error| RuntimeError::Configuration(format!("proxy headers layer: {error}")))?;

    let service = ServiceBuilder::new()
        .layer(kube_config.base_uri_layer())
        .option_layer(auth_layer)
        .layer(extra_headers_layer)
        .map_err(BoxError::from)
        .service(hyper_client);

    Ok(Client::new(service, kube_config.default_namespace.clone()))
}

#[cfg(test)]
mod tests {
    //! `kube_client_performs_a_request_through_the_hickory_connector_after_lockdown`
    //! (see the Apsides v0.3 plan, Phase 5c, item 4): builds a `kube::Client`
    //! from an in-memory `kube::Config` (the same shape
    //! `production::production_kube_config` builds) whose only route to its
    //! target is a hostname, resolves that hostname through this module's
    //! `HickoryResolve`-backed connector against a minimal local DNS stub,
    //! and completes a real HTTPS request against a local
    //! `engine::test_support::HttpsFixtureServer` -- all *after* applying a
    //! real Landlock deny-all lockdown to this test's own thread.
    //!
    //! An ordinary `#[test]` (not `harness = false`, not a separate
    //! process): `landlock::RulesetCreated::restrict_self` on this kernel's
    //! pinned ABI restricts only the *calling* thread
    //! (`RestrictionStatus::all_threads == false`, see
    //! `engine/tests/landlock_fs.rs`'s own doc comment, which established
    //! this exact property is safe under the default per-`#[test]`-thread
    //! harness), so this test's lockdown cannot affect any other
    //! concurrently-running test's thread -- the same safety property
    //! `landlock_denies_worker_thread_open`/`startup_reads_before_lock`
    //! achieve via `harness = false`/a subprocess, achieved here instead by
    //! keeping every bit of I/O this test does (the DNS+HTTPS fixture
    //! servers' own background threads are started *before* lockdown; the
    //! locked-down work after it runs entirely on one
    //! `new_current_thread` tokio runtime, so no additional worker thread
    //! is ever created after lockdown that some other test could
    //! inadvertently share).

    use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

    use engine::test_support::{HttpsFixtureServer, RESOLVABLE_FIXTURE_HOST};
    use hickory_proto::op::{Message, Metadata};
    use hickory_proto::rr::{RData, Record, RecordType, rdata::A};
    use hickory_proto::serialize::binary::BinEncodable;
    use hickory_resolver::config::{ConnectionConfig, NameServerConfig};
    use http::Request;
    use kube::Config;
    use kube::client::Body;

    type TestResult<T = ()> = std::result::Result<T, String>;

    /// [`NameServerConfig::udp_and_tcp`] always dials port 53 (correct for
    /// a real nameserver, which is exactly why `production`'s own
    /// `read_production_startup_files` uses it) -- this test's DNS stub
    /// binds an arbitrary ephemeral port instead, so its config must name
    /// that exact port explicitly.
    fn ns_config(addr: SocketAddr) -> NameServerConfig {
        // Both `NameServerConfig` and `ConnectionConfig` are
        // `#[non_exhaustive]`, so they cannot be struct-literal
        // constructed here even though their fields are `pub`; built via
        // their own default constructors instead, then the one field each
        // that must differ from the default (a real nameserver's port,
        // always 53) is overwritten directly.
        let mut connection = ConnectionConfig::udp();
        connection.port = addr.port();
        let mut ns = NameServerConfig::udp(addr.ip());
        ns.connections = vec![connection];
        ns
    }

    /// A minimal DNS server, single-purpose for this one test: answers
    /// every `A` query (whatever name it asks for -- this stub is only
    /// ever pointed to by this test's own resolver, which only ever asks
    /// for one name, so there is no need to check it) with `answer_ip`,
    /// and NOERROR/zero-answers for any other query type (so a stray
    /// `AAAA` query -- this module's own resolver is pinned
    /// `Ipv4thenIpv6`, so none is expected, but a well-formed empty answer
    /// is more correct than silently dropping the packet either way).
    ///
    /// Runs on its own blocking `std::thread` (started here, before this
    /// test applies Landlock lockdown) for the rest of the test process's
    /// life; there is no shutdown handle since the test binary process
    /// exiting is what stops it.
    fn spawn_dns_stub(answer_ip: Ipv4Addr) -> TestResult<SocketAddr> {
        let socket = UdpSocket::bind("127.0.0.1:0").map_err(|error| error.to_string())?;
        let addr = socket.local_addr().map_err(|error| error.to_string())?;
        std::thread::spawn(move || {
            let mut buf = [0_u8; 512];
            while let Ok((len, from)) = socket.recv_from(&mut buf) {
                let Ok(request) = Message::from_vec(&buf[..len]) else {
                    continue;
                };
                let mut response = Message::response(request.metadata.id, request.metadata.op_code);
                response.metadata = Metadata::response_from_request(&request.metadata);
                response.metadata.recursion_available = true;
                response.add_queries(request.queries.iter().cloned());
                if let Some(query) = request.queries.first()
                    && query.query_type() == RecordType::A
                {
                    response.add_answer(Record::from_rdata(
                        query.name().clone(),
                        60,
                        RData::A(A(answer_ip)),
                    ));
                }
                let Ok(bytes) = response.to_bytes() else {
                    continue;
                };
                let _ignored = socket.send_to(&bytes, from);
            }
        });
        Ok(addr)
    }

    #[test]
    fn kube_client_performs_a_request_through_the_hickory_connector_after_lockdown() -> TestResult {
        // Both fixture servers start their own background thread now,
        // before this test's own thread applies Landlock below -- neither
        // needs filesystem access again afterward (the DNS stub above does
        // no I/O but UDP sockets; `HttpsFixtureServer` already generated
        // its self-signed certificate and bound its listener by the time
        // `start` returns).
        let dns_addr = spawn_dns_stub(Ipv4Addr::LOCALHOST)?;
        let https = HttpsFixtureServer::start(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")
            .map_err(|error| format!("failed to start the HTTPS fixture server: {error}"))?;

        let status =
            engine::apply_deny_all_or_fail_closed(true).map_err(|error| error.to_string())?;
        let enforced = status == engine::LockdownOutcome::Enforced;
        eprintln!("[kube-client-hickory] lockdown status: {status:?} (enforced={enforced})");

        let mut kube_config = Config::new(
            format!("https://{RESOLVABLE_FIXTURE_HOST}:{}", https.addr.port())
                .parse()
                .map_err(|error| format!("{error}"))?,
        );
        kube_config.root_cert = Some(vec![https.ca_der]);
        kube_config.tls_server_name = Some(RESOLVABLE_FIXTURE_HOST.to_owned());
        kube_config.connect_timeout = Some(std::time::Duration::from_secs(5));
        kube_config.read_timeout = Some(std::time::Duration::from_secs(5));

        let resolver_config = hickory_resolver::config::ResolverConfig::from_parts(
            None,
            vec![],
            vec![ns_config(dns_addr)],
        );

        // A `new_current_thread` runtime, not `new_multi_thread`: every
        // task this drives (the client request, the hickory resolver's own
        // background tasks) then runs on this one already-locked-down
        // thread, never spawning a fresh worker thread this test would
        // then need `harness = false` to isolate from other tests -- see
        // this module's own doc comment.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("failed to build tokio runtime: {error}"))?;

        rt.block_on(async move {
            let client = super::build_client(&kube_config, resolver_config)
                .map_err(|error| format!("failed to build the kube client: {error}"))?;
            let request = Request::builder()
                .uri("/")
                .body(Body::empty())
                .map_err(|error| format!("failed to build the request: {error}"))?;
            let response = client.send(request).await.map_err(|error| {
                use std::fmt::Write as _;
                let mut chain = format!("{error}");
                let mut source = std::error::Error::source(&error);
                while let Some(s) = source {
                    let _ = write!(chain, " <- {s}");
                    source = std::error::Error::source(s);
                }
                format!("request through the hickory connector failed: {chain}")
            })?;
            eprintln!(
                "[kube-client-hickory] response status via hickory-resolved DNS + TLS: {}",
                response.status()
            );
            Ok(())
        })
    }
}
