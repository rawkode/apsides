//! Phase 5b ops: `op_apsides_fetch`, `op_apsides_output_write`,
//! `op_apsides_log`. Exposed to handler code only through the frozen
//! `apsides` global object `BOOTSTRAP_GUARD_SCRIPT` (in `lib.rs`) installs;
//! nothing in this module is reachable by any other name, and nothing here
//! trusts a caller-supplied `Host` header, follows a redirect, or writes to
//! an output id the caller did not declare.
//!
//! Every fallible op below is infallible at the `#[op2]` boundary itself:
//! each returns a `#[serde]`-tagged `..Outcome`/`..Result` enum (`Ok { .. }`
//! or `Error { code, message }`) rather than a `Result<T, E>` thrown through
//! `deno_core`'s own error machinery. `code` is always the `Display`-paired
//! variant of a dedicated Rust error enum (`FetchError`/`OutputWriteError`),
//! serialized as a fixed `SCREAMING_SNAKE_CASE` identifier -- the "thiserror
//! enum, surfaced to JS as an Error with `.code`" the plan asks for -- and
//! `message` is that error's `Display` text. `BOOTSTRAP_GUARD_SCRIPT`'s
//! `apsides.*` wrappers turn an `Error { .. }` outcome into a thrown JS
//! `Error` with `.code` set from it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::net::IpAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use deno_core::JsBuffer;
use deno_core::OpState;
use deno_core::op2;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{EgressPolicy, OutputSink, OutputSinkError};
use crate::net;

/// Per-invocation state the ops above read from `OpState`. Built once by
/// `crate::prepare_isolate` from the caller's `HandlerInvocation` and never
/// replaced; `logs` is the only field any op mutates.
pub(crate) struct EngineOpState {
    pub egress: EgressPolicy,
    /// Declared output id -> its `max_bytes` cap.
    pub outputs: HashMap<String, usize>,
    pub sink: Arc<dyn OutputSink>,
    /// Absolute deadline for the whole invocation (`Instant::now() +
    /// Limits::timeout` at invocation start), forwarded to
    /// `net::FetchLimits::deadline` so a fetch's own "total time <=
    /// timeout" budget can never outlive the invocation's watchdog-enforced
    /// one.
    pub deadline: Instant,
    pub logs: Vec<String>,
}

/// See the plan's `op_apsides_log` rule.
const MAX_LOG_LINE_BYTES: usize = 4 * 1024;
/// See the plan's `op_apsides_log` rule.
const MAX_LOG_LINES: usize = 64;
/// See the plan's `op_apsides_fetch` rule.
const MAX_FETCH_REQUEST_BODY_BYTES: usize = 1024 * 1024;
/// See the plan's `op_apsides_fetch` rule.
const MAX_FETCH_RESPONSE_BYTES: usize = 4 * 1024 * 1024;

// ---------------------------------------------------------------------
// op_apsides_log
// ---------------------------------------------------------------------

/// Appends one log line, truncated (at a UTF-8 char boundary, never
/// splitting a code point) to at most [`MAX_LOG_LINE_BYTES`]. Once
/// [`MAX_LOG_LINES`] lines have been recorded for this invocation, further
/// calls are silently dropped rather than throwing: logging must never be
/// able to fail a handler that is otherwise working correctly.
#[op2(fast)]
pub fn op_apsides_log(state: &mut OpState, #[string] line: &str) {
    let engine_state = state.borrow_mut::<EngineOpState>();
    if engine_state.logs.len() >= MAX_LOG_LINES {
        return;
    }
    let mut truncated = String::new();
    let mut byte_len = 0_usize;
    for ch in line.chars() {
        let ch_len = ch.len_utf8();
        if byte_len + ch_len > MAX_LOG_LINE_BYTES {
            break;
        }
        truncated.push(ch);
        byte_len += ch_len;
    }
    engine_state.logs.push(truncated);
}

// ---------------------------------------------------------------------
// op_apsides_output_write
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutputWriteErrorCode {
    UndeclaredOutputId,
    InvalidKey,
    DataTooLarge,
    SinkRejected,
}

#[derive(Debug, Error)]
pub enum OutputWriteError {
    #[error("output id {0:?} was not declared for this invocation")]
    UndeclaredOutputId(String),
    #[error("key {0:?} does not match the required pattern [-._a-zA-Z0-9]+")]
    InvalidKey(String),
    #[error("data is {0} bytes, over the declared {1} byte cap for output {2:?}")]
    DataTooLarge(usize, usize, String),
    #[error("output sink rejected the write: {0}")]
    SinkRejected(#[from] OutputSinkError),
}

impl OutputWriteError {
    const fn code(&self) -> OutputWriteErrorCode {
        match self {
            Self::UndeclaredOutputId(_) => OutputWriteErrorCode::UndeclaredOutputId,
            Self::InvalidKey(_) => OutputWriteErrorCode::InvalidKey,
            Self::DataTooLarge(..) => OutputWriteErrorCode::DataTooLarge,
            Self::SinkRejected(_) => OutputWriteErrorCode::SinkRejected,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum OutputWriteOutcome {
    Ok,
    Error {
        code: OutputWriteErrorCode,
        message: String,
    },
}

/// `[-._a-zA-Z0-9]+`, checked byte-wise (the pattern is ASCII-only).
fn is_valid_output_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

fn write_output(
    state: &OpState,
    id: &str,
    key: &str,
    data: &[u8],
) -> Result<(), OutputWriteError> {
    let engine_state = state.borrow::<EngineOpState>();
    let Some(&max_bytes) = engine_state.outputs.get(id) else {
        return Err(OutputWriteError::UndeclaredOutputId(id.to_owned()));
    };
    if !is_valid_output_key(key) {
        return Err(OutputWriteError::InvalidKey(key.to_owned()));
    }
    if data.len() > max_bytes {
        return Err(OutputWriteError::DataTooLarge(
            data.len(),
            max_bytes,
            id.to_owned(),
        ));
    }
    engine_state
        .sink
        .write_output(id, key, data)
        .map_err(OutputWriteError::from)
}

#[op2]
#[serde]
pub fn op_apsides_output_write(
    state: &OpState,
    #[string] id: &str,
    #[string] key: &str,
    #[buffer] data: &[u8],
) -> OutputWriteOutcome {
    match write_output(state, id, key, data) {
        Ok(()) => OutputWriteOutcome::Ok,
        Err(err) => OutputWriteOutcome::Error {
            code: err.code(),
            message: err.to_string(),
        },
    }
}

// ---------------------------------------------------------------------
// op_apsides_fetch
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FetchErrorCode {
    SchemeNotHttps,
    InvalidUrl,
    HostIsIpLiteral,
    HostNotAllowed,
    HostHeaderNotPermitted,
    DnsResolutionFailed,
    AddressNotGlobalUnicast,
    RequestBodyTooLarge,
    ConnectFailed,
    TlsHandshakeFailed,
    ResponseTooLarge,
    RedirectNotFollowed,
    Timeout,
    MalformedResponse,
}

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("URL scheme must be https")]
    SchemeNotHttps,
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error(
        "URL host must be a DNS name, not an IP literal ({0}); apsides.fetch resolves a \
         declared hostname itself and refuses to bypass that"
    )]
    HostIsIpLiteral(IpAddr),
    #[error("{0}:{1} is not on the egress allowlist")]
    HostNotAllowed(String, u16),
    #[error(
        "caller-supplied Host header is not permitted; the declared host is set automatically"
    )]
    HostHeaderNotPermitted,
    #[error("DNS resolution failed: {0}")]
    DnsResolutionFailed(#[from] net::ResolveError),
    #[error("resolved address {0} is not a global unicast address")]
    AddressNotGlobalUnicast(IpAddr),
    #[error("request body is {0} bytes, over the {1} byte limit")]
    RequestBodyTooLarge(usize, usize),
    #[error(transparent)]
    Https(#[from] net::HttpsError),
}

impl FetchError {
    const fn code(&self) -> FetchErrorCode {
        match self {
            Self::SchemeNotHttps => FetchErrorCode::SchemeNotHttps,
            Self::InvalidUrl(_) => FetchErrorCode::InvalidUrl,
            Self::HostIsIpLiteral(_) => FetchErrorCode::HostIsIpLiteral,
            Self::HostNotAllowed(..) => FetchErrorCode::HostNotAllowed,
            Self::HostHeaderNotPermitted => FetchErrorCode::HostHeaderNotPermitted,
            Self::DnsResolutionFailed(_) => FetchErrorCode::DnsResolutionFailed,
            Self::AddressNotGlobalUnicast(_) => FetchErrorCode::AddressNotGlobalUnicast,
            Self::RequestBodyTooLarge(..) => FetchErrorCode::RequestBodyTooLarge,
            Self::Https(err) => match err {
                net::HttpsError::ResponseTooLarge(_) => FetchErrorCode::ResponseTooLarge,
                net::HttpsError::RedirectNotFollowed => FetchErrorCode::RedirectNotFollowed,
                net::HttpsError::Timeout => FetchErrorCode::Timeout,
                net::HttpsError::Connect(..) => FetchErrorCode::ConnectFailed,
                net::HttpsError::TlsHandshake(..)
                | net::HttpsError::InvalidServerName(..)
                | net::HttpsError::CryptoProvider(_) => FetchErrorCode::TlsHandshakeFailed,
                net::HttpsError::Write(_)
                | net::HttpsError::Read(_)
                | net::HttpsError::MalformedStatusLine
                | net::HttpsError::MalformedHeaders => FetchErrorCode::MalformedResponse,
            },
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct FetchRequestArgs {
    pub url: String,
    #[serde(default = "default_fetch_method")]
    pub method: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
}

fn default_fetch_method() -> String {
    "GET".to_owned()
}

#[derive(Debug, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum FetchOutcome {
    Ok {
        status: u16,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    },
    Error {
        code: FetchErrorCode,
        message: String,
    },
}

#[allow(
    clippy::future_not_send,
    reason = "deno_core::OpState/JsRuntime are deliberately !Send (single isolate, single thread); see crate::invoke_handler's doc comment"
)]
async fn fetch(
    state: &Rc<RefCell<OpState>>,
    request: FetchRequestArgs,
    body: Vec<u8>,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>), FetchError> {
    if body.len() > MAX_FETCH_REQUEST_BODY_BYTES {
        return Err(FetchError::RequestBodyTooLarge(
            body.len(),
            MAX_FETCH_REQUEST_BODY_BYTES,
        ));
    }
    if request
        .headers
        .iter()
        .any(|(name, _value)| name.eq_ignore_ascii_case("host"))
    {
        return Err(FetchError::HostHeaderNotPermitted);
    }

    let (egress, deadline) = {
        let state_ref = state.borrow();
        let engine_state = state_ref.borrow::<EngineOpState>();
        (engine_state.egress.clone(), engine_state.deadline)
    };

    let parsed = deno_core::url::Url::parse(&request.url)
        .map_err(|err| FetchError::InvalidUrl(err.to_string()))?;
    if parsed.scheme() != "https" {
        return Err(FetchError::SchemeNotHttps);
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| FetchError::InvalidUrl("URL has no host".to_owned()))?
        .to_owned();
    let port = parsed.port_or_known_default().unwrap_or(443);

    // The whole design is "resolve a declared *name*, then vet the
    // resolved address" (SNI and the outgoing Host header are the parsed
    // name, never an address) -- a URL that already names a bare IP
    // literal has no name to declare, so it is rejected outright, even if
    // that literal happens to appear in the allowlist.
    if let Ok(ip_literal) = host.parse::<IpAddr>() {
        return Err(FetchError::HostIsIpLiteral(ip_literal));
    }

    if !egress
        .allowed
        .iter()
        .any(|(allowed_host, allowed_port)| *allowed_host == host && *allowed_port == port)
    {
        return Err(FetchError::HostNotAllowed(host, port));
    }

    let resolved_ip = net::resolve_host(&host, egress.resolver_override.as_ref()).await?;
    // The global-unicast check is skipped only in fixture mode
    // (`fixture_trust_anchor` set, already validated to carry the required
    // acknowledgement before this isolate was ever built -- see
    // `validate_fixture_acknowledgement`): a local HTTPS test server binds
    // to a loopback address by construction, which the check would
    // otherwise always reject. Bundled behind the same acknowledgement as
    // the extra trust anchor, not a separate flag, since both are the same
    // "this is a disposable test fixture, not a production destination"
    // relaxation.
    if egress.fixture_trust_anchor.is_none() && !net::is_global_unicast(resolved_ip) {
        return Err(FetchError::AddressNotGlobalUnicast(resolved_ip));
    }
    let addr = std::net::SocketAddr::new(resolved_ip, port);

    let mut path_and_query = parsed.path().to_owned();
    if let Some(query) = parsed.query() {
        path_and_query.push('?');
        path_and_query.push_str(query);
    }

    let extra_trust_anchor = egress
        .fixture_trust_anchor
        .as_ref()
        .map(|fixture| net::FixtureTrustAnchor {
            der: fixture.trust_anchor_der.clone(),
        });
    let fetch_limits = net::FetchLimits {
        max_response_bytes: MAX_FETCH_RESPONSE_BYTES,
        deadline,
    };

    let response = net::https_request(
        addr,
        &host,
        &request.method,
        &path_and_query,
        &request.headers,
        &body,
        &fetch_limits,
        extra_trust_anchor.as_ref(),
    )
    .await?;

    Ok((response.status, response.headers, response.body))
}

#[allow(
    clippy::future_not_send,
    reason = "deno_core::OpState/JsRuntime are deliberately !Send (single isolate, single thread); see crate::invoke_handler's doc comment"
)]
#[op2]
#[serde]
pub async fn op_apsides_fetch(
    state: Rc<RefCell<OpState>>,
    #[serde] request: FetchRequestArgs,
    #[buffer] body: JsBuffer,
) -> FetchOutcome {
    let body_vec = body.to_vec();
    match fetch(&state, request, body_vec).await {
        Ok((status, headers, body)) => FetchOutcome::Ok {
            status,
            headers,
            body,
        },
        Err(err) => FetchOutcome::Error {
            code: err.code(),
            message: err.to_string(),
        },
    }
}

deno_core::extension!(
    apsides_ops,
    ops = [op_apsides_fetch, op_apsides_output_write, op_apsides_log],
    options = {
        egress: EgressPolicy,
        outputs: HashMap<String, usize>,
        sink: Arc<dyn OutputSink>,
        deadline: Instant,
    },
    state = |state, options| {
        state.put(EngineOpState {
            egress: options.egress,
            outputs: options.outputs,
            sink: options.sink,
            deadline: options.deadline,
            logs: Vec::new(),
        });
    },
);
