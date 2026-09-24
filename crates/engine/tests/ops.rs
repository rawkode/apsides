//! Phase 5b tests for `op_apsides_fetch`, `op_apsides_output_write`, and
//! `op_apsides_log` (see the Apsides v0.3 plan, "Phase 5 -> 5b", and
//! review round 5's B1-B8), driven entirely through the public
//! `apsides.*` JS surface via `invoke_handler_blocking`, matching how a
//! real handler would observe these ops.
//!
//! Every `#[test]` here returns `Result<(), String>` and propagates
//! failures with `?`/explicit `Err(...)` returns, matching this crate's
//! established convention (see `tests/landlock_fs.rs`) of never using
//! `unwrap`/`expect`/`panic!`/`unreachable!` -- all denied by the
//! workspace's clippy lints, including in test code.

mod common;

use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::AtomicUsize;
use std::sync::{Arc, Mutex};

use common::https_fixture::{FIXTURE_HOST, HttpsFixtureServer};
use common::{handler_invocation, wrap_as_handler};
use engine::{
    EgressPolicy, FIXTURE_ACKNOWLEDGEMENT, FixtureEgressOverride, HandlerInvocation,
    InvocationReport, OutputDeclaration, OutputSink, OutputSinkError, invoke_handler_blocking,
};

fn no_usage() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

/// Wraps `body` (which must `return` a JSON-serializable value) as the
/// exported handler and runs it with `invocation` otherwise left at
/// `handler_invocation`'s defaults, except for whatever `configure` (e.g.
/// `egress`/`outputs`/`sink`) sets.
fn run(body: &str, configure: impl FnOnce(&mut HandlerInvocation<'_>)) -> InvocationReport {
    let bundle = wrap_as_handler(body);
    let mut invocation = handler_invocation(&bundle);
    configure(&mut invocation);
    invoke_handler_blocking(invocation, &no_usage())
}

/// A handler body that attempts `js_call` (an `apsides.*` call expression,
/// already terminated with `;`), returning `"NO_ERROR_THROWN"` if it did
/// not throw, or the thrown error's `.code` otherwise.
fn catches_code(js_call: &str) -> String {
    format!(
        r#"
        try {{
            {js_call}
            return "NO_ERROR_THROWN";
        }} catch (e) {{
            return e.code;
        }}
    "#
    )
}

fn assert_code(report: &InvocationReport, expected_code: &str) -> Result<(), String> {
    match &report.result {
        Ok(value) => {
            let expected = serde_json::Value::String(expected_code.to_owned());
            if *value == expected {
                Ok(())
            } else {
                Err(format!(
                    "expected the handler to return code {expected_code:?}, got {value:?}"
                ))
            }
        }
        Err(err) => Err(format!(
            "expected the handler to return a code (not fail the invocation itself), got Err: {err}"
        )),
    }
}

fn assert_ok_value(report: InvocationReport, expected: &serde_json::Value) -> Result<(), String> {
    let value = report
        .result
        .map_err(|err| format!("expected the handler to succeed, got Err: {err}"))?;
    if value != *expected {
        return Err(format!("expected {expected:?}, got {value:?}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// op_apsides_fetch: denied, no network required
// ---------------------------------------------------------------------

#[test]
fn fetch_denied_scheme_not_https() -> Result<(), String> {
    let report = run(
        &catches_code(r#"await apsides.fetch("http://example.invalid/", {});"#),
        |_invocation| {},
    );
    assert_code(&report, "SCHEME_NOT_HTTPS")
}

#[test]
fn fetch_denied_host_is_ip_literal() -> Result<(), String> {
    let report = run(
        &catches_code(r#"await apsides.fetch("https://93.184.216.34/", {});"#),
        |_invocation| {},
    );
    assert_code(&report, "HOST_IS_IP_LITERAL")
}

/// review round 5 decision: the IP-literal check matches on `url::Host`,
/// not a re-parse of the host string, so a *bracketed* IPv6 literal is
/// rejected the same way a bare IPv4 one is.
#[test]
fn fetch_denied_host_is_bracketed_ipv6_literal() -> Result<(), String> {
    let report = run(
        &catches_code(r#"await apsides.fetch("https://[::1]/", {});"#),
        |_invocation| {},
    );
    assert_code(&report, "HOST_IS_IP_LITERAL")
}

#[test]
fn fetch_denied_host_not_listed() -> Result<(), String> {
    // Default `EgressPolicy` (from `handler_invocation`) has an empty
    // allowlist, so any DNS-looking host is rejected before any DNS
    // lookup is attempted.
    let report = run(
        &catches_code(r#"await apsides.fetch("https://not-listed.fixture.invalid/", {});"#),
        |_invocation| {},
    );
    assert_code(&report, "HOST_NOT_ALLOWED")
}

#[test]
fn fetch_denied_caller_supplied_host_header() -> Result<(), String> {
    let report = run(
        &catches_code(
            r#"await apsides.fetch("https://example.invalid/", { headers: { Host: "evil.invalid" } });"#,
        ),
        |_invocation| {},
    );
    assert_code(&report, "FORBIDDEN_REQUEST_HEADER")
}

/// review round 5, B2: every forbidden header (not just `Host`) is
/// rejected the same way.
#[test]
fn fetch_denied_forbidden_headers() -> Result<(), String> {
    for header in [
        "Content-Length",
        "Transfer-Encoding",
        "Connection",
        "TE",
        "Upgrade",
        "Trailer",
    ] {
        let body = catches_code(&format!(
            r#"await apsides.fetch("https://example.invalid/", {{ headers: {{ "{header}": "x" }} }});"#
        ));
        let report = run(&body, |_invocation| {});
        assert_code(&report, "FORBIDDEN_REQUEST_HEADER")
            .map_err(|err| format!("header {header:?}: {err}"))?;
    }
    Ok(())
}

#[test]
fn fetch_denied_private_address_from_dns() -> Result<(), String> {
    let private_ip: IpAddr = "10.0.0.5"
        .parse()
        .map_err(|err| format!("bad literal: {err}"))?;
    let mut overrides = HashMap::new();
    overrides.insert("private.fixture.invalid".to_owned(), vec![private_ip]);
    let egress = EgressPolicy {
        allowed: vec![("private.fixture.invalid".to_owned(), 443)],
        resolver_override: Some(overrides),
        // Deliberately no fixture_trust_anchor: that would also waive the
        // global-unicast check this test exists to exercise.
        fixture_trust_anchor: None,
    };

    let report = run(
        &catches_code(r#"await apsides.fetch("https://private.fixture.invalid/", {});"#),
        |invocation| invocation.egress = egress,
    );
    assert_code(&report, "NO_GLOBAL_UNICAST_ADDRESS")
}

/// review round 5, B4: every resolved candidate is tried in order, not
/// just the first; a vetted-but-unreachable first address must not block
/// a working second one.
#[test]
fn fetch_allowed_tries_the_second_candidate_after_the_first_fails() -> Result<(), String> {
    let server = HttpsFixtureServer::start(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
    )?;
    // 127.0.0.1 with nothing listening on the fixture server's port: the
    // first candidate must fail to connect and be skipped, not abort the
    // whole fetch.
    let unreachable_first: IpAddr = "127.0.0.1"
        .parse()
        .map_err(|err| format!("bad literal: {err}"))?;
    let mut overrides = HashMap::new();
    overrides.insert(FIXTURE_HOST.to_owned(), vec![unreachable_first, server.addr.ip()]);
    let egress = EgressPolicy {
        allowed: vec![(FIXTURE_HOST.to_owned(), server.addr.port())],
        resolver_override: Some(overrides),
        fixture_trust_anchor: Some(FixtureEgressOverride {
            acknowledgement: FIXTURE_ACKNOWLEDGEMENT.to_owned(),
            trust_anchor_der: server.ca_der.clone(),
        }),
    };
    let url = fixture_url(&server);

    let body = format!(r#"const r = await apsides.fetch("{url}", {{}}); return r.status;"#);
    let report = run(&body, |invocation| invocation.egress = egress);
    assert_ok_value(report, &serde_json::json!(200))?;

    server.join()
}

// ---------------------------------------------------------------------
// op_apsides_fetch: allowed/denied against the local fixture server
// ---------------------------------------------------------------------

fn fixture_egress(server: &HttpsFixtureServer) -> EgressPolicy {
    let mut overrides = HashMap::new();
    overrides.insert(FIXTURE_HOST.to_owned(), vec![server.addr.ip()]);
    EgressPolicy {
        allowed: vec![(FIXTURE_HOST.to_owned(), server.addr.port())],
        resolver_override: Some(overrides),
        fixture_trust_anchor: Some(FixtureEgressOverride {
            acknowledgement: FIXTURE_ACKNOWLEDGEMENT.to_owned(),
            trust_anchor_der: server.ca_der.clone(),
        }),
    }
}

fn fixture_url(server: &HttpsFixtureServer) -> String {
    format!("https://{FIXTURE_HOST}:{}/", server.addr.port())
}

#[test]
fn fetch_allowed_against_local_https_fixture_server() -> Result<(), String> {
    let server = HttpsFixtureServer::start(
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\nok",
    )?;
    let url = fixture_url(&server);

    let body = format!(
        r#"
        const response = await apsides.fetch("{url}", {{}});
        return {{ status: response.status, bodyLength: response.body.length }};
    "#
    );
    let report = run(&body, |invocation| invocation.egress = fixture_egress(&server));
    assert_ok_value(report, &serde_json::json!({ "status": 200, "bodyLength": 2 }))?;

    server.join()
}

/// review round 5 decision: a redirect response is returned to the
/// handler like any other response (never auto-followed); a handler that
/// wants to follow it calls `apsides.fetch` again, which is allowlist
/// checked like any other call.
#[test]
fn fetch_redirect_is_returned_to_the_handler_as_a_normal_response() -> Result<(), String> {
    let server = HttpsFixtureServer::start(
        b"HTTP/1.1 302 Found\r\nLocation: https://elsewhere.invalid/\r\nContent-Length: 0\r\n\r\n",
    )?;
    let url = fixture_url(&server);
    let body = format!(
        r#"
        const response = await apsides.fetch("{url}", {{}});
        const location = response.headers.find((h) => h[0].toLowerCase() === "location");
        return {{ status: response.status, location: location ? location[1] : null }};
    "#
    );

    let report = run(&body, |invocation| invocation.egress = fixture_egress(&server));
    assert_ok_value(
        report,
        &serde_json::json!({ "status": 302, "location": "https://elsewhere.invalid/" }),
    )?;

    server.join()
}

#[test]
fn fetch_denied_response_too_large() -> Result<(), String> {
    // One byte over the 4 MiB response cap.
    let oversized_body = vec![b'a'; 4 * 1024 * 1024 + 1];
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
        oversized_body.len()
    )
    .into_bytes();
    response.extend_from_slice(&oversized_body);
    let response: &'static [u8] = Box::leak(response.into_boxed_slice());

    let server = HttpsFixtureServer::start(response)?;
    let url = fixture_url(&server);
    let body = catches_code(&format!(r#"await apsides.fetch("{url}", {{}});"#));

    let report = run(&body, |invocation| invocation.egress = fixture_egress(&server));
    assert_code(&report, "RESPONSE_TOO_LARGE")?;

    server.join()
}

/// review round 5, B3: a response body just under the 4 MiB per-call cap
/// must succeed even with a heap far smaller than that (`ToJsBuffer`
/// builds its own backing store outside the capped allocator/heap; the
/// old JSON-number-array marshalling would have blown a 64 MiB heap on a
/// 4 MiB body).
#[test]
fn fetch_near_the_byte_cap_succeeds_with_a_small_heap() -> Result<(), String> {
    let body_len = 4 * 1024 * 1024 - 64;
    let big_body = vec![b'a'; body_len];
    let mut response = format!("HTTP/1.1 200 OK\r\nContent-Length: {body_len}\r\n\r\n").into_bytes();
    response.extend_from_slice(&big_body);
    let response: &'static [u8] = Box::leak(response.into_boxed_slice());

    let server = HttpsFixtureServer::start(response)?;
    let url = fixture_url(&server);
    let handler_body =
        format!(r#"const r = await apsides.fetch("{url}", {{}}); return r.body.length;"#);

    let report = run(&handler_body, |invocation| {
        invocation.egress = fixture_egress(&server);
        invocation.limits.max_heap_bytes = 64 * 1024 * 1024;
        invocation.limits.max_fetch_bytes_total = 8 * 1024 * 1024;
    });
    assert_ok_value(report, &serde_json::json!(body_len))?;

    server.join()
}

/// review round 5, B2: `hyper` decodes a chunked response body correctly
/// (the old hand-rolled client never implemented chunked decoding at all).
#[test]
fn fetch_chunked_response_body_decodes() -> Result<(), String> {
    let server = HttpsFixtureServer::start(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
    )?;
    let url = fixture_url(&server);
    let body = format!(
        r#"
        const r = await apsides.fetch("{url}", {{}});
        let text = "";
        for (const b of r.body) {{ text += String.fromCharCode(b); }}
        return {{ status: r.status, text }};
    "#
    );

    let report = run(&body, |invocation| invocation.egress = fixture_egress(&server));
    assert_ok_value(report, &serde_json::json!({ "status": 200, "text": "hello" }))?;

    server.join()
}

/// review round 5, B2: a response whose `Content-Length` promises more
/// bytes than the server actually sends before closing the connection is
/// a real error, not a silently-truncated success.
#[test]
fn fetch_short_content_length_is_an_error() -> Result<(), String> {
    let server = HttpsFixtureServer::start_without_close_notify(
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\nab",
    )?;
    let url = fixture_url(&server);
    let body = catches_code(&format!(r#"await apsides.fetch("{url}", {{}});"#));

    let report = run(&body, |invocation| invocation.egress = fixture_egress(&server));
    let outcome = report
        .result
        .map_err(|err| format!("expected the handler to return a code, got Err: {err}"))?;
    if outcome == serde_json::json!("NO_ERROR_THROWN") {
        return Err("expected a short Content-Length body to be rejected, but it succeeded".to_owned());
    }

    server.join()
}

/// review round 5, B2: a `103 Early Hints` informational response must
/// not be returned as the fetch's result; `hyper` skips it and returns the
/// final `200`.
#[test]
fn fetch_103_then_200_returns_200() -> Result<(), String> {
    let server = HttpsFixtureServer::start(
        b"HTTP/1.1 103 Early Hints\r\n\r\nHTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok",
    )?;
    let url = fixture_url(&server);
    let body = format!(r#"const r = await apsides.fetch("{url}", {{}}); return r.status;"#);

    let report = run(&body, |invocation| invocation.egress = fixture_egress(&server));
    assert_ok_value(report, &serde_json::json!(200))?;

    server.join()
}

/// review round 5, B2: a `Content-Length`-delimited response completes
/// successfully even without a clean TLS `close_notify` -- `hyper` stops
/// reading once it has read `Content-Length` bytes. The crate's old
/// hand-rolled client always read to EOF and could not have passed this.
#[test]
fn fetch_content_length_without_close_notify_succeeds() -> Result<(), String> {
    let server =
        HttpsFixtureServer::start_without_close_notify(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok")?;
    let url = fixture_url(&server);
    let body = format!(r#"const r = await apsides.fetch("{url}", {{}}); return r.status;"#);

    let report = run(&body, |invocation| invocation.egress = fixture_egress(&server));
    assert_ok_value(report, &serde_json::json!(200))?;

    server.join()
}

// ---------------------------------------------------------------------
// op_apsides_output_write
// ---------------------------------------------------------------------

fn one_declared_output(max_bytes: usize) -> Vec<OutputDeclaration> {
    vec![OutputDeclaration {
        id: "declared".to_owned(),
        max_bytes,
    }]
}

#[test]
fn output_write_denied_undeclared_id() -> Result<(), String> {
    let report = run(
        &catches_code(r#"apsides.writeOutput("undeclared", "key", "data");"#),
        |invocation| invocation.outputs = one_declared_output(1024),
    );
    assert_code(&report, "UNDECLARED_OUTPUT_ID")
}

#[test]
fn output_write_denied_invalid_key() -> Result<(), String> {
    let report = run(
        &catches_code(r#"apsides.writeOutput("declared", "bad key!", "data");"#),
        |invocation| invocation.outputs = one_declared_output(1024),
    );
    assert_code(&report, "INVALID_KEY")
}

/// review round 5, B7: keys over 253 characters are rejected.
#[test]
fn output_write_denied_key_too_long() -> Result<(), String> {
    let long_key = "a".repeat(254);
    let body = catches_code(&format!(r#"apsides.writeOutput("declared", "{long_key}", "data");"#));
    let report = run(&body, |invocation| invocation.outputs = one_declared_output(10_000));
    assert_code(&report, "INVALID_KEY")
}

/// review round 5, B7: a key of exactly `.` or `..` is rejected (even
/// though both match `[-._a-zA-Z0-9]+`).
#[test]
fn output_write_denied_key_is_dot_or_dotdot() -> Result<(), String> {
    for key in [".", ".."] {
        let body = catches_code(&format!(r#"apsides.writeOutput("declared", "{key}", "data");"#));
        let report = run(&body, |invocation| invocation.outputs = one_declared_output(1024));
        assert_code(&report, "INVALID_KEY").map_err(|err| format!("key {key:?}: {err}"))?;
    }
    Ok(())
}

#[test]
fn output_write_denied_data_too_large() -> Result<(), String> {
    let report = run(
        &catches_code(r#"apsides.writeOutput("declared", "key", "this-is-more-than-ten-bytes");"#),
        |invocation| invocation.outputs = one_declared_output(10),
    );
    assert_code(&report, "DATA_TOO_LARGE")
}

/// review round 5, B7: `max_bytes` bounds the *total* across every key
/// written to one output, not just one call's payload -- two individually
/// small writes that together exceed the cap are rejected, and updating
/// an existing key does not double-count its old contribution.
#[test]
fn output_write_denied_cumulative_total_across_keys_exceeds_cap() -> Result<(), String> {
    // cap 20; "a" + 8-byte data = 9 bytes; "b" + 8-byte data = 9 bytes;
    // together 18, still within cap. A third write for "c" (+9 more)
    // pushes the total to 27, over the 20 byte cap, and must be rejected.
    // Re-writing "a" (replacing, not adding, its own 9 bytes) must still
    // succeed afterward.
    let report = run(
        r#"
        apsides.writeOutput("declared", "a", "12345678");
        apsides.writeOutput("declared", "b", "12345678");
        let thirdThrew = false;
        let thirdCode = null;
        try {
            apsides.writeOutput("declared", "c", "12345678");
        } catch (e) {
            thirdThrew = true;
            thirdCode = e.code;
        }
        apsides.writeOutput("declared", "a", "87654321");
        return { thirdThrew, thirdCode };
    "#,
        |invocation| invocation.outputs = one_declared_output(20),
    );
    assert_ok_value(
        report,
        &serde_json::json!({ "thirdThrew": true, "thirdCode": "DATA_TOO_LARGE" }),
    )
}

struct RecordingOutputSink {
    writes: Mutex<Vec<(String, String, Vec<u8>)>>,
}

impl OutputSink for RecordingOutputSink {
    fn write_output(&self, id: &str, key: &str, data: &[u8]) -> Result<(), OutputSinkError> {
        let mut writes = self
            .writes
            .lock()
            .map_err(|_poisoned| OutputSinkError {
                message: "mutex poisoned".to_owned(),
            })?;
        writes.push((id.to_owned(), key.to_owned(), data.to_owned()));
        Ok(())
    }
}

#[test]
fn output_write_allowed_reaches_the_sink() -> Result<(), String> {
    let recording = Arc::new(RecordingOutputSink {
        writes: Mutex::new(Vec::new()),
    });
    let bundle = wrap_as_handler(r#"apsides.writeOutput("declared", "my-key", "hello");"#);
    let mut invocation = handler_invocation(&bundle);
    invocation.outputs = one_declared_output(1024);
    invocation.sink = Arc::clone(&recording) as Arc<dyn OutputSink>;

    invoke_handler_blocking(invocation, &no_usage())
        .result
        .map_err(|err| format!("expected the write to succeed, got Err: {err}"))?;

    let writes = recording
        .writes
        .lock()
        .map_err(|_poisoned| "mutex poisoned".to_owned())?;
    if writes.len() != 1 {
        return Err(format!("expected exactly one recorded write, got {}", writes.len()));
    }
    let (id, key, data) = &writes[0];
    if id != "declared" || key != "my-key" || data != b"hello" {
        return Err(format!("unexpected recorded write: {writes:?}"));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// op_apsides_log
// ---------------------------------------------------------------------

#[test]
fn log_truncates_a_line_over_the_4kib_cap() -> Result<(), String> {
    let report = run(
        r#"
        apsides.log("info", "x".repeat(5000));
        return "done";
    "#,
        |_invocation| {},
    );

    report
        .result
        .map_err(|err| format!("expected the handler to succeed, got Err: {err}"))?;
    if report.logs.len() != 1 {
        return Err(format!("expected exactly one log line, got {}", report.logs.len()));
    }
    if report.logs[0].message.len() > 4 * 1024 {
        return Err(format!(
            "expected the logged line to be truncated to at most 4 KiB, got {} bytes",
            report.logs[0].message.len()
        ));
    }
    Ok(())
}

#[test]
fn log_caps_an_invocation_at_64_lines() -> Result<(), String> {
    let report = run(
        r#"
        for (let i = 0; i < 100; i++) {
            apsides.log("info", "line " + i);
        }
        return "done";
    "#,
        |_invocation| {},
    );

    report
        .result
        .map_err(|err| format!("expected the handler to succeed, got Err: {err}"))?;
    if report.logs.len() != 64 {
        return Err(format!(
            "expected exactly 64 lines to have been recorded (the rest silently dropped), got {}",
            report.logs.len()
        ));
    }
    Ok(())
}

/// review round 5, B8: the level a handler passes is recorded.
#[test]
fn log_records_the_given_level() -> Result<(), String> {
    let report = run(
        r#"
        apsides.log("warn", "a warning");
        return "done";
    "#,
        |_invocation| {},
    );

    report
        .result
        .map_err(|err| format!("expected the handler to succeed, got Err: {err}"))?;
    if report.logs != vec![engine::LogLine {
        level: engine::LogLevel::Warn,
        message: "a warning".to_owned(),
    }] {
        return Err(format!("unexpected logs: {:?}", report.logs));
    }
    Ok(())
}

#[test]
fn log_rejects_an_invalid_level() -> Result<(), String> {
    let report = run(
        &catches_code(r#"apsides.log("bogus", "x");"#),
        |_invocation| {},
    );
    // Level validation happens in the frozen JS wrapper itself (a plain
    // TypeError, not an op error with a `.code`), so this only needs to
    // confirm it throws at all, not go through `assert_code`.
    let value = report
        .result
        .map_err(|err| format!("expected the handler to return a value, got Err: {err}"))?;
    if value == serde_json::json!("NO_ERROR_THROWN") {
        return Err("expected an invalid log level to throw".to_owned());
    }
    Ok(())
}

// ---------------------------------------------------------------------
// review round 5 decision: ops close before the return value is
// marshalled
// ---------------------------------------------------------------------

/// A handler that returns an object whose `output` property is a getter
/// running *after* the handler's own async function has already resolved
/// (getters run during `serde_v8::from_v8`'s property walk of the
/// returned value, which happens after `call_handler_export` awaits the
/// promise). `apsides.fetch`/`writeOutput` must already be closed by then.
#[test]
fn ops_are_closed_before_the_return_value_is_marshalled() -> Result<(), String> {
    let report = run(
        r#"
        return {
            get output() {
                try {
                    apsides.writeOutput("declared", "key", "data");
                    return "NO_ERROR_THROWN";
                } catch (e) {
                    return e.code;
                }
            },
        };
    "#,
        |invocation| invocation.outputs = one_declared_output(1024),
    );

    let value = report
        .result
        .map_err(|err| format!("expected the handler to succeed, got Err: {err}"))?;
    let output = value
        .get("output")
        .ok_or_else(|| format!("expected an `output` property, got {value:?}"))?;
    if *output != serde_json::json!("OPS_CLOSED") {
        return Err(format!(
            "expected the getter's apsides.writeOutput call to fail with OPS_CLOSED, got {output:?}"
        ));
    }
    Ok(())
}
