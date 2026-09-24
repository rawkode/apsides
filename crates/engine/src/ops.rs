//! Phase 5b ops: `op_apsides_fetch`, `op_apsides_output_write`,
//! `op_apsides_log`. Exposed to handler code only through the frozen
//! `apsides` global object `BOOTSTRAP_GUARD_SCRIPT` (in `lib.rs`) installs;
//! nothing in this module is reachable by any other name, and nothing here
//! trusts a caller-supplied `Host`/`Content-Length`/`Transfer-Encoding`/etc.
//! header, follows a redirect, or writes to an output id the caller did
//! not declare.
//!
//! Every fallible op below is infallible at the `#[op2]` boundary itself:
//! each returns a `#[serde]`-tagged `..Outcome` enum (`Ok { .. }` or
//! `Error { code, message }`) rather than a `Result<T, E>` thrown through
//! `deno_core`'s own error machinery. `code` is always the `Display`-paired
//! variant of a dedicated Rust error enum (`FetchError`/`OutputWriteError`),
//! serialized as a fixed `SCREAMING_SNAKE_CASE` identifier -- the "thiserror
//! enum, surfaced to JS as an Error with `.code`" the plan asks for -- and
//! `message` is that error's `Display` text. `BOOTSTRAP_GUARD_SCRIPT`'s
//! `apsides.*` wrappers turn an `Error { .. }` outcome into a thrown JS
//! `Error` with `.code` set from it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use deno_core::OpState;
use deno_core::{JsBuffer, ToJsBuffer, op2};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::net;
use crate::{EgressPolicy, EgressResolver, LogLevel, LogLine, OutputSink, OutputSinkError};

/// Per-invocation state the ops above read from `OpState`. Built once by
/// `crate::prepare_isolate` from the caller's `HandlerInvocation` and
/// mutated only by the ops themselves (`logs`, `outputs_written`,
/// `fetched_bytes_used`) and by `crate::call_handler_export` (`closed`,
/// set once the handler's own promise has settled -- see that function's
/// doc comment).
pub(crate) struct EngineOpState {
    pub egress: EgressPolicy,
    /// Declared output id -> its `max_bytes` cap.
    pub outputs: HashMap<String, usize>,
    /// Declared output id -> (key -> `key.len() + data.len()` for the
    /// most recent write of that key). The cap in `outputs` bounds the sum
    /// of this inner map's values, not any one write.
    pub outputs_written: HashMap<String, HashMap<String, usize>>,
    pub sink: Arc<dyn OutputSink>,
    pub resolver: Option<Arc<EgressResolver>>,
    /// Absolute deadline for the whole invocation (`Instant::now() +
    /// Limits::timeout` at invocation start), forwarded to DNS resolution
    /// and `net::FetchLimits::deadline` so a fetch's own "total time <=
    /// timeout" budget can never outlive the invocation's watchdog-enforced
    /// one.
    pub deadline: Instant,
    pub max_fetch_bytes_total: usize,
    pub fetched_bytes_used: usize,
    pub handler_id: String,
    pub invocation_id: String,
    pub logs: Vec<LogLine>,
    /// Set once, by `crate::call_handler_export`, after the handler's own
    /// promise has resolved but before its return value is marshalled back
    /// to Rust -- see that function's doc comment for why. Every op checks
    /// this first and fails fast once it is set.
    pub closed: bool,
}

/// See the plan's `op_apsides_log` rule.
const MAX_LOG_LINE_BYTES: usize = 4 * 1024;
/// See the plan's `op_apsides_log` rule.
const MAX_LOG_LINES: usize = 64;
/// See the plan's `op_apsides_fetch` rule.
const MAX_FETCH_REQUEST_BODY_BYTES: usize = 1024 * 1024;
/// See the plan's `op_apsides_fetch` rule.
const MAX_FETCH_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
/// Output key length cap (review round 5, B7).
const MAX_OUTPUT_KEY_LEN: usize = 253;

// ---------------------------------------------------------------------
// op_apsides_log
// ---------------------------------------------------------------------

fn parse_log_level(level: &str) -> LogLevel {
    match level {
        "debug" => LogLevel::Debug,
        "warn" => LogLevel::Warn,
        "error" => LogLevel::Error,
        // The `apsides.log` JS wrapper validates `level` against exactly
        // these four strings before ever calling this op (see
        // `BOOTSTRAP_GUARD_SCRIPT`); an unrecognized value here can only
        // mean a future wrapper bug, and this op must never panic over
        // that -- default to `Info` rather than reject the line outright.
        _other => LogLevel::Info,
    }
}

#[derive(Serialize)]
struct StderrLogRecord<'a> {
    handler_id: &'a str,
    invocation_id: &'a str,
    level: &'a LogLevel,
    message: &'a str,
}

/// Appends one log line, truncated (at a UTF-8 char boundary, never
/// splitting a code point) to at most [`MAX_LOG_LINE_BYTES`], and -- at op
/// time, unconditionally, before the in-memory cap below is even checked
/// -- emits it to stderr as one `serde_json`-escaped JSON object per line,
/// tagged with this invocation's `handler_id`/`invocation_id`. The
/// stderr line is built entirely in Rust from already-validated/truncated
/// fields, so a handler cannot forge or inject an extra line into it.
///
/// Once [`MAX_LOG_LINES`] lines have been recorded in `EngineOpState.logs`
/// for this invocation, further calls still reach stderr but are silently
/// dropped from the in-memory/`InvocationReport` copy: logging must never
/// be able to fail a handler that is otherwise working correctly, and the
/// in-memory cap exists only to bound `InvocationReport`'s own size, not
/// to bound what reaches stderr.
#[op2(fast)]
pub fn op_apsides_log(state: &mut OpState, #[string] level: &str, #[string] line: &str) {
    let engine_state = state.borrow_mut::<EngineOpState>();
    if engine_state.closed {
        return;
    }

    let level = parse_log_level(level);
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

    if let Ok(record) = serde_json::to_string(&StderrLogRecord {
        handler_id: &engine_state.handler_id,
        invocation_id: &engine_state.invocation_id,
        level: &level,
        message: &truncated,
    }) {
        eprintln!("{record}");
    }

    if engine_state.logs.len() < MAX_LOG_LINES {
        engine_state.logs.push(LogLine {
            level,
            message: truncated,
        });
    }
}

// ---------------------------------------------------------------------
// op_apsides_output_write
// ---------------------------------------------------------------------

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum OutputWriteErrorCode {
    OpsClosed,
    UndeclaredOutputId,
    InvalidKey,
    DataTooLarge,
    SinkRejected,
}

#[derive(Debug, Error)]
pub enum OutputWriteError {
    #[error("apsides.writeOutput is no longer available: the handler has already returned")]
    OpsClosed,
    #[error("output id {0:?} was not declared for this invocation")]
    UndeclaredOutputId(String),
    #[error(
        "key {0:?} is invalid: it must match [-._a-zA-Z0-9]+, be at most {MAX_OUTPUT_KEY_LEN} \
         characters, and never be exactly \".\" or \"..\""
    )]
    InvalidKey(String),
    #[error(
        "writing {0} would bring output {2:?}'s total (key + data bytes, across every key) to \
         {1} bytes, over its declared cap"
    )]
    DataTooLarge(usize, usize, String),
    #[error("output sink rejected the write: {0}")]
    SinkRejected(#[from] OutputSinkError),
}

impl OutputWriteError {
    const fn code(&self) -> OutputWriteErrorCode {
        match self {
            Self::OpsClosed => OutputWriteErrorCode::OpsClosed,
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

/// `[-._a-zA-Z0-9]+`, at most [`MAX_OUTPUT_KEY_LEN`] characters, never
/// exactly `.` or `..` (checked byte-wise; the pattern is ASCII-only).
fn is_valid_output_key(key: &str) -> bool {
    if key.is_empty() || key.len() > MAX_OUTPUT_KEY_LEN || key == "." || key == ".." {
        return false;
    }
    key.bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_'))
}

fn write_output(
    state: &mut OpState,
    id: &str,
    key: &str,
    data: &[u8],
) -> Result<(), OutputWriteError> {
    let engine_state = state.borrow_mut::<EngineOpState>();
    if engine_state.closed {
        return Err(OutputWriteError::OpsClosed);
    }
    let Some(&max_bytes) = engine_state.outputs.get(id) else {
        return Err(OutputWriteError::UndeclaredOutputId(id.to_owned()));
    };
    if !is_valid_output_key(key) {
        return Err(OutputWriteError::InvalidKey(key.to_owned()));
    }

    let entry_size = key.len() + data.len();
    let existing = engine_state
        .outputs_written
        .entry(id.to_owned())
        .or_default();
    let previous_for_this_key = existing.get(key).copied().unwrap_or(0);
    let total_excluding_this_key = existing.values().sum::<usize>() - previous_for_this_key;
    let new_total = total_excluding_this_key.saturating_add(entry_size);
    if new_total > max_bytes {
        return Err(OutputWriteError::DataTooLarge(
            entry_size,
            new_total,
            id.to_owned(),
        ));
    }

    engine_state
        .sink
        .write_output(id, key, data)
        .map_err(OutputWriteError::from)?;

    engine_state
        .outputs_written
        .entry(id.to_owned())
        .or_default()
        .insert(key.to_owned(), entry_size);
    Ok(())
}

#[op2]
#[serde]
pub fn op_apsides_output_write(
    state: &mut OpState,
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

/// Request headers no handler may ever set directly -- each is either
/// derived automatically from the vetted connection (`Host`), describes
/// the request body in a way only this client may set correctly
/// (`Content-Length`/`Transfer-Encoding`), or governs connection-level
/// framing/upgrade semantics this crate's one-shot HTTP/1.1 client does
/// not support and must not let a handler quietly desynchronize
/// (`Connection`/`TE`/`Upgrade`/`Trailer`). Checked case-insensitively.
const FORBIDDEN_REQUEST_HEADERS: &[&str] = &[
    "host",
    "content-length",
    "transfer-encoding",
    "connection",
    "te",
    "upgrade",
    "trailer",
];

#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum FetchErrorCode {
    OpsClosed,
    SchemeNotHttps,
    InvalidUrl,
    HostIsIpLiteral,
    HostNotAllowed,
    ForbiddenRequestHeader,
    InvalidRequestHeader,
    InvalidRequestMethod,
    RequestBodyTooLarge,
    FetchByteBudgetExceeded,
    DnsResolutionFailed,
    NoGlobalUnicastAddress,
    ConnectFailed,
    ConnectTimeout,
    TlsHandshakeFailed,
    HttpHandshakeFailed,
    RequestSendFailed,
    ResponseHeadersTooLarge,
    InvalidResponseHeader,
    ResponseTooLarge,
    ResponseBodyFailed,
    Timeout,
}

#[derive(Debug, Error)]
pub enum FetchError {
    #[error("apsides.fetch is no longer available: the handler has already returned")]
    OpsClosed,
    #[error("URL scheme must be https")]
    SchemeNotHttps,
    #[error("invalid URL: {0}")]
    InvalidUrl(String),
    #[error(
        "URL host must be a DNS name, not an IP literal ({0}); apsides.fetch resolves a \
         declared hostname itself and refuses to bypass that"
    )]
    HostIsIpLiteral(String),
    #[error("{0}:{1} is not on the egress allowlist")]
    HostNotAllowed(String, u16),
    #[error("request header {0:?} is not permitted; it is set automatically or never allowed")]
    ForbiddenRequestHeader(String),
    #[error("request body is {0} bytes, over the {1} byte limit")]
    RequestBodyTooLarge(usize, usize),
    #[error(
        "this invocation's cumulative apsides.fetch response budget \
         (Limits::max_fetch_bytes_total) is exhausted"
    )]
    FetchByteBudgetExceeded,
    #[error("DNS resolution failed: {0}")]
    DnsResolutionFailed(#[from] net::ResolveError),
    #[error(transparent)]
    Https(#[from] net::HttpsError),
}

impl FetchError {
    const fn code(&self) -> FetchErrorCode {
        match self {
            Self::OpsClosed => FetchErrorCode::OpsClosed,
            Self::SchemeNotHttps => FetchErrorCode::SchemeNotHttps,
            Self::InvalidUrl(_) => FetchErrorCode::InvalidUrl,
            Self::HostIsIpLiteral(_) => FetchErrorCode::HostIsIpLiteral,
            Self::HostNotAllowed(..) => FetchErrorCode::HostNotAllowed,
            Self::ForbiddenRequestHeader(_) => FetchErrorCode::ForbiddenRequestHeader,
            Self::RequestBodyTooLarge(..) => FetchErrorCode::RequestBodyTooLarge,
            Self::FetchByteBudgetExceeded => FetchErrorCode::FetchByteBudgetExceeded,
            Self::DnsResolutionFailed(_) => FetchErrorCode::DnsResolutionFailed,
            Self::Https(err) => match err {
                net::HttpsError::Dial(dial) => match dial {
                    net::DialError::NoGlobalUnicastAddress => {
                        FetchErrorCode::NoGlobalUnicastAddress
                    }
                    net::DialError::Timeout => FetchErrorCode::ConnectTimeout,
                    net::DialError::AllAttemptsFailed(_) => FetchErrorCode::ConnectFailed,
                },
                net::HttpsError::CryptoProvider(_)
                | net::HttpsError::InvalidServerName(..)
                | net::HttpsError::TlsHandshake(..) => FetchErrorCode::TlsHandshakeFailed,
                net::HttpsError::InvalidMethod(_) => FetchErrorCode::InvalidRequestMethod,
                net::HttpsError::InvalidHeader(_) => FetchErrorCode::InvalidRequestHeader,
                net::HttpsError::HttpHandshake(_) => FetchErrorCode::HttpHandshakeFailed,
                net::HttpsError::RequestSend(_) => FetchErrorCode::RequestSendFailed,
                net::HttpsError::ResponseHeadersTooLarge(..) => {
                    FetchErrorCode::ResponseHeadersTooLarge
                }
                net::HttpsError::InvalidResponseHeader => FetchErrorCode::InvalidResponseHeader,
                net::HttpsError::ResponseTooLarge(_) => FetchErrorCode::ResponseTooLarge,
                net::HttpsError::ResponseBody(_) => FetchErrorCode::ResponseBodyFailed,
                net::HttpsError::Timeout => FetchErrorCode::Timeout,
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
        body: ToJsBuffer,
    },
    Error {
        code: FetchErrorCode,
        message: String,
    },
}

#[allow(
    clippy::future_not_send,
    reason = "deno_core::OpState/JsRuntime are deliberately !Send (single isolate, single thread); see crate::invoke_handler_with_watchdog's doc comment"
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
    for (name, _value) in &request.headers {
        if FORBIDDEN_REQUEST_HEADERS
            .iter()
            .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
        {
            return Err(FetchError::ForbiddenRequestHeader(name.clone()));
        }
    }

    let (egress, deadline, resolver, closed, remaining_fetch_budget) = {
        let state_ref = state.borrow();
        let engine_state = state_ref.borrow::<EngineOpState>();
        (
            engine_state.egress.clone(),
            engine_state.deadline,
            engine_state.resolver.clone(),
            engine_state.closed,
            engine_state
                .max_fetch_bytes_total
                .saturating_sub(engine_state.fetched_bytes_used),
        )
    };
    if closed {
        return Err(FetchError::OpsClosed);
    }
    if remaining_fetch_budget == 0 {
        return Err(FetchError::FetchByteBudgetExceeded);
    }

    let parsed = deno_core::url::Url::parse(&request.url)
        .map_err(|err| FetchError::InvalidUrl(err.to_string()))?;
    if parsed.scheme() != "https" {
        return Err(FetchError::SchemeNotHttps);
    }
    // `parsed.host()`, not `host_str()`: a bracketed IPv6 literal
    // (`https://[::1]/`) parses to `Host::Ipv6` either way, but matching
    // on the typed `Host` (rather than re-parsing the string form) is
    // what makes the IP-literal rejection below exhaustive and structural,
    // not a second, possibly-divergent parse.
    let host = match parsed.host() {
        Some(deno_core::url::Host::Domain(domain)) => domain.to_owned(),
        Some(deno_core::url::Host::Ipv4(ip)) => {
            return Err(FetchError::HostIsIpLiteral(ip.to_string()));
        }
        Some(deno_core::url::Host::Ipv6(ip)) => {
            return Err(FetchError::HostIsIpLiteral(format!("[{ip}]")));
        }
        None => return Err(FetchError::InvalidUrl("URL has no host".to_owned())),
    };
    let port = parsed.port_or_known_default().unwrap_or(443);

    if !egress
        .allowed
        .iter()
        .any(|(allowed_host, allowed_port)| *allowed_host == host && *allowed_port == port)
    {
        return Err(FetchError::HostNotAllowed(host, port));
    }

    let candidates = net::resolve_host(
        &host,
        egress.resolver_override.as_ref(),
        resolver.as_deref(),
        deadline,
    )
    .await?;

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
        max_response_bytes: remaining_fetch_budget.min(MAX_FETCH_RESPONSE_BYTES),
        deadline,
    };

    let response = net::https_request(
        &candidates,
        port,
        &host,
        &request.method,
        &path_and_query,
        &request.headers,
        body,
        &fetch_limits,
        extra_trust_anchor.as_ref(),
    )
    .await?;

    {
        let mut state_ref = state.borrow_mut();
        let engine_state = state_ref.borrow_mut::<EngineOpState>();
        engine_state.fetched_bytes_used = engine_state
            .fetched_bytes_used
            .saturating_add(response.body.len());
    }

    Ok((response.status, response.headers, response.body))
}

#[allow(
    clippy::future_not_send,
    reason = "deno_core::OpState/JsRuntime are deliberately !Send (single isolate, single thread); see crate::invoke_handler_with_watchdog's doc comment"
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
            body: body.into(),
        },
        Err(err) => FetchOutcome::Error {
            code: err.code(),
            message: err.to_string(),
        },
    }
}

/// The two caller-supplied identifiers `apsides.log` attaches to every
/// stderr line (see `EngineOpState`/`StderrLogRecord`). Bundled into one
/// type so the `extension!` macro below (which becomes an `init` function
/// clippy counts arguments on) stays at 7 options, not 8.
pub(crate) struct InvocationIds {
    pub handler_id: String,
    pub invocation_id: String,
}

deno_core::extension!(
    apsides_ops,
    ops = [op_apsides_fetch, op_apsides_output_write, op_apsides_log],
    options = {
        egress: EgressPolicy,
        outputs: HashMap<String, usize>,
        sink: Arc<dyn OutputSink>,
        resolver: Option<Arc<EgressResolver>>,
        deadline: Instant,
        max_fetch_bytes_total: usize,
        ids: InvocationIds,
    },
    state = |state, options| {
        state.put(EngineOpState {
            egress: options.egress,
            outputs: options.outputs,
            outputs_written: HashMap::new(),
            sink: options.sink,
            resolver: options.resolver,
            deadline: options.deadline,
            max_fetch_bytes_total: options.max_fetch_bytes_total,
            fetched_bytes_used: 0,
            handler_id: options.ids.handler_id,
            invocation_id: options.ids.invocation_id,
            logs: Vec::new(),
            closed: false,
        });
    },
);
