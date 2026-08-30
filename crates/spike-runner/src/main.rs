use std::{
    env, fs,
    path::{Path, PathBuf},
    process::ExitCode,
};

use oxideloop_mutation_model::{ProbeCaseV0, plan};
use oxideloop_renderer_abi::model::{Dependency, RenderDisposition};
use renderer_host::{HostError, Invocation, Limits, RendererHost};
use serde::Serialize;

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("spike-runner: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), String> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("mutation-probes") => {
            let directory = args.next().ok_or_else(usage)?;
            mutation_probes(Path::new(&directory))
        }
        Some("evaluate") => {
            let args = args.collect::<Vec<_>>();
            evaluate(&args)
        }
        Some("decision-report") => {
            let args = args.collect::<Vec<_>>();
            decision_report(&args)
        }
        Some("react-lifecycle-probe") => {
            let args = args.collect::<Vec<_>>();
            react_lifecycle_probe(&args)
        }
        Some("dependency-probe") => {
            let args = args.collect::<Vec<_>>();
            dependency_probe(&args)
        }
        Some("render-safety-probe") => {
            let args = args.collect::<Vec<_>>();
            render_safety_probe(&args)
        }
        _ => Err(usage()),
    }
}

fn mutation_probes(directory: &Path) -> Result<(), String> {
    let mut files = fs::read_dir(directory)
        .map_err(|error| format!("failed to read {}: {error}", directory.display()))?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| format!("failed to enumerate {}: {error}", directory.display()))?;
    files.retain(|path| {
        path.extension()
            .is_some_and(|extension| extension == "json")
    });
    files.sort();
    if files.is_empty() {
        return Err(format!("no JSON probes found in {}", directory.display()));
    }

    for path in &files {
        let bytes = fs::read(path)
            .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
        let case: ProbeCaseV0 = serde_json::from_slice(&bytes)
            .map_err(|error| format!("invalid probe {}: {error}", path.display()))?;
        let actual = plan(&case.input)
            .map_err(|error| format!("probe {} could not be planned: {error}", case.name))?;
        if actual != case.expected {
            return Err(format!(
                "probe {:?} disagreed:\nexpected: {}\nactual:   {}",
                case.name,
                serde_json::to_string(&case.expected).map_err(display)?,
                serde_json::to_string(&actual).map_err(display)?
            ));
        }
    }

    println!("{} model-only mutation probes passed", files.len());
    println!("no live Kubernetes API behavior was exercised");
    Ok(())
}

fn evaluate(args: &[String]) -> Result<(), String> {
    let phase = required_value(args, "--phase")?;
    let fixture = PathBuf::from(required_value(args, "--fixture")?);
    let input = PathBuf::from(required_value(args, "--input")?);
    let repeat = optional_value(args, "--repeat")
        .unwrap_or("1")
        .parse::<usize>()
        .map_err(|error| format!("invalid --repeat: {error}"))?;
    if repeat == 0 {
        return Err("--repeat must be positive".into());
    }

    let source = fs::read_to_string(&fixture)
        .map_err(|error| format!("failed to read {}: {error}", fixture.display()))?;
    let input = fs::read_to_string(&input)
        .map_err(|error| format!("failed to read {}: {error}", input.display()))?;
    let host = RendererHost::new(Limits::default());
    let mut expected_hash: Option<String> = None;

    for iteration in 0..repeat {
        let hash = match phase {
            "render" => match host.render(&source, &input) {
                Invocation::Completed(output) => output.sha256,
                invocation => {
                    return Err(format!(
                        "iteration {iteration} did not complete: {invocation:?}"
                    ));
                }
            },
            "action" => match host.action(&source, &input) {
                Invocation::Completed(output) => output.sha256,
                invocation => {
                    return Err(format!(
                        "iteration {iteration} did not complete: {invocation:?}"
                    ));
                }
            },
            _ => return Err("--phase must be render or action".into()),
        };
        if let Some(expected) = &expected_hash {
            if expected != &hash {
                return Err(format!(
                    "nondeterministic output at iteration {iteration}: {expected} != {hash}"
                ));
            }
        } else {
            expected_hash = Some(hash);
        }
    }

    println!(
        "{repeat} fresh-isolate {phase} evaluations produced canonical hash {}",
        expected_hash.expect("repeat is non-zero")
    );
    Ok(())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct DecisionReport {
    schema_version: &'static str,
    actual_react: Candidate,
    react_like: Candidate,
    in_process_deno_core: Candidate,
    capability_surface: Option<renderer_host::CapabilitySurface>,
    boundary: &'static str,
}

#[derive(Serialize)]
struct Candidate {
    status: &'static str,
    reason: &'static str,
}

fn decision_report(args: &[String]) -> Result<(), String> {
    let output = PathBuf::from(required_value(args, "--output")?);
    let host = RendererHost::new(Limits::default());
    let capability_surface = host.capability_surface().ok();
    let report = DecisionReport {
        schema_version: "m0-decision.oxideloop.dev/v0",
        actual_react: Candidate {
            status: "NoGo",
            reason: "the happy-path bundle is deterministic, but React useLayoutEffect executes during render commit and violates the pure renderer contract",
        },
        react_like: Candidate {
            status: "Go",
            reason: "the explicit React-like runtime produced one canonical hash across the accepted fresh-isolate locale and timezone matrix",
        },
        in_process_deno_core: Candidate {
            status: "NoGo",
            reason: "execution and watchdog probes pass, but heap, macOS memory, and process-fatal containment remain unenforced in process",
        },
        capability_surface,
        boundary: "local renderer/model evidence only; no Kubernetes API was contacted",
    };
    if let Some(parent) = output.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }
    let bytes = serde_json::to_vec_pretty(&report).map_err(display)?;
    fs::write(&output, bytes)
        .map_err(|error| format!("failed to write {}: {error}", output.display()))?;
    println!("wrote M0 decision report to {}", output.display());
    Ok(())
}

fn react_lifecycle_probe(args: &[String]) -> Result<(), String> {
    let fixture = PathBuf::from(required_value(args, "--fixture")?);
    let input = PathBuf::from(required_value(args, "--input")?);
    let source = fs::read_to_string(&fixture)
        .map_err(|error| format!("failed to read {}: {error}", fixture.display()))?;
    let input = fs::read_to_string(&input)
        .map_err(|error| format!("failed to read {}: {error}", input.display()))?;
    let Invocation::Completed(output) =
        RendererHost::new(Limits::default()).render(&source, &input)
    else {
        return Err(
            "actual React lifecycle fixture did not complete as typed render output".into(),
        );
    };
    let observed = output.document.diagnostics.iter().any(|diagnostic| {
        diagnostic.code == "actual-react-layout-effect-executed" && diagnostic.message == "count=1"
    });
    if !observed {
        return Err(
            "actual React lifecycle fixture did not report exactly one layout effect".into(),
        );
    }
    println!("actual React lifecycle probe observed one layout-effect callback");
    Ok(())
}

fn dependency_probe(args: &[String]) -> Result<(), String> {
    let fixture = PathBuf::from(required_value(args, "--fixture")?);
    let input = PathBuf::from(required_value(args, "--input")?);
    let source = fs::read_to_string(&fixture)
        .map_err(|error| format!("failed to read {}: {error}", fixture.display()))?;
    let input = fs::read_to_string(&input)
        .map_err(|error| format!("failed to read {}: {error}", input.display()))?;
    let Invocation::Completed(output) =
        RendererHost::new(Limits::default()).render(&source, &input)
    else {
        return Err("dependency fixture did not complete as typed render output".into());
    };
    let expected = vec![Dependency::Object {
        api_version: "v1".into(),
        kind: "Service".into(),
        namespace: "default".into(),
        name: "api-service".into(),
        uid: None,
        resource_version: None,
        observation_token: "test-observation-token".into(),
    }];
    if output.document.dependencies != expected {
        return Err("dependency fixture returned an unexpected graph".into());
    }
    println!("renderer output recorded one deduplicated absent Service dependency");
    Ok(())
}

fn render_safety_probe(args: &[String]) -> Result<(), String> {
    let explicit_fixture = PathBuf::from(required_value(args, "--explicit-fixture")?);
    let null_fixture = PathBuf::from(required_value(args, "--null-fixture")?);
    let input = PathBuf::from(required_value(args, "--input")?);
    let explicit_source = fs::read_to_string(&explicit_fixture)
        .map_err(|error| format!("failed to read {}: {error}", explicit_fixture.display()))?;
    let null_source = fs::read_to_string(&null_fixture)
        .map_err(|error| format!("failed to read {}: {error}", null_fixture.display()))?;
    let input = fs::read_to_string(&input)
        .map_err(|error| format!("failed to read {}: {error}", input.display()))?;
    let host = RendererHost::new(Limits::default());
    let Invocation::Completed(explicit) = host.render(&explicit_source, &input) else {
        return Err("explicit-empty fixture did not complete".into());
    };
    if explicit.document.disposition != RenderDisposition::ExplicitEmpty
        || !explicit.document.resources.is_empty()
    {
        return Err("explicit-empty fixture did not authorize exactly an empty graph".into());
    }
    if host.render(&null_source, &input) != Invocation::Recoverable(HostError::RuntimeFailure) {
        return Err(
            "null component did not fail closed before an empty graph was authorized".into(),
        );
    }
    println!("only explicitEmpty() authorized an empty graph; null failed closed");
    Ok(())
}

fn required_value<'a>(args: &'a [String], flag: &str) -> Result<&'a str, String> {
    optional_value(args, flag).ok_or_else(|| format!("missing {flag}\n{}", usage()))
}

fn optional_value<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|window| window[0] == flag)
        .map(|window| window[1].as_str())
}

fn display(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn usage() -> String {
    "usage:\n  spike-runner mutation-probes <fixture-directory>\n  spike-runner evaluate --phase <render|action> --fixture <callable.js> --input <input.json> [--repeat N]\n  spike-runner dependency-probe --fixture <callable.js> --input <input.json>\n  spike-runner react-lifecycle-probe --fixture <callable.js> --input <input.json>\n  spike-runner render-safety-probe --explicit-fixture <callable.js> --null-fixture <callable.js> --input <input.json>\n  spike-runner decision-report --output <report.json>".into()
}

#[cfg(test)]
mod tests {
    use super::optional_value;

    #[test]
    fn reads_flag_values() {
        let args = vec!["--phase".into(), "render".into()];
        assert_eq!(optional_value(&args, "--phase"), Some("render"));
        assert_eq!(optional_value(&args, "--missing"), None);
    }
}
