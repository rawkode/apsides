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
use hickory_resolver::TokioResolver;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::net;
use crate::{
    EgressPolicy, EgressResolver, LogLevel, LogLine, LogWriter, OutputSink, OutputSinkError,
};

/// Per-invocation state the ops above read from `OpState`. Built once by
/// `crate::prepare_isolate` from the caller's `HandlerInvocation` and
/// mutated only by the ops themselves (`logs`, `outputs_written`,
/// `fetch_bytes_reserved`, `fetches_in_flight`, `dropped_log_lines`) and by
/// `crate::call_handler_export` (`closed`, set once the handler's own
/// promise has settled -- see that function's doc comment).
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
    /// The real `TokioResolver` [`resolver`](Self::resolver)'s config
    /// builds, lazily, the first time this invocation actually needs to
    /// resolve a host -- see `EgressResolver`'s doc comment on why this
    /// crate never builds one before that point, and never reuses one
    /// across invocations. `None` until the first `apsides.fetch` call
    /// that needs real DNS resolution; built once per invocation and
    /// reused for every later call within the *same* invocation (still the
    /// same runtime the whole time).
    pub built_resolver: Option<TokioResolver>,
    /// Absolute deadline for the whole invocation (`Instant::now() +
    /// Limits::timeout` at invocation start), forwarded to DNS resolution
    /// and `net::FetchLimits::deadline` so a fetch's own "total time <=
    /// timeout" budget can never outlive the invocation's watchdog-enforced
    /// one.
    pub deadline: Instant,
    pub max_fetch_bytes_total: usize,
    /// Bytes of `max_fetch_bytes_total` currently reserved: the sum, over
    /// every `apsides.fetch` call that has started and not yet released
    /// its reservation, of that call's request body length plus its
    /// worst-case response allowance (see `fetch`'s doc comment). Reserved
    /// synchronously before any `.await`, refunded down to actual usage on
    /// success, and left fully charged on failure (review round 6, N1) --
    /// this is a *reservation* ledger, not a "bytes actually seen so far"
    /// counter.
    pub fetch_bytes_reserved: usize,
    /// `apsides.fetch` calls with a reservation currently held (started,
    /// not yet finished). Bounded by `max_concurrent_fetches`.
    pub fetches_in_flight: usize,
    pub max_concurrent_fetches: usize,
    pub handler_id: String,
    pub invocation_id: String,
    pub logs: Vec<LogLine>,
    /// Lines a handler attempted to log past `MAX_LOG_LINES`, dropped from
    /// both `logs` and stderr -- see `op_apsides_log`. Reported as one
    /// final stderr record by `emit_dropped_log_record_if_needed`, called
    /// once the invocation finishes.
    pub dropped_log_lines: usize,
    pub log_writer: Arc<dyn LogWriter>,
    /// Set once, by `crate::call_handler_export`, after the handler's own
    /// promise has resolved but before its return value is marshalled back
    /// to Rust -- see that function's doc comment for why. Every op checks
    /// this first and fails fast once it is set.
    pub closed: bool,
}

#[derive(Serialize)]
struct DroppedLogRecord<'a> {
    handler_id: &'a str,
    invocation_id: &'a str,
    level: &'static str,
    dropped_log_lines: usize,
}

impl EngineOpState {
    /// Emits one final `serde_json`-escaped stderr record
    /// (`{"handler_id","invocation_id","level":"warn","dropped_log_lines"}`)
    /// if any `apsides.log` lines were dropped for exceeding
    /// [`MAX_LOG_LINES`] this invocation. Called once, by
    /// `crate::collect_logs`, after the invocation has finished -- the
    /// final count is only known once no more `apsides.log` calls can
    /// happen (see `EngineOpState::closed`).
    pub(crate) fn emit_dropped_log_record_if_needed(&self) {
        if self.dropped_log_lines == 0 {
            return;
        }
        if let Ok(record) = serde_json::to_string(&DroppedLogRecord {
            handler_id: &self.handler_id,
            invocation_id: &self.invocation_id,
            level: "warn",
            dropped_log_lines: self.dropped_log_lines,
        }) {
            self.log_writer.write_line(&record);
        }
    }
}

/// See the plan's `op_apsides_log` rule.
const MAX_LOG_LINE_BYTES: usize = 4 * 1024;
/// See the plan's `op_apsides_log` rule.
const MAX_LOG_LINES: usize = 64;
/// See the plan's `op_apsides_fetch` rule.
pub const MAX_FETCH_REQUEST_BODY_BYTES: usize = 1024 * 1024;
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
/// splitting a code point) to at most [`MAX_LOG_LINE_BYTES`], and emits it
/// via `EngineOpState::log_writer` (real stderr in production; an
/// injectable [`LogWriter`] in tests) as one `serde_json`-escaped JSON
/// object per line, tagged with this invocation's
/// `handler_id`/`invocation_id`. The emitted line is built entirely in
/// Rust from already-validated/truncated fields, so a handler cannot forge
/// or inject an extra line into it.
///
/// Once [`MAX_LOG_LINES`] lines have been recorded for this invocation,
/// **every** further call -- both the in-memory `InvocationReport` copy
/// and the emitted line -- is dropped, not just the in-memory one:
/// without that, a handler logging in an unbounded loop could still push
/// unbounded bytes through `log_writer` even though `InvocationReport`
/// itself stayed capped (review round 6, N2 -- an earlier version emitted
/// every line regardless of the cap). Dropped calls are counted in
/// `EngineOpState::dropped_log_lines`, reported once, at the end of the
/// invocation, by `emit_dropped_log_record_if_needed` -- logging must
/// never be able to fail a handler that is otherwise working correctly,
/// so a dropped line is silent to the handler itself, not thrown.
#[op2(fast)]
pub fn op_apsides_log(state: &mut OpState, #[string] level: &str, #[string] line: &str) {
    let engine_state = state.borrow_mut::<EngineOpState>();
    if engine_state.closed {
        return;
    }
    if engine_state.logs.len() >= MAX_LOG_LINES {
        engine_state.dropped_log_lines += 1;
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
        engine_state.log_writer.write_line(&record);
    }

    engine_state.logs.push(LogLine {
        level,
        message: truncated,
    });
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
    FetchConcurrencyExceeded,
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
    #[error(
        "this invocation already has Limits::max_concurrent_fetches apsides.fetch calls in \
         flight"
    )]
    FetchConcurrencyExceeded,
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
            Self::FetchConcurrencyExceeded => FetchErrorCode::FetchConcurrencyExceeded,
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

/// Structurally validates `method`/`headers` the same way `hyper` would
/// when actually building the request (`Method::from_bytes`/
/// `HeaderName`/`HeaderValue::from_bytes`), but *before* any DNS
/// resolution or dial -- review round 6's non-blocking item: an invalid
/// method/header must never open a connection at all, not fail only after
/// one is already established. `net::https_request` still validates
/// these itself too (defense in depth; it must never trust an unvalidated
/// caller), so this is deliberately redundant with, not a replacement
/// for, the check inside it.
fn validate_method_and_headers(
    method: &str,
    headers: &[(String, String)],
) -> Result<(), FetchError> {
    hyper::Method::from_bytes(method.as_bytes())
        .map_err(|_err| FetchError::Https(net::HttpsError::InvalidMethod(method.to_owned())))?;
    for (name, value) in headers {
        hyper::header::HeaderName::from_bytes(name.as_bytes())
            .map_err(|_err| FetchError::Https(net::HttpsError::InvalidHeader(name.clone())))?;
        hyper::header::HeaderValue::from_bytes(value.as_bytes())
            .map_err(|_err| FetchError::Https(net::HttpsError::InvalidHeader(value.clone())))?;
    }
    Ok(())
}

/// Releases this call's `fetches_in_flight` slot when dropped -- covering
/// every exit path after [`fetch`] reserves one (an early `?` return, a
/// dropped future if the isolate is terminated mid-call, or falling off
/// the end normally), not just the success path.
///
/// Also refunds the *entire* byte-budget reservation this call took,
/// unless [`Self::mark_success`] was called first. A failed fetch never
/// hands a response buffer to JS, so nothing is gained by keeping its
/// reservation charged, and every call that never gets far enough to know
/// its own outcome ends this way -- `mark_success` is only ever called
/// once a response has actually been received and its true size is known
/// (review round 6, R1; a prior version left a failed call's reservation
/// charged permanently, on the theory that this crate cannot always tell
/// how many bytes were actually read before a given failure -- true, but
/// irrelevant, since a failed fetch never gets those bytes into the
/// process's live memory via a returned buffer either way, and charging
/// for them let a burst of e.g. `HOST_NOT_ALLOWED` or connect-failure
/// calls exhaust an invocation's entire budget without ever touching the
/// network). On success, [`fetch`] itself refunds the unused part of the
/// worst-case allowance (the actual body was smaller) before marking the
/// guard successful; the actual body length then stays charged for the
/// rest of the invocation, same as before.
struct InFlightGuard {
    state: Rc<RefCell<OpState>>,
    /// The total (`body_len + response_reservation`) this call reserved.
    reservation: usize,
    success: bool,
}

impl InFlightGuard {
    const fn mark_success(&mut self) {
        self.success = true;
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        let mut state_ref = self.state.borrow_mut();
        let engine_state = state_ref.borrow_mut::<EngineOpState>();
        engine_state.fetches_in_flight = engine_state.fetches_in_flight.saturating_sub(1);
        if !self.success {
            engine_state.fetch_bytes_reserved = engine_state
                .fetch_bytes_reserved
                .saturating_sub(self.reservation);
        }
    }
}

/// Implements `op_apsides_fetch`'s full request.
///
/// Validates the request (body size, forbidden/malformed headers, URL
/// scheme, IP-literal host, the egress allowlist) *before* reserving any
/// concurrency slot or byte budget, so a call that was always going to be
/// rejected for a reason unrelated to available budget never consumes
/// budget it does not need (review round 6, R1: a prior version reserved
/// first, so a burst of e.g. `HOST_NOT_ALLOWED` calls could exhaust an
/// invocation's entire `max_fetch_bytes_total` without ever touching the
/// network).
///
/// Only once every such check has passed does it reserve this call's
/// concurrency slot and byte budget -- still **synchronously, before the
/// first `.await`** (review round 6, N1): without that, `N` concurrent
/// `apsides.fetch` calls could each read the same "budget/slots still
/// available" snapshot before any of them updates it, and all proceed --
/// together using `N` times the intended cap. The byte reservation is
/// this call's request body length plus a *worst-case* response
/// allowance (`MAX_FETCH_RESPONSE_BYTES`, clamped to whatever of the
/// budget remains after the body) -- not the eventual actual response
/// size, which is not known yet. If that would leave a zero-byte response
/// allowance, the call is rejected as budget-exhausted right there,
/// before dialling, rather than proceeding to a dial that can only ever
/// fail with a confusing `RESPONSE_TOO_LARGE` on the first byte received.
///
/// On success, the unused part of the worst-case allowance
/// (`response_reservation - actual body length`) is refunded and
/// [`InFlightGuard::mark_success`] is called. On any failure, the guard's
/// `Drop` refunds the *entire* reservation -- see its doc comment.
#[allow(
    clippy::future_not_send,
    reason = "deno_core::OpState/JsRuntime are deliberately !Send (single isolate, single thread); see crate::invoke_handler_with_watchdog's doc comment"
)]
#[allow(
    clippy::too_many_lines,
    reason = "validate -> parse/vet the URL -> reserve concurrency/budget -> resolve -> dial+request -> refund is one inherently sequential, hard-to-split pipeline; see the doc comment above for why each step exists"
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
    validate_method_and_headers(&request.method, &request.headers)?;

    let body_len = body.len();

    // Read-only snapshot: `closed`, plus the policy/deadline this call
    // needs for URL/host validation. Deliberately reserves nothing yet.
    // The resolver itself is handled separately below (lazily built, then
    // cloned out) -- see `EgressResolver`'s doc comment on why it cannot be
    // built once and reused across invocations.
    let (egress, deadline) = {
        let mut state_ref = state.borrow_mut();
        let engine_state = state_ref.borrow_mut::<EngineOpState>();
        if engine_state.closed {
            return Err(FetchError::OpsClosed);
        }
        (engine_state.egress.clone(), engine_state.deadline)
    };

    // Lazily build this invocation's own `TokioResolver`, on this
    // invocation's own current tokio runtime, the first time any call
    // within it actually needs real DNS resolution -- never once up front,
    // never shared with any other invocation. Cloning a `TokioResolver` out
    // (it wraps `Arc`-shared internals; see `hickory_resolver::Resolver`'s
    // own `#[derive(Clone)]`) lets this scope end, and the `RefCell`
    // borrow with it, before the `.await` below -- holding a `RefCell`
    // borrow across an `.await` point risks a panic if another op call
    // interleaves on the same single-threaded isolate.
    let resolver = {
        let mut state_ref = state.borrow_mut();
        let engine_state = state_ref.borrow_mut::<EngineOpState>();
        if engine_state.closed {
            return Err(FetchError::OpsClosed);
        }
        if engine_state.built_resolver.is_none()
            && let Some(handle) = engine_state.resolver.clone()
        {
            let built = handle.build().map_err(FetchError::DnsResolutionFailed)?;
            engine_state.built_resolver = Some(built);
        }
        engine_state.built_resolver.clone()
    };

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

    // Reservation: still synchronous, still before this function's first
    // `.await` (DNS resolution, next) -- but only now, after every check
    // above that can fail for a reason unrelated to available budget has
    // already passed (review round 6, R1).
    let (total_reservation, response_reservation) = {
        let mut state_ref = state.borrow_mut();
        let engine_state = state_ref.borrow_mut::<EngineOpState>();
        if engine_state.fetches_in_flight >= engine_state.max_concurrent_fetches {
            return Err(FetchError::FetchConcurrencyExceeded);
        }
        let remaining_budget = engine_state
            .max_fetch_bytes_total
            .saturating_sub(engine_state.fetch_bytes_reserved);
        if body_len >= remaining_budget {
            return Err(FetchError::FetchByteBudgetExceeded);
        }
        let response_reservation = MAX_FETCH_RESPONSE_BYTES.min(remaining_budget - body_len);
        if response_reservation == 0 {
            return Err(FetchError::FetchByteBudgetExceeded);
        }
        let total_reservation = body_len + response_reservation;

        engine_state.fetches_in_flight += 1;
        engine_state.fetch_bytes_reserved = engine_state
            .fetch_bytes_reserved
            .saturating_add(total_reservation);

        (total_reservation, response_reservation)
    };
    // From here on, every exit path (an early `?` return below, or a
    // dropped future) must release the concurrency slot and refund the
    // reservation just taken -- `in_flight_guard`'s `Drop` does both,
    // unless `mark_success` is called first (only once a real response
    // has been received; see below).
    let mut in_flight_guard = InFlightGuard {
        state: Rc::clone(state),
        reservation: total_reservation,
        success: false,
    };

    let candidates = net::resolve_host(
        &host,
        egress.resolver_override.as_ref(),
        resolver.as_ref(),
        deadline,
    )
    .await?;

    let mut path_and_query = parsed.path().to_owned();
    if let Some(query) = parsed.query() {
        path_and_query.push('?');
        path_and_query.push_str(query);
    }

    let extra_trust_anchor =
        egress
            .fixture_trust_anchor
            .as_ref()
            .map(|fixture| net::FixtureTrustAnchor {
                der: fixture.trust_anchor_der.clone(),
            });
    let fetch_limits = net::FetchLimits {
        max_response_bytes: response_reservation,
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

    let unused_reservation = response_reservation.saturating_sub(response.body.len());
    if unused_reservation > 0 {
        let mut state_ref = state.borrow_mut();
        let engine_state = state_ref.borrow_mut::<EngineOpState>();
        engine_state.fetch_bytes_reserved = engine_state
            .fetch_bytes_reserved
            .saturating_sub(unused_reservation);
    }
    // A real response was received and its true size is now reflected in
    // `fetch_bytes_reserved` above -- from here the guard's `Drop` must
    // not also refund the (now-correct) charge.
    in_flight_guard.mark_success();

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
/// emitted line (see `EngineOpState`/`StderrLogRecord`). Bundled into one
/// type so the `extension!` macro below (which becomes an `init` function
/// clippy counts arguments on) has fewer than 8 options.
pub(crate) struct InvocationIds {
    pub handler_id: String,
    pub invocation_id: String,
}

/// `apsides.fetch`'s deadline and byte/concurrency budgets, bundled for
/// the same reason as [`InvocationIds`].
pub(crate) struct FetchBudget {
    pub deadline: Instant,
    pub max_fetch_bytes_total: usize,
    pub max_concurrent_fetches: usize,
}

deno_core::extension!(
    apsides_ops,
    ops = [op_apsides_fetch, op_apsides_output_write, op_apsides_log],
    options = {
        egress: EgressPolicy,
        outputs: HashMap<String, usize>,
        sink: Arc<dyn OutputSink>,
        resolver: Option<Arc<EgressResolver>>,
        fetch_budget: FetchBudget,
        ids: InvocationIds,
        log_writer: Arc<dyn LogWriter>,
    },
    state = |state, options| {
        state.put(EngineOpState {
            egress: options.egress,
            outputs: options.outputs,
            outputs_written: HashMap::new(),
            sink: options.sink,
            resolver: options.resolver,
            built_resolver: None,
            deadline: options.fetch_budget.deadline,
            max_fetch_bytes_total: options.fetch_budget.max_fetch_bytes_total,
            fetch_bytes_reserved: 0,
            fetches_in_flight: 0,
            max_concurrent_fetches: options.fetch_budget.max_concurrent_fetches,
            handler_id: options.ids.handler_id,
            invocation_id: options.ids.invocation_id,
            logs: Vec::new(),
            dropped_log_lines: 0,
            log_writer: options.log_writer,
            closed: false,
        });
    },
);
