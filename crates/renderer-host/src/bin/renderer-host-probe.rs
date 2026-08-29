//! Child-process probe entrypoint. A parent can supervise this binary for
//! crash-only and non-cooperative renderer experiments.

use renderer_host::{Invocation, Limits, RendererHost};

const RENDER_INPUT: &str = r#"{
  "abiVersion":"renderer.oxideloop.dev/v0",
  "root":{
    "apiVersion":"probe.oxideloop.dev/v0",
    "kind":"Probe",
    "namespace":"default",
    "name":"probe",
    "uid":"probe-uid",
    "generation":1,
    "spec":{}
  },
  "bundleDigest":"sha256:probe",
  "cacheEpoch":"0",
  "observationToken":"probe-token",
  "observations":[]
}"#;

fn main() {
    let kind = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "recoverable".to_owned());
    if kind == "fatal" {
        // Intentional crash probe: this proves why the caller must supervise
        // truly fatal paths at a process boundary.
        std::process::abort();
    }
    let source = match kind.as_str() {
        "infinite" => "() => { while (true) {} }",
        "recoverable" => "() => { throw new Error('probe recoverable') }",
        _ => {
            "() => ({ abiVersion: 'renderer.oxideloop.dev/v0', disposition: 'explicitEmpty', resources: [], dependencies: [], automations: [], diagnostics: [] })"
        }
    };
    match RendererHost::new(Limits::default()).render(source, RENDER_INPUT) {
        Invocation::Completed(output) => println!("completed:{}", output.sha256),
        Invocation::Recoverable(error) => println!("recoverable:{error}"),
        Invocation::TimedOut => println!("timed_out"),
        Invocation::ProcessFatal(category) => println!("process_fatal:{category}"),
    }
}
