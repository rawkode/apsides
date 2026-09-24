//! Phase 5b tests for `op_apsides_fetch`, `op_apsides_output_write`, and
//! `op_apsides_log` (see the Apsides v0.3 plan, "Phase 5 -> 5b"), driven
//! entirely through the public `apsides.*` JS surface via
//! `invoke_handler_blocking`, matching how a real handler would observe
//! these ops.
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
    EgressPolicy, FIXTURE_ACKNOWLEDGEMENT, FixtureEgressOverride, HandlerInvocation, HandlerOutput,
    InvokeError, OutputDeclaration, OutputSink, OutputSinkError, invoke_handler_blocking,
};

fn no_usage() -> Arc<AtomicUsize> {
    Arc::new(AtomicUsize::new(0))
}

/// Wraps `body` (which must `return` a JSON-serializable value) as the
/// exported handler and runs it with `invocation` otherwise left at
/// `handler_invocation`'s defaults, except for whatever `configure` (e.g.
/// `egress`/`outputs`/`sink`) sets.
fn run(
    body: &str,
    configure: impl FnOnce(&mut HandlerInvocation<'_>),
) -> Result<HandlerOutput, InvokeError> {
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

fn assert_code(result: &Result<HandlerOutput, InvokeError>, expected_code: &str) -> Result<(), String> {
    match result {
        Ok(output) => {
            let expected = serde_json::Value::String(expected_code.to_owned());
            if output.value == expected {
                Ok(())
            } else {
                Err(format!(
                    "expected the handler to return code {expected_code:?}, got {:?}",
                    output.value
                ))
            }
        }
        Err(err) => Err(format!(
            "expected the handler to return a code (not fail the invocation itself), got Err: {err}"
        )),
    }
}

// ---------------------------------------------------------------------
// op_apsides_fetch: denied, no network required
// ---------------------------------------------------------------------

#[test]
fn fetch_denied_scheme_not_https() -> Result<(), String> {
    let result = run(
        &catches_code(r#"await apsides.fetch("http://example.invalid/", {});"#),
        |_invocation| {},
    );
    assert_code(&result, "SCHEME_NOT_HTTPS")
}

#[test]
fn fetch_denied_host_is_ip_literal() -> Result<(), String> {
    let result = run(
        &catches_code(r#"await apsides.fetch("https://93.184.216.34/", {});"#),
        |_invocation| {},
    );
    assert_code(&result, "HOST_IS_IP_LITERAL")
}

#[test]
fn fetch_denied_host_not_listed() -> Result<(), String> {
    // Default `EgressPolicy` (from `handler_invocation`) has an empty
    // allowlist, so any DNS-looking host is rejected before any DNS
    // lookup is attempted.
    let result = run(
        &catches_code(r#"await apsides.fetch("https://not-listed.fixture.invalid/", {});"#),
        |_invocation| {},
    );
    assert_code(&result, "HOST_NOT_ALLOWED")
}

#[test]
fn fetch_denied_caller_supplied_host_header() -> Result<(), String> {
    let result = run(
        &catches_code(
            r#"await apsides.fetch("https://example.invalid/", { headers: { Host: "evil.invalid" } });"#,
        ),
        |_invocation| {},
    );
    assert_code(&result, "HOST_HEADER_NOT_PERMITTED")
}

#[test]
fn fetch_denied_private_address_from_dns() -> Result<(), String> {
    let private_ip: IpAddr = "10.0.0.5"
        .parse()
        .map_err(|err| format!("bad literal: {err}"))?;
    let mut overrides = HashMap::new();
    overrides.insert("private.fixture.invalid".to_owned(), private_ip);
    let egress = EgressPolicy {
        allowed: vec![("private.fixture.invalid".to_owned(), 443)],
        resolver_override: Some(overrides),
        // Deliberately no fixture_trust_anchor: that would also waive the
        // global-unicast check this test exists to exercise.
        fixture_trust_anchor: None,
    };

    let result = run(
        &catches_code(r#"await apsides.fetch("https://private.fixture.invalid/", {});"#),
        |invocation| invocation.egress = egress,
    );
    assert_code(&result, "ADDRESS_NOT_GLOBAL_UNICAST")
}

// ---------------------------------------------------------------------
// op_apsides_fetch: allowed/denied against the local fixture server
// ---------------------------------------------------------------------

fn fixture_egress(server: &HttpsFixtureServer) -> EgressPolicy {
    let mut overrides = HashMap::new();
    overrides.insert(FIXTURE_HOST.to_owned(), server.addr.ip());
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
    let result = run(&body, |invocation| invocation.egress = fixture_egress(&server));

    let output = result.map_err(|err| format!("expected the fetch to succeed, got Err: {err}"))?;
    let expected = serde_json::json!({ "status": 200, "bodyLength": 2 });
    if output.value != expected {
        return Err(format!("expected {expected:?}, got {:?}", output.value));
    }

    server.join()
}

#[test]
fn fetch_denied_redirect_not_followed() -> Result<(), String> {
    let server = HttpsFixtureServer::start(
        b"HTTP/1.1 302 Found\r\nLocation: https://elsewhere.invalid/\r\nContent-Length: 0\r\n\r\n",
    )?;
    let url = fixture_url(&server);
    let body = catches_code(&format!(r#"await apsides.fetch("{url}", {{}});"#));

    let result = run(&body, |invocation| invocation.egress = fixture_egress(&server));
    assert_code(&result, "REDIRECT_NOT_FOLLOWED")?;

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

    let result = run(&body, |invocation| invocation.egress = fixture_egress(&server));
    assert_code(&result, "RESPONSE_TOO_LARGE")?;

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
    let result = run(
        &catches_code(r#"apsides.writeOutput("undeclared", "key", "data");"#),
        |invocation| invocation.outputs = one_declared_output(1024),
    );
    assert_code(&result, "UNDECLARED_OUTPUT_ID")
}

#[test]
fn output_write_denied_invalid_key() -> Result<(), String> {
    let result = run(
        &catches_code(r#"apsides.writeOutput("declared", "bad key!", "data");"#),
        |invocation| invocation.outputs = one_declared_output(1024),
    );
    assert_code(&result, "INVALID_KEY")
}

#[test]
fn output_write_denied_data_too_large() -> Result<(), String> {
    let result = run(
        &catches_code(r#"apsides.writeOutput("declared", "key", "this-is-more-than-ten-bytes");"#),
        |invocation| invocation.outputs = one_declared_output(10),
    );
    assert_code(&result, "DATA_TOO_LARGE")
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
    let result = run(
        r#"
        apsides.log("x".repeat(5000));
        return "done";
    "#,
        |_invocation| {},
    );

    let output = result.map_err(|err| format!("expected the handler to succeed, got Err: {err}"))?;
    if output.logs.len() != 1 {
        return Err(format!("expected exactly one log line, got {}", output.logs.len()));
    }
    if output.logs[0].len() > 4 * 1024 {
        return Err(format!(
            "expected the logged line to be truncated to at most 4 KiB, got {} bytes",
            output.logs[0].len()
        ));
    }
    Ok(())
}

#[test]
fn log_caps_an_invocation_at_64_lines() -> Result<(), String> {
    let result = run(
        r#"
        for (let i = 0; i < 100; i++) {
            apsides.log("line " + i);
        }
        return "done";
    "#,
        |_invocation| {},
    );

    let output = result.map_err(|err| format!("expected the handler to succeed, got Err: {err}"))?;
    if output.logs.len() != 64 {
        return Err(format!(
            "expected exactly 64 lines to have been recorded (the rest silently dropped), got {}",
            output.logs.len()
        ));
    }
    Ok(())
}
