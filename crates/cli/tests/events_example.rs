//! `examples/events` compiles to exactly the event-plane surface its README
//! describes, and its compiled handler bundle behaves as documented when
//! the controller's own engine runs it.
//!
//! The handler tests run the bundle `aps compile` embeds in the generated
//! controller (`controller/src/handlers/sync-flags.js`, checked against the
//! IR's `bundleSha256`) through `engine::invoke_handler_with_watchdog`, the
//! entry point `runtime`'s `HandlerRuntime` uses, with the IR's own limits
//! and the event shape `runtime`'s `build_event` constructs. Compiling here,
//! rather than pasting a captured bundle into `runtime`'s tests, keeps the
//! tests on the example's current source with no copy to go stale; this
//! crate already builds `aps` and runs the Deno compiler for its other
//! tests, and `engine::test_support::HttpsFixtureServer` supplies the flag
//! service. One deviation is unavoidable: that server's certificate only
//! names `engine::test_support::FIXTURE_HOST` and it listens on an
//! ephemeral loopback port, so each test swaps the bundle's single
//! `"https://flags.example.com"` origin literal for the fixture's, and
//! allows exactly that host and port.
//!
//! The compiler runs under Deno with the repository's installed modules, as
//! every `aps compile` does (`cuenv task install`).
use compiler_core::{CompiledPlan, Handler};
use engine::test_support::{FIXTURE_HOST, HttpsFixtureServer};
use engine::{
    EgressPolicy, FIXTURE_ACKNOWLEDGEMENT, FixtureEgressOverride, HandlerInvocation,
    InvocationReport, Limits, LogLevel, OutputDeclaration, OutputSink, OutputSinkError,
    StderrLogWriter,
};
use serde_json::json;
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, HashMap},
    fs,
    net::{IpAddr, Ipv4Addr},
    path::{Path, PathBuf},
    process::Command,
    sync::{Arc, Mutex, atomic::AtomicUsize},
    time::Duration,
};

type Result<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
type Writes = BTreeMap<String, BTreeMap<String, String>>;

/// A successful flag service answer for one environment.
const FLAGS_OK: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 49\r\n\r\n{\"flags\":{\"new-checkout\":true,\"dark-mode\":false}}";
/// The flag service is up but refuses to answer.
const FLAGS_UNAVAILABLE: &[u8] = b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n";
/// A 2xx answer that is not the documented `{"flags": {...}}` object.
const FLAGS_MALFORMED: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 26\r\n\r\n{\"flags\":[\"new-checkout\"]}";

struct CompiledExample {
    _directory: tempfile::TempDir,
    output: PathBuf,
    plan: CompiledPlan,
}

fn compile_example() -> Result<CompiledExample> {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../examples/events/program.tsx")
        .canonicalize()?;
    let directory = tempfile::tempdir()?;
    let output = directory.path().join("compiled");
    let result = Command::new(env!("CARGO_BIN_EXE_aps"))
        .arg("compile")
        .arg("--source")
        .arg(source)
        .arg("--output")
        .arg(&output)
        .output()?;
    if !result.status.success() {
        return Err(format!(
            "aps compile failed: {}",
            String::from_utf8_lossy(&result.stderr)
        )
        .into());
    }
    let plan = compiler_core::parse_and_validate(&fs::read_to_string(output.join("ir.json"))?)?;
    Ok(CompiledExample {
        _directory: directory,
        output,
        plan,
    })
}

#[test]
fn events_example_surface_lists_its_handler_egress_output_and_rollout() -> Result {
    let example = compile_example()?;
    let surface: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(example.output.join("surface.json"))?)?;
    assert_eq!(
        surface["handlers"],
        json!([{
            "id": "sync-flags",
            "trigger": "feature-settings",
            "inputs": ["feature-settings"],
            "outputs": ["feature-flags"],
        }])
    );
    assert_eq!(
        surface["egress"],
        json!([{"host": "flags.example.com", "port": 443, "handler": "sync-flags"}])
    );
    assert_eq!(
        surface["outputs"],
        json!([{"id": "feature-flags", "name": "feature-flags", "namespace": "default"}])
    );
    assert_eq!(
        surface["reactions"],
        json!([{"input": "feature-flags", "target": "deployment-storefront"}])
    );
    Ok(())
}

/// Records every `apsides.writeOutput` the engine accepted, by output id
/// and key.
#[derive(Default)]
struct RecordingSink(Mutex<Writes>);

impl OutputSink for RecordingSink {
    fn write_output(
        &self,
        id: &str,
        key: &str,
        data: &[u8],
    ) -> std::result::Result<(), OutputSinkError> {
        let value = String::from_utf8(data.to_vec()).map_err(|error| OutputSinkError {
            message: error.to_string(),
        })?;
        self.0
            .lock()
            .map_err(|_poisoned| OutputSinkError {
                message: "recording sink lock poisoned".into(),
            })?
            .entry(id.to_owned())
            .or_default()
            .insert(key.to_owned(), value);
        Ok(())
    }
}

fn sync_flags(plan: &CompiledPlan) -> Result<&Handler> {
    Ok(plan
        .ir
        .handlers
        .iter()
        .find(|handler| handler.id == "sync-flags")
        .ok_or("the sync-flags handler is missing from the IR")?)
}

/// The embedded bundle, with its one declared origin literal pointed at the
/// fixture server instead (see the module doc comment).
fn fixture_bundle(example: &CompiledExample, handler: &Handler, port: u16) -> Result<String> {
    let bundle = fs::read_to_string(
        example
            .output
            .join("controller/src/handlers")
            .join(format!("{}.js", handler.id)),
    )?;
    assert_eq!(
        format!("{:x}", Sha256::digest(bundle.as_bytes())),
        handler.bundle_sha256,
        "the embedded bundle is the one the IR declares"
    );
    let [egress] = handler.egress.as_slice() else {
        return Err("sync-flags declares exactly one egress host".into());
    };
    if egress.port != 443 {
        return Err("the bundle's origin literal carries no port, so egress must be :443".into());
    }
    let declared = format!("\"https://{}\"", egress.host);
    if bundle.matches(&declared).count() != 1 {
        return Err(format!("the bundle must contain {declared} exactly once").into());
    }
    Ok(bundle.replacen(&declared, &format!("\"https://{FIXTURE_HOST}:{port}\""), 1))
}

/// Serves `response` once as the flag service and runs the example's
/// handler for a `feature-settings` input naming `environment`.
fn run_sync_flags(
    response: &'static [u8],
    environment: &str,
) -> Result<(InvocationReport, Writes)> {
    let example = compile_example()?;
    let handler = sync_flags(&example.plan)?;
    let server = HttpsFixtureServer::start(response)?;
    let port = server.addr.port();
    let bundle = fixture_bundle(&example, handler, port)?;
    let sink = Arc::new(RecordingSink::default());
    let sink_for_engine: Arc<dyn OutputSink> = sink.clone();
    // The shape `runtime`'s `build_event` gives every handler event.
    let event = json!({
        "id": "events-example",
        "trigger": handler.trigger,
        "inputs": {"feature-settings": {"data": {"environment": environment}}},
    });
    let report = engine::invoke_handler_with_watchdog(
        HandlerInvocation {
            bundle: &bundle,
            export: &handler.export,
            event,
            limits: Limits {
                timeout: Duration::from_millis(u64::from(handler.timeout_millis)),
                max_heap_bytes: usize::try_from(handler.max_heap_bytes)?,
                max_array_buffer_bytes: usize::try_from(compiler_core::array_buffer_cap_bytes(
                    handler.max_heap_bytes,
                ))?,
                max_fetch_bytes_total: usize::try_from(
                    compiler_core::HANDLER_FETCH_BUDGET_TOTAL_BYTES,
                )?,
                max_concurrent_fetches: usize::try_from(
                    compiler_core::HANDLER_MAX_CONCURRENT_FETCHES,
                )?,
            },
            egress: EgressPolicy {
                allowed: vec![(FIXTURE_HOST.to_owned(), port)],
                resolver_override: Some(HashMap::from([(
                    FIXTURE_HOST.to_owned(),
                    vec![IpAddr::V4(Ipv4Addr::LOCALHOST)],
                )])),
                fixture_trust_anchor: Some(FixtureEgressOverride {
                    acknowledgement: FIXTURE_ACKNOWLEDGEMENT.to_owned(),
                    trust_anchor_der: server.ca_der.clone(),
                }),
            },
            outputs: handler
                .outputs
                .iter()
                .map(|id| OutputDeclaration {
                    id: id.clone(),
                    max_bytes: example.plan.ir.domain.budgets.max_object_bytes,
                })
                .collect(),
            sink: sink_for_engine,
            resolver: None,
            handler_id: &handler.id,
            invocation_id: "events-example",
            log_writer: Arc::new(StderrLogWriter),
        },
        &Arc::new(AtomicUsize::new(0)),
    );
    server.join()?;
    let writes = sink
        .0
        .lock()
        .map_err(|_poisoned| "recording sink lock poisoned")?
        .clone();
    Ok((report, writes))
}

#[test]
fn events_example_handler_writes_one_key_per_flag_and_the_environment() -> Result {
    let (report, writes) = run_sync_flags(FLAGS_OK, "production")?;
    let value = report
        .result
        .map_err(|error| format!("the handler must succeed: {error}"))?;
    assert_eq!(value, json!({"environment": "production", "flags": 2}));
    assert_eq!(
        writes,
        BTreeMap::from([(
            "feature-flags".to_owned(),
            BTreeMap::from([
                ("FEATURE_DARK_MODE".to_owned(), "false".to_owned()),
                ("FEATURE_NEW_CHECKOUT".to_owned(), "true".to_owned()),
                ("FLAGS_ENVIRONMENT".to_owned(), "production".to_owned()),
            ]),
        )])
    );
    Ok(())
}

#[test]
fn events_example_handler_writes_nothing_when_the_flag_service_fails() -> Result {
    let (report, writes) = run_sync_flags(FLAGS_UNAVAILABLE, "production")?;
    let error = report
        .result
        .err()
        .ok_or("a 503 from the flag service must fail the invocation")?;
    assert!(
        error.to_string().contains("flag service answered HTTP 503"),
        "unexpected failure: {error}"
    );
    assert!(writes.is_empty(), "a failed invocation wrote {writes:?}");
    assert!(
        report.logs.iter().any(|line| line.level == LogLevel::Warn
            && line.message == "flag service answered HTTP 503 for production"),
        "missing the handler's warning: {:?}",
        report.logs
    );
    Ok(())
}

#[test]
fn events_example_handler_writes_nothing_for_a_malformed_flag_document() -> Result {
    let (report, writes) = run_sync_flags(FLAGS_MALFORMED, "production")?;
    let error = report
        .result
        .err()
        .ok_or("a malformed flag document must fail the invocation")?;
    assert!(
        error
            .to_string()
            .contains("flag service response has no flags object"),
        "unexpected failure: {error}"
    );
    assert!(writes.is_empty(), "a failed invocation wrote {writes:?}");
    Ok(())
}
