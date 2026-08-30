use std::{
    fs,
    io::Write,
    net::{Shutdown, SocketAddr, TcpListener, TcpStream},
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

use oxideloop_bundle_contract::{
    BUNDLE_MANIFEST_VERSION_V0, BundleIntegrityManifestV0, BundleStore, ExecutionPhaseV0,
    FileIntegrityV0, ModuleIntegrityV0, PhaseModuleV0, ToolIdentityV0, parse_validate_manifest,
};
use oxideloop_renderer_abi::sha256_hex;
use oxideloop_renderer_protocol::{
    ErrorCategoryV0, FrameV0, PhaseV0, RequestIdV0, RequestV0, ResponseStatusV0, ResponseV0,
    read_frame, write_frame,
};
use renderer_worker::{Supervisor, WorkerConfig, WorkerError, validate_loopback_bind};
use tempfile::TempDir;

const RENDER_SOURCE: &str = "input => ({ abiVersion: 'renderer.oxideloop.dev/v0', disposition: 'apply', resources: [{ logicalId: 'config', object: { apiVersion: 'v1', kind: 'ConfigMap', metadata: { name: input.root.name } }, readiness: null }], dependencies: [], automations: [], diagnostics: [] })";
const ACTION_SOURCE: &str = "input => ({ abiVersion: 'renderer.oxideloop.dev/v0', actions: [{ logicalId: 'smoke', kind: 'Job', specification: { occurrenceId: input.occurrenceId } }], diagnostics: [] })";

struct InstalledFixture {
    store: TempDir,
    bundle_digest: String,
    render_digest: String,
    action_digest: String,
}

fn artifact(path: &str, bytes: &[u8]) -> FileIntegrityV0 {
    FileIntegrityV0 {
        path: path.into(),
        length: bytes.len() as u64,
        sha256: sha256_hex(bytes),
    }
}

fn install_fixture(render_source: &str) -> InstalledFixture {
    let source = TempDir::new().unwrap();
    let store = TempDir::new().unwrap();
    let descriptor = br#"{"program":"example"}"#;
    let render = ModuleIntegrityV0 {
        identity: "render.main".into(),
        file: artifact("render.js", render_source.as_bytes()),
    };
    let action = ModuleIntegrityV0 {
        identity: "action.smoke".into(),
        file: artifact("action.js", ACTION_SOURCE.as_bytes()),
    };
    let manifest = BundleIntegrityManifestV0 {
        manifest_version: BUNDLE_MANIFEST_VERSION_V0.into(),
        compiler: ToolIdentityV0 {
            identity: "tsx".into(),
            version: "1".into(),
        },
        bundler: ToolIdentityV0 {
            identity: "rolldown".into(),
            version: "1".into(),
        },
        renderer_abi_version: "renderer.oxideloop.dev/v0".into(),
        descriptor: artifact("descriptor.json", descriptor),
        render_modules: vec![render.clone()],
        action_modules: vec![action.clone()],
        source_maps: vec![],
        phase_modules: vec![
            PhaseModuleV0 {
                phase: ExecutionPhaseV0::Render,
                module_identity: render.identity.clone(),
            },
            PhaseModuleV0 {
                phase: ExecutionPhaseV0::Action,
                module_identity: action.identity.clone(),
            },
        ],
    };
    fs::write(source.path().join("descriptor.json"), descriptor).unwrap();
    fs::write(source.path().join("render.js"), render_source).unwrap();
    fs::write(source.path().join("action.js"), ACTION_SOURCE).unwrap();
    let validated = parse_validate_manifest(&serde_json::to_string(&manifest).unwrap()).unwrap();
    let bundle_store = BundleStore::new(store.path()).unwrap();
    bundle_store
        .install_directory(source.path(), &validated)
        .unwrap();
    InstalledFixture {
        store,
        bundle_digest: validated.bundle_digest().into(),
        render_digest: render.file.sha256,
        action_digest: action.file.sha256,
    }
}

fn evaluator_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_renderer-worker"))
}

fn start(fixture: &InstalledFixture) -> (SocketAddr, thread::JoinHandle<Result<(), WorkerError>>) {
    let supervisor = Supervisor::bind(WorkerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        fixture.store.path().into(),
        evaluator_binary().into(),
    ))
    .unwrap();
    let address = supervisor.local_addr().unwrap();
    let handle = thread::spawn(move || supervisor.serve_one());
    (address, handle)
}

fn request(
    fixture: &InstalledFixture,
    phase: PhaseV0,
    module_digest: Option<String>,
    payload: String,
    duration_millis: u16,
) -> RequestV0 {
    RequestV0 {
        request_id: RequestIdV0 { bytes: [42; 16] },
        phase,
        bundle_digest: fixture.bundle_digest.clone(),
        module_digest,
        duration_millis,
        payload: payload.into_bytes(),
    }
}

fn render_input(bundle_digest: &str) -> String {
    format!(
        r#"{{"abiVersion":"renderer.oxideloop.dev/v0","root":{{"apiVersion":"example.dev/v1","kind":"WebApplication","namespace":"default","name":"demo","uid":"root-1","generation":1,"spec":{{}}}},"bundleDigest":"{bundle_digest}","cacheEpoch":"1","observationToken":"snapshot-1","observations":[]}}"#
    )
}

fn action_input(bundle_digest: &str, action_digest: &str) -> String {
    format!(
        r#"{{"abiVersion":"renderer.oxideloop.dev/v0","bundleDigest":"{bundle_digest}","automationIdentity":"after-ready","automationVersion":"1","actionModuleIdentity":"action.smoke","actionModuleDigest":"{action_digest}","occurrenceId":"occurrence-1","sourceTransitionId":"transition-1","sourceUid":"root-1","sourceResourceVersion":"7","eventMetadata":{{}}}}"#
    )
}

fn round_trip(address: SocketAddr, request: &RequestV0) -> Result<ResponseV0, String> {
    let mut stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))
        .map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    write_frame(&mut stream, &FrameV0::Request(request.clone()))
        .map_err(|error| error.to_string())?;
    stream
        .shutdown(Shutdown::Write)
        .map_err(|error| error.to_string())?;
    match read_frame(&mut stream).map_err(|error| error.to_string())? {
        FrameV0::Response(response) => Ok(response),
        FrameV0::Request(_) => Err("unexpected request response".into()),
    }
}

#[test]
fn descriptor_render_and_action_complete_through_fresh_evaluators() {
    let fixture = install_fixture(RENDER_SOURCE);

    let (address, worker) = start(&fixture);
    let descriptor = request(&fixture, PhaseV0::Descriptor, None, String::new(), 1000);
    let response = round_trip(address, &descriptor).unwrap();
    assert_eq!(response.status, ResponseStatusV0::Ok);
    assert_eq!(response.payload, br#"{"program":"example"}"#);
    assert!(worker.join().unwrap().is_ok());

    let (address, worker) = start(&fixture);
    let render = request(
        &fixture,
        PhaseV0::Render,
        Some(fixture.render_digest.clone()),
        render_input(&fixture.bundle_digest),
        1000,
    );
    let response = round_trip(address, &render).unwrap();
    assert_eq!(response.status, ResponseStatusV0::Ok);
    assert!(
        String::from_utf8(response.payload)
            .unwrap()
            .contains("ConfigMap")
    );
    assert!(worker.join().unwrap().is_ok());

    let (address, worker) = start(&fixture);
    let action = request(
        &fixture,
        PhaseV0::Action,
        Some(fixture.action_digest.clone()),
        action_input(&fixture.bundle_digest, &fixture.action_digest),
        1000,
    );
    let response = round_trip(address, &action).unwrap();
    assert_eq!(response.status, ResponseStatusV0::Ok);
    assert!(
        String::from_utf8(response.payload)
            .unwrap()
            .contains("occurrence-1")
    );
    assert!(worker.join().unwrap().is_ok());
}

#[test]
fn wrong_bundle_module_and_phase_are_opaque_recoverable_outcomes() {
    let fixture = install_fixture(RENDER_SOURCE);
    for request in [
        RequestV0 {
            bundle_digest: "0".repeat(64),
            ..request(
                &fixture,
                PhaseV0::Render,
                Some(fixture.render_digest.clone()),
                render_input(&fixture.bundle_digest),
                1000,
            )
        },
        request(
            &fixture,
            PhaseV0::Render,
            Some("1".repeat(64)),
            render_input(&fixture.bundle_digest),
            1000,
        ),
        request(
            &fixture,
            PhaseV0::Action,
            Some(fixture.render_digest.clone()),
            action_input(&fixture.bundle_digest, &fixture.render_digest),
            1000,
        ),
    ] {
        let (address, worker) = start(&fixture);
        let response = round_trip(address, &request).unwrap();
        assert_eq!(response.status, ResponseStatusV0::Recoverable);
        assert_eq!(
            response.error_category,
            Some(ErrorCategoryV0::BundleUnavailable)
        );
        assert!(response.payload.is_empty());
        assert!(worker.join().unwrap().is_ok());
    }
}

#[test]
fn typed_payload_cannot_retarget_the_verified_bundle_or_action_module() {
    let fixture = install_fixture(RENDER_SOURCE);
    let (address, worker) = start(&fixture);
    let request = request(
        &fixture,
        PhaseV0::Action,
        Some(fixture.action_digest.clone()),
        action_input(&fixture.bundle_digest, &"1".repeat(64)),
        1000,
    );
    let response = round_trip(address, &request).unwrap();
    assert_eq!(response.status, ResponseStatusV0::Recoverable);
    assert_eq!(response.error_category, Some(ErrorCategoryV0::InvalidInput));
    assert!(worker.join().unwrap().is_ok());
}

#[test]
fn tampered_installed_bytes_never_reach_an_isolate() {
    let fixture = install_fixture(RENDER_SOURCE);
    let bundle = fixture.store.path().join(&fixture.bundle_digest);
    make_writable(&bundle);
    let module = bundle.join("render.js");
    make_writable(&module);
    fs::write(&module, "input => { while (true) {} }").unwrap();

    let (address, worker) = start(&fixture);
    let request = request(
        &fixture,
        PhaseV0::Render,
        Some(fixture.render_digest.clone()),
        render_input(&fixture.bundle_digest),
        1000,
    );
    let response = round_trip(address, &request).unwrap();
    assert_eq!(response.status, ResponseStatusV0::Recoverable);
    assert_eq!(
        response.error_category,
        Some(ErrorCategoryV0::BundleUnavailable)
    );
    assert!(worker.join().unwrap().is_ok());
}

#[test]
fn malformed_abi_phase_trailing_and_truncated_frames_terminate_the_supervisor() {
    let fixture = install_fixture(RENDER_SOURCE);
    let valid = request(
        &fixture,
        PhaseV0::Render,
        Some(fixture.render_digest.clone()),
        render_input(&fixture.bundle_digest),
        1000,
    );
    let mut valid_bytes = vec![];
    write_frame(&mut valid_bytes, &FrameV0::Request(valid)).unwrap();
    let inputs = [
        valid_bytes[..12].to_vec(),
        {
            let mut bytes = valid_bytes.clone();
            bytes[7] = 1;
            bytes
        },
        {
            let mut bytes = valid_bytes.clone();
            bytes[9] = 99;
            bytes
        },
        {
            let mut bytes = valid_bytes;
            bytes.push(0);
            bytes
        },
    ];
    for bytes in inputs {
        let (address, worker) = start(&fixture);
        let mut stream = TcpStream::connect(address).unwrap();
        stream.write_all(&bytes).unwrap();
        stream.shutdown(Shutdown::Write).unwrap();
        assert!(worker.join().unwrap().is_err());
    }
}

#[test]
fn slow_drip_request_cannot_extend_the_absolute_worker_admission_deadline() {
    let fixture = install_fixture(RENDER_SOURCE);
    let (address, worker) = start(&fixture);
    let request = request(
        &fixture,
        PhaseV0::Render,
        Some(fixture.render_digest.clone()),
        render_input(&fixture.bundle_digest),
        1000,
    );
    let mut bytes = vec![];
    write_frame(&mut bytes, &FrameV0::Request(request)).unwrap();
    let mut stream = TcpStream::connect(address).unwrap();
    let started = Instant::now();
    for byte in bytes {
        if stream.write_all(&[byte]).is_err() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    let _ = stream.shutdown(Shutdown::Write);
    assert!(worker.join().unwrap().is_err());
    assert!(started.elapsed() < Duration::from_millis(1300));
}

#[test]
fn evaluator_timeout_terminates_the_supervisor_and_a_new_one_can_succeed() {
    let stuck = install_fixture("input => { while (true) {} }");
    let (address, worker) = start(&stuck);
    let stuck_request = request(
        &stuck,
        PhaseV0::Render,
        Some(stuck.render_digest.clone()),
        render_input(&stuck.bundle_digest),
        25,
    );
    assert!(round_trip(address, &stuck_request).is_err());
    assert!(matches!(
        worker.join().unwrap(),
        Err(WorkerError::EvaluatorFailure)
    ));

    let healthy = install_fixture(RENDER_SOURCE);
    let (address, worker) = start(&healthy);
    let healthy_request = request(
        &healthy,
        PhaseV0::Render,
        Some(healthy.render_digest.clone()),
        render_input(&healthy.bundle_digest),
        1000,
    );
    assert_eq!(
        round_trip(address, &healthy_request).unwrap().status,
        ResponseStatusV0::Ok
    );
    assert!(worker.join().unwrap().is_ok());
}

#[cfg(unix)]
#[test]
fn a_child_that_never_reads_its_pipe_is_killed_and_reaped() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = install_fixture(RENDER_SOURCE);
    let script = fixture.store.path().join("wedged-evaluator.sh");
    let pid_file = fixture.store.path().join("wedged-evaluator.pid");
    fs::write(
        &script,
        format!(
            "#!/bin/sh\necho $$ > '{}'\nwhile :; do :; done\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&script, permissions).unwrap();
    let supervisor = Supervisor::bind(WorkerConfig::new(
        "127.0.0.1:0".parse().unwrap(),
        fixture.store.path().into(),
        script,
    ))
    .unwrap();
    let address = supervisor.local_addr().unwrap();
    let worker = thread::spawn(move || supervisor.serve_one());
    let request = request(
        &fixture,
        PhaseV0::Render,
        Some(fixture.render_digest.clone()),
        render_input(&fixture.bundle_digest),
        500,
    );
    assert!(round_trip(address, &request).is_err());
    assert!(matches!(
        worker.join().unwrap(),
        Err(WorkerError::EvaluatorFailure)
    ));
    let pid = fs::read_to_string(&pid_file).unwrap();
    assert!(
        !std::process::Command::new("/bin/kill")
            .args(["-0", pid.trim()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .unwrap()
            .success()
    );
}

#[cfg(unix)]
#[test]
fn supervisor_exits_on_a_descendant_that_retains_stdout_after_the_child_exits() {
    use std::os::unix::fs::PermissionsExt;

    let fixture = install_fixture(RENDER_SOURCE);
    let script = fixture.store.path().join("leaky-evaluator.sh");
    let pid_file = fixture.store.path().join("leaky-descendant.pid");
    // `sleep` inherits the evaluator stdout pipe. The direct shell child exits
    // straight away, leaving the reader blocked without EOF unless the
    // supervisor detects that containment cannot be proven and exits itself.
    fs::write(
        &script,
        format!(
            "#!/bin/sh\nsleep 5 &\necho $! > '{}'\nexit 0\n",
            pid_file.display()
        ),
    )
    .unwrap();
    let mut permissions = fs::metadata(&script).unwrap().permissions();
    permissions.set_mode(0o700);
    fs::set_permissions(&script, permissions).unwrap();

    let reservation = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let address_text = address.to_string();
    let store_root = fixture.store.path().display().to_string();
    let evaluator_path = script.display().to_string();
    let mut worker = Command::new(evaluator_binary())
        .args([
            "--listen",
            &address_text,
            "--store-root",
            &store_root,
            "--evaluator-binary",
            &evaluator_path,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();

    let deadline = Instant::now() + Duration::from_secs(2);
    let mut stream = loop {
        match TcpStream::connect(address) {
            Ok(stream) => break stream,
            Err(_) if Instant::now() < deadline => thread::sleep(Duration::from_millis(5)),
            Err(error) => panic!("worker did not accept loopback connection: {error}"),
        }
    };
    let descriptor = request(&fixture, PhaseV0::Descriptor, None, String::new(), 500);
    write_frame(&mut stream, &FrameV0::Request(descriptor)).unwrap();
    stream.shutdown(Shutdown::Write).unwrap();

    let started = Instant::now();
    let exit_status = loop {
        match worker.try_wait().unwrap() {
            Some(status) => break status,
            None if started.elapsed() < Duration::from_millis(1500) => {
                thread::sleep(Duration::from_millis(5));
            }
            None => panic!("worker did not exit after evaluator containment became uncertain"),
        }
    };
    assert_eq!(exit_status.code(), Some(70));
    assert!(started.elapsed() < Duration::from_millis(1500));

    let pid = fs::read_to_string(&pid_file).unwrap();
    let still_running = Command::new("/bin/kill")
        .args(["-0", pid.trim()])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap()
        .success();
    // The test owns this intentionally escaped descendant; remove it even
    // when the assertion below fails, instead of leaving it until `sleep`
    // reaches its timeout.
    let _ = Command::new("/bin/kill").arg(pid.trim()).status();
    assert!(
        still_running,
        "test descendant did not retain evaluator stdout"
    );
}

#[test]
fn only_explicit_ipv4_loopback_is_a_valid_bind() {
    assert!(validate_loopback_bind("127.0.0.1:0".parse().unwrap()).is_ok());
    for address in ["0.0.0.0:0", "127.0.0.2:0", "[::1]:0"] {
        assert!(matches!(
            validate_loopback_bind(address.parse().unwrap()),
            Err(WorkerError::NonLoopbackBind)
        ));
    }
}

fn make_writable(path: &Path) {
    let mut permissions = fs::metadata(path).unwrap().permissions();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        permissions.set_mode(0o700);
    }
    #[cfg(not(unix))]
    permissions.set_readonly(false);
    fs::set_permissions(path, permissions).unwrap();
}
