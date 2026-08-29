//! A deliberately small V8 boundary for `OxideLoop` render programs.
//!
//! This crate is not a general-purpose JavaScript runtime.  Every invocation
//! creates a fresh isolate on a dedicated operating-system thread, supplies
//! JSON-only inputs, and returns JSON-only outputs. Kubernetes access is never
//! exposed to JavaScript.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    path::Path,
    process::Command,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use deno_core::{JsRuntime, RuntimeOptions};
pub use renderer_abi::{ActionInputV0, ActionOutputV0, RenderInputV0, RenderOutputV0};
use renderer_abi::{
    Validated, parse_validate_action_input, parse_validate_action_output,
    parse_validate_render_input, parse_validate_render_output,
};
use serde::{Deserialize, Serialize};

/// The two ABI phases have intentionally different argument shapes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Phase {
    Render,
    Action,
}

/// Limits enforced by the parent/supervisor, rather than trusted to guest JS.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Limits {
    pub max_source_bytes: usize,
    pub max_input_bytes: usize,
    pub max_output_bytes: usize,
    pub wall_clock: Duration,
    /// Grace period after `terminate_execution` before the host reports timeout.
    pub termination_grace: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_source_bytes: 256 * 1024,
            max_input_bytes: 2 * 1024 * 1024,
            max_output_bytes: 2 * 1024 * 1024,
            wall_clock: Duration::from_secs(1),
            termination_grace: Duration::from_millis(100),
        }
    }
}

/// Facts that are deliberately not claimed as proven by this in-process host.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ContainmentGap {
    HeapLimit,
    MacosMemoryEnforcement,
    ProcessFatalContainment,
}

/// The observed global surface after lockdown, useful for regression tests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilityDescriptor {
    pub kind: String,
    pub value_type: String,
    pub writable: Option<bool>,
    pub enumerable: bool,
    pub configurable: bool,
    pub getter_type: Option<String>,
    pub setter_type: Option<String>,
    pub prototype: Option<String>,
}

/// The observed global surface after lockdown, useful for regression tests.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CapabilitySurface {
    pub globals: BTreeSet<String>,
    pub unexpected_globals: BTreeSet<String>,
    pub missing_globals: BTreeSet<String>,
    pub descriptors: BTreeMap<String, CapabilityDescriptor>,
    pub descriptor_drift: BTreeSet<String>,
    pub unproven: BTreeSet<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum HostError {
    InputTooLarge { limit: usize, actual: usize },
    SourceTooLarge { limit: usize, actual: usize },
    OutputTooLarge { limit: usize, actual: usize },
    InputContract,
    OutputContract,
    InvalidSource,
    RuntimeFailure,
    StartupFailure,
}

impl fmt::Display for HostError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputTooLarge { limit, actual } => {
                write!(formatter, "input exceeds limit ({actual} > {limit})")
            }
            Self::SourceTooLarge { limit, actual } => {
                write!(formatter, "source exceeds limit ({actual} > {limit})")
            }
            Self::OutputTooLarge { limit, actual } => {
                write!(formatter, "output exceeds limit ({actual} > {limit})")
            }
            Self::InputContract => formatter.write_str("input does not satisfy the renderer ABI"),
            Self::OutputContract => formatter.write_str("output does not satisfy the renderer ABI"),
            Self::InvalidSource => {
                formatter.write_str("renderer source is not a callable expression")
            }
            Self::RuntimeFailure => formatter.write_str("renderer execution failed"),
            Self::StartupFailure => formatter.write_str("renderer isolate did not start"),
        }
    }
}

impl std::error::Error for HostError {}

/// Parent-visible result. Timeouts and fatal outcomes are not mistaken for a
/// normal, typed JavaScript exception.
#[derive(Clone, Debug, PartialEq)]
pub enum Invocation<T> {
    Completed(Validated<T>),
    Recoverable(HostError),
    TimedOut,
    ProcessFatal(FatalCategory),
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum RawInvocation {
    Completed(Vec<u8>),
    Recoverable(HostError),
    TimedOut,
    ProcessFatal(FatalCategory),
}

/// Closed, host-owned fatal categories. Guest-authored text never appears in
/// a fatal result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatalCategory {
    SupervisorDisconnectedBeforeStartup,
    RuntimePanicked,
    IsolateHandleTimeout,
    WatchdogUnresponsive,
    WorkerDisconnected,
}

impl fmt::Display for FatalCategory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::SupervisorDisconnectedBeforeStartup => "supervisor_disconnected_before_startup",
            Self::RuntimePanicked => "runtime_panicked",
            Self::IsolateHandleTimeout => "isolate_handle_timeout",
            Self::WatchdogUnresponsive => "watchdog_unresponsive",
            Self::WorkerDisconnected => "worker_disconnected",
        })
    }
}

/// Crash and hang checks run as a child process, because V8's in-process
/// termination API cannot contain a native process abort or memory pressure.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProbeKind {
    Recoverable,
    Infinite,
    Fatal,
}

impl ProbeKind {
    fn argument(self) -> &'static str {
        match self {
            Self::Recoverable => "recoverable",
            Self::Infinite => "infinite",
            Self::Fatal => "fatal",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SupervisedProbe {
    Exited { success: bool, stdout: String },
    SupervisorTimedOut,
}

/// Runs the package probe binary with an operating-system process boundary.
/// The caller chooses the binary path so production orchestration can launch
/// a separately constrained process (for example with cgroups on Linux).
///
/// # Errors
///
/// Returns an I/O error when the child cannot be started, observed, killed, or
/// collected by the supervisor.
pub fn supervise_probe(
    binary: &Path,
    kind: ProbeKind,
    wall_clock: Duration,
) -> std::io::Result<SupervisedProbe> {
    let mut child = Command::new(binary)
        .arg(kind.argument())
        .stdout(std::process::Stdio::piped())
        .spawn()?;
    let deadline = Instant::now() + wall_clock;
    loop {
        if let Some(status) = child.try_wait()? {
            let output = child.wait_with_output()?;
            return Ok(SupervisedProbe::Exited {
                success: status.success(),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            });
        }
        if Instant::now() >= deadline {
            child.kill()?;
            let _ = child.wait();
            return Ok(SupervisedProbe::SupervisorTimedOut);
        }
        thread::sleep(Duration::from_millis(5));
    }
}

#[derive(Clone, Debug)]
pub struct RendererHost {
    limits: Limits,
}

impl RendererHost {
    #[must_use]
    pub fn new(limits: Limits) -> Self {
        Self { limits }
    }

    #[must_use]
    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// Validates a render input and output with the versioned ABI.
    #[must_use]
    pub fn render(&self, source: &str, input_json: &str) -> Invocation<RenderOutputV0> {
        if input_json.len() > self.limits.max_input_bytes {
            return Invocation::Recoverable(HostError::InputTooLarge {
                limit: self.limits.max_input_bytes,
                actual: input_json.len(),
            });
        }
        let Ok(input) = parse_validate_render_input(input_json) else {
            return Invocation::Recoverable(HostError::InputContract);
        };
        Self::finish_render(self.invoke_raw(Phase::Render, source, &input.canonical_bytes))
    }

    /// Validates an action input and output with the versioned ABI. There is no
    /// independent, caller-controlled action JSON argument.
    #[must_use]
    pub fn action(&self, source: &str, input_json: &str) -> Invocation<ActionOutputV0> {
        if input_json.len() > self.limits.max_input_bytes {
            return Invocation::Recoverable(HostError::InputTooLarge {
                limit: self.limits.max_input_bytes,
                actual: input_json.len(),
            });
        }
        let Ok(input) = parse_validate_action_input(input_json) else {
            return Invocation::Recoverable(HostError::InputContract);
        };
        Self::finish_action(self.invoke_raw(Phase::Action, source, &input.canonical_bytes))
    }

    fn finish_render(invocation: RawInvocation) -> Invocation<RenderOutputV0> {
        match invocation {
            RawInvocation::Completed(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(|value| parse_validate_render_output(value).ok())
                .map_or(
                    Invocation::Recoverable(HostError::OutputContract),
                    Invocation::Completed,
                ),
            RawInvocation::Recoverable(error) => Invocation::Recoverable(error),
            RawInvocation::TimedOut => Invocation::TimedOut,
            RawInvocation::ProcessFatal(category) => Invocation::ProcessFatal(category),
        }
    }

    fn finish_action(invocation: RawInvocation) -> Invocation<ActionOutputV0> {
        match invocation {
            RawInvocation::Completed(bytes) => std::str::from_utf8(&bytes)
                .ok()
                .and_then(|value| parse_validate_action_output(value).ok())
                .map_or(
                    Invocation::Recoverable(HostError::OutputContract),
                    Invocation::Completed,
                ),
            RawInvocation::Recoverable(error) => Invocation::Recoverable(error),
            RawInvocation::TimedOut => Invocation::TimedOut,
            RawInvocation::ProcessFatal(category) => Invocation::ProcessFatal(category),
        }
    }

    /// Lists the actual V8 globals after the same lockdown used by programs.
    ///
    /// # Errors
    ///
    /// Returns [`HostError`] when a fresh isolate cannot start or its global
    /// inventory cannot be returned as valid JSON.
    pub fn capability_surface(&self) -> Result<CapabilitySurface, HostError> {
        let source = descriptor_probe_source();
        let RawInvocation::Completed(bytes) = self.invoke_raw(Phase::Render, source, b"null")
        else {
            return Err(HostError::StartupFailure);
        };
        let descriptors: BTreeMap<String, CapabilityDescriptor> =
            serde_json::from_slice(&bytes).map_err(|_| HostError::OutputContract)?;
        let globals = descriptors
            .keys()
            .filter_map(|path| path.strip_prefix("globalThis."))
            .filter(|name| !name.contains('.'))
            .map(str::to_owned)
            .collect::<BTreeSet<_>>();
        let allowed = allowed_globals();
        let unexpected_globals = globals.difference(&allowed).cloned().collect();
        let missing_globals = allowed.difference(&globals).cloned().collect();
        let expected_descriptors = expected_descriptors();
        let descriptor_drift = descriptors
            .keys()
            .chain(expected_descriptors.keys())
            .filter(|path| descriptors.get(*path) != expected_descriptors.get(*path))
            .cloned()
            .collect();
        Ok(CapabilitySurface {
            globals,
            unexpected_globals,
            missing_globals,
            descriptors,
            descriptor_drift,
            unproven: BTreeSet::from([
                "heap_limit".to_owned(),
                "macos_memory_enforcement".to_owned(),
                "process_fatal_containment".to_owned(),
            ]),
        })
    }

    #[cfg(test)]
    fn invoke_probe(&self, source: &str, input_json: &[u8]) -> RawInvocation {
        self.invoke_raw(Phase::Render, source, input_json)
    }

    fn invoke_raw(&self, phase: Phase, source: &str, input_json: &[u8]) -> RawInvocation {
        if source.len() > self.limits.max_source_bytes {
            return RawInvocation::Recoverable(HostError::SourceTooLarge {
                limit: self.limits.max_source_bytes,
                actual: source.len(),
            });
        }
        if input_json.len() > self.limits.max_input_bytes {
            return RawInvocation::Recoverable(HostError::InputTooLarge {
                limit: self.limits.max_input_bytes,
                actual: input_json.len(),
            });
        }

        let limits = self.limits.clone();
        let source = source.to_owned();
        let input_json = input_json.to_vec();
        let (started_tx, started_rx) = mpsc::sync_channel(1);
        let (done_tx, done_rx) = mpsc::sync_channel(1);

        let Ok(worker) = thread::Builder::new()
            .name("oxideloop-renderer".to_owned())
            .spawn(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let Ok(tokio_runtime) = tokio::runtime::Builder::new_current_thread()
                        .enable_time()
                        .build()
                    else {
                        return RawInvocation::Recoverable(HostError::StartupFailure);
                    };
                    let _tokio_guard = tokio_runtime.enter();
                    let mut runtime = JsRuntime::new(RuntimeOptions::default());
                    let handle = runtime.v8_isolate().thread_safe_handle();
                    if started_tx.send(handle).is_err() {
                        return RawInvocation::ProcessFatal(
                            FatalCategory::SupervisorDisconnectedBeforeStartup,
                        );
                    }
                    execute(&mut runtime, phase, &source, &input_json, &limits)
                }));
                let _ = done_tx.send(match outcome {
                    Ok(value) => value,
                    Err(_) => RawInvocation::ProcessFatal(FatalCategory::RuntimePanicked),
                });
            })
        else {
            return RawInvocation::Recoverable(HostError::StartupFailure);
        };

        let Ok(handle) = started_rx.recv_timeout(self.limits.wall_clock) else {
            return RawInvocation::ProcessFatal(FatalCategory::IsolateHandleTimeout);
        };
        match done_rx.recv_timeout(self.limits.wall_clock) {
            Ok(result) => {
                let _ = worker.join();
                result
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                handle.terminate_execution();
                match done_rx.recv_timeout(self.limits.termination_grace) {
                    Ok(_) | Err(mpsc::RecvTimeoutError::Disconnected) => {
                        let _ = worker.join();
                        RawInvocation::TimedOut
                    }
                    Err(mpsc::RecvTimeoutError::Timeout) => {
                        RawInvocation::ProcessFatal(FatalCategory::WatchdogUnresponsive)
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                RawInvocation::ProcessFatal(FatalCategory::WorkerDisconnected)
            }
        }
    }
}

fn execute(
    runtime: &mut JsRuntime,
    phase: Phase,
    source: &str,
    input_json: &[u8],
    limits: &Limits,
) -> RawInvocation {
    let Ok(input) = std::str::from_utf8(input_json) else {
        return RawInvocation::Recoverable(HostError::InputContract);
    };
    let Ok(input_literal) = serde_json::to_string(input) else {
        return RawInvocation::Recoverable(HostError::InputContract);
    };
    let invocation = "__program(__input)";
    {
        deno_core::scope!(scope, runtime);
        scope
            .get_current_context()
            .set_allow_generation_from_strings(false);
    }
    let script = format!(
        "\"use strict\";\nconst __oxideloop_parse = JSON.parse;\n{}\n{}\nconst __input = __oxideloop_deep_freeze(__oxideloop_parse({}));\n{}\nconst __program = ({});\nif (typeof __program !== 'function') throw new TypeError('program must be a function');\nconst __output = {};\nif (__output && typeof __output.then === 'function') throw new TypeError('async output is unsupported');\nconst __oxideloop_result = JSON.stringify(__output);",
        lockdown_script(),
        freeze_script(),
        input_literal,
        phase_capability_script(phase),
        source,
        invocation
    );
    if let Err(error) = runtime.execute_script("oxideloop:program", script) {
        let _ = error;
        return RawInvocation::Recoverable(HostError::RuntimeFailure);
    }
    let value = match runtime.execute_script("oxideloop:result", "__oxideloop_result") {
        Ok(value) => value,
        Err(error) => {
            let _ = error;
            return RawInvocation::Recoverable(HostError::RuntimeFailure);
        }
    };
    let output = {
        deno_core::scope!(scope, runtime);
        let value = value.open(scope);
        let Some(value) = value.to_string(scope) else {
            return RawInvocation::Recoverable(HostError::OutputContract);
        };
        let encoded_len = value.utf8_length(scope);
        if encoded_len > limits.max_output_bytes {
            return RawInvocation::Recoverable(HostError::OutputTooLarge {
                limit: limits.max_output_bytes,
                actual: encoded_len,
            });
        }
        value.to_rust_string_lossy(scope)
    };
    if serde_json::from_str::<serde_json::Value>(&output).is_err() {
        return RawInvocation::Recoverable(HostError::OutputContract);
    }
    if output.len() > limits.max_output_bytes {
        return RawInvocation::Recoverable(HostError::OutputTooLarge {
            limit: limits.max_output_bytes,
            actual: output.len(),
        });
    }
    RawInvocation::Completed(output.into_bytes())
}

fn allowed_globals() -> BTreeSet<String> {
    [
        "AggregateError",
        "Array",
        "ArrayBuffer",
        "BigInt",
        "BigInt64Array",
        "BigUint64Array",
        "Boolean",
        "DataView",
        "Error",
        "EvalError",
        "Float16Array",
        "Float32Array",
        "Float64Array",
        "Infinity",
        "Int16Array",
        "Int32Array",
        "Int8Array",
        "Iterator",
        "JSON",
        "Map",
        "Math",
        "NaN",
        "Number",
        "Object",
        "Proxy",
        "RangeError",
        "ReferenceError",
        "Reflect",
        "RegExp",
        "Set",
        "String",
        "Symbol",
        "SyntaxError",
        "TypeError",
        "URIError",
        "Uint16Array",
        "Uint32Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "WeakMap",
        "WeakSet",
        "decodeURI",
        "decodeURIComponent",
        "clearImmediate",
        "encodeURI",
        "encodeURIComponent",
        "globalThis",
        "isFinite",
        "isNaN",
        "parseFloat",
        "parseInt",
        "performance",
        "setImmediate",
        "undefined",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect()
}

fn descriptor_probe_source() -> &'static str {
    r#"() => {
  const prototypeIdentity = (value) => {
    if (value === null || (typeof value !== "object" && typeof value !== "function")) return null;
    const prototype = Object.getPrototypeOf(value);
    if (prototype === null) return "null";
    if (prototype === Object.getPrototypeOf(Object)) return "Function.prototype";
    if (prototype === Object.prototype) return "Object.prototype";
    if (prototype === Array.prototype) return "Array.prototype";
    return Object.prototype.toString.call(prototype);
  };
  const describe = (owner, key) => {
    const descriptor = Object.getOwnPropertyDescriptor(owner, key);
    const isData = Object.prototype.hasOwnProperty.call(descriptor, "value");
    const value = isData ? descriptor.value : undefined;
    return {
      kind: isData ? "data" : "accessor",
      valueType: isData ? typeof value : "accessor",
      writable: isData ? descriptor.writable : null,
      enumerable: descriptor.enumerable,
      configurable: descriptor.configurable,
      getterType: isData || descriptor.get === undefined ? null : typeof descriptor.get,
      setterType: isData || descriptor.set === undefined ? null : typeof descriptor.set,
      prototype: isData ? prototypeIdentity(value) : null
    };
  };
  const result = {};
  for (const name of Object.getOwnPropertyNames(globalThis).sort()) {
    result["globalThis." + name] = describe(globalThis, name);
  }
  const nested = [
    [Math, "random", "Math.random"],
    [performance, "now", "performance.now"],
    [String.prototype, "localeCompare", "String.prototype.localeCompare"],
    [String.prototype, "toLocaleLowerCase", "String.prototype.toLocaleLowerCase"],
    [String.prototype, "toLocaleUpperCase", "String.prototype.toLocaleUpperCase"],
    [Number.prototype, "toLocaleString", "Number.prototype.toLocaleString"],
    [BigInt.prototype, "toLocaleString", "BigInt.prototype.toLocaleString"],
    [Array.prototype, "toLocaleString", "Array.prototype.toLocaleString"]
  ];
  for (const [owner, key, path] of nested) result[path] = describe(owner, key);
  return result;
}"#
}

fn expected_descriptors() -> BTreeMap<String, CapabilityDescriptor> {
    let constructors = capability_constructors();
    let object_values = BTreeSet::from(["JSON", "Math", "Reflect", "globalThis"]);
    let inherited_constructors = inherited_capability_constructors();
    let immutable_scalars = BTreeSet::from(["Infinity", "NaN", "undefined"]);
    let locked_globals = BTreeSet::from(["clearImmediate", "performance", "setImmediate"]);
    let mut expected = BTreeMap::new();
    for name in allowed_globals() {
        let value_type = if constructors.contains(name.as_str())
            || matches!(name.as_str(), "clearImmediate" | "setImmediate")
        {
            "function"
        } else if object_values.contains(name.as_str()) || name == "performance" {
            "object"
        } else if name == "undefined" {
            "undefined"
        } else {
            "number"
        };
        let prototype = match value_type {
            "function" if inherited_constructors.contains(name.as_str()) => {
                Some("[object Function]")
            }
            "function" => Some("Function.prototype"),
            "object" if name == "globalThis" => Some("[object Object]"),
            "object" => Some("Object.prototype"),
            _ => None,
        };
        let locked =
            immutable_scalars.contains(name.as_str()) || locked_globals.contains(name.as_str());
        expected.insert(
            format!("globalThis.{name}"),
            data_descriptor(value_type, !locked, false, !locked, prototype),
        );
    }
    for path in nested_descriptor_paths() {
        expected.insert(
            (*path).to_owned(),
            data_descriptor("function", false, false, false, Some("Function.prototype")),
        );
    }
    expected
}

fn capability_constructors() -> BTreeSet<&'static str> {
    BTreeSet::from([
        "AggregateError",
        "Array",
        "ArrayBuffer",
        "BigInt",
        "BigInt64Array",
        "BigUint64Array",
        "Boolean",
        "DataView",
        "Error",
        "EvalError",
        "Float16Array",
        "Float32Array",
        "Float64Array",
        "Int16Array",
        "Int32Array",
        "Int8Array",
        "Iterator",
        "Map",
        "Number",
        "Object",
        "Proxy",
        "RangeError",
        "ReferenceError",
        "RegExp",
        "Set",
        "String",
        "Symbol",
        "SyntaxError",
        "TypeError",
        "URIError",
        "Uint16Array",
        "Uint32Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "WeakMap",
        "WeakSet",
        "decodeURI",
        "decodeURIComponent",
        "encodeURI",
        "encodeURIComponent",
        "isFinite",
        "isNaN",
        "parseFloat",
        "parseInt",
    ])
}

fn inherited_capability_constructors() -> BTreeSet<&'static str> {
    BTreeSet::from([
        "AggregateError",
        "BigInt64Array",
        "BigUint64Array",
        "EvalError",
        "Float16Array",
        "Float32Array",
        "Float64Array",
        "Int16Array",
        "Int32Array",
        "Int8Array",
        "RangeError",
        "ReferenceError",
        "SyntaxError",
        "TypeError",
        "URIError",
        "Uint16Array",
        "Uint32Array",
        "Uint8Array",
        "Uint8ClampedArray",
    ])
}

fn nested_descriptor_paths() -> &'static [&'static str] {
    &[
        "Math.random",
        "performance.now",
        "String.prototype.localeCompare",
        "String.prototype.toLocaleLowerCase",
        "String.prototype.toLocaleUpperCase",
        "Number.prototype.toLocaleString",
        "BigInt.prototype.toLocaleString",
        "Array.prototype.toLocaleString",
    ]
}

fn data_descriptor(
    value_type: &str,
    writable: bool,
    enumerable: bool,
    configurable: bool,
    prototype: Option<&str>,
) -> CapabilityDescriptor {
    CapabilityDescriptor {
        kind: "data".to_owned(),
        value_type: value_type.to_owned(),
        writable: Some(writable),
        enumerable,
        configurable,
        getter_type: None,
        setter_type: None,
        prototype: prototype.map(str::to_owned),
    }
}

fn lockdown_script() -> &'static str {
    r#"
const __oxideloop_forbidden = [
  "Atomics", "AsyncDisposableStack", "Bun", "Date", "Deno", "DisposableStack", "FinalizationRegistry",
  "Function", "Intl", "Promise", "SharedArrayBuffer", "SuppressedError", "Temporal", "WeakRef",
  "WebAssembly", "Worker", "XMLHttpRequest", "WebSocket", "__bootstrap", "console", "eval", "escape",
  "fetch", "localStorage", "location", "navigator", "process", "queueMicrotask", "require", "sessionStorage",
  "unescape"
];
for (const name of __oxideloop_forbidden) {
  try { delete globalThis[name]; } catch (_) { /* absent or non-configurable */ }
}
for (const name of __oxideloop_forbidden) {
  if (Object.prototype.hasOwnProperty.call(globalThis, name)) {
    throw new Error("forbidden global remains: " + name);
  }
}
const __oxideloop_performance = {};
Object.defineProperty(__oxideloop_performance, "now", {
  value: () => 0,
  writable: false,
  enumerable: false,
  configurable: false
});
Object.freeze(__oxideloop_performance);
Object.defineProperty(globalThis, "performance", {
  value: __oxideloop_performance,
  writable: false,
  configurable: false
});
Object.defineProperties(globalThis, {
  setImmediate: {
    value: (_callback) => 0,
    writable: false,
    configurable: false
  },
  clearImmediate: {
    value: (_handle) => {},
    writable: false,
    configurable: false
  }
});
Object.defineProperty(Math, "random", {
  value: () => { throw new Error("Math.random is unavailable in deterministic rendering"); },
  writable: false,
  configurable: false
});
const __oxideloop_locale_sensitive = [
  [String.prototype, "localeCompare"],
  [String.prototype, "toLocaleLowerCase"],
  [String.prototype, "toLocaleUpperCase"],
  [Number.prototype, "toLocaleString"],
  [BigInt.prototype, "toLocaleString"],
  [Array.prototype, "toLocaleString"]
];
for (const [prototype, name] of __oxideloop_locale_sensitive) {
  Object.defineProperty(prototype, name, {
    value: () => { throw new Error(name + " is unavailable in deterministic rendering"); },
    writable: false,
    configurable: false
  });
}
Object.preventExtensions(globalThis);
"#
}

fn freeze_script() -> &'static str {
    r#"
const __oxideloop_deep_freeze = (value) => {
  if (value === null || typeof value !== "object" || Object.isFrozen(value)) return value;
  for (const key of Reflect.ownKeys(value)) __oxideloop_deep_freeze(value[key]);
  return Object.freeze(value);
};
"#
}

fn phase_capability_script(phase: Phase) -> &'static str {
    match phase {
        Phase::Render => {
            r#"
const __oxideloop = Object.freeze({
  phase: "render",
  read: (name) => Array.isArray(__input.observations)
    ? __input.observations.find((observation) => observation.name === name)
    : undefined,
  list: () => Array.isArray(__input.observations) ? __input.observations.slice() : [],
  emitRender: (declaration) => declaration
});
"#
        }
        Phase::Action => {
            r#"
const __oxideloop = Object.freeze({
  phase: "action",
  emitAction: (declaration) => declaration
});
"#
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_render_input() -> String {
        serde_json::json!({
            "abiVersion": "renderer.oxideloop.dev/v0",
            "root": {
                "apiVersion": "apps.example.io/v1alpha1",
                "kind": "WebApplication",
                "namespace": "default",
                "name": "api",
                "uid": "root-uid",
                "generation": 1,
                "spec": {}
            },
            "bundleDigest": "sha256:test-bundle",
            "cacheEpoch": "0",
            "observationToken": "test-token",
            "observations": []
        })
        .to_string()
    }

    #[test]
    fn public_entrypoints_enforce_phase_specific_strict_abis() {
        let host = RendererHost::new(Limits::default());
        let render_input = valid_render_input();
        let render_source = "input => ({ abiVersion: 'renderer.oxideloop.dev/v0', disposition: 'apply', resources: [{ logicalId: 'root', object: { apiVersion: 'v1', kind: 'ConfigMap', metadata: { name: input.root.name } }, readiness: null }], dependencies: [], automations: [], diagnostics: [] })";
        assert!(matches!(
            host.render(render_source, &render_input),
            Invocation::Completed(_)
        ));

        let duplicate = format!(
            "{{\"abiVersion\":\"renderer.oxideloop.dev/v0\",{}",
            render_input.strip_prefix('{').unwrap()
        );
        assert_eq!(
            host.render(render_source, &duplicate),
            Invocation::Recoverable(HostError::InputContract)
        );

        let action_input = serde_json::json!({
            "abiVersion": "renderer.oxideloop.dev/v0",
            "automationIdentity": "on-ready",
            "occurrenceId": "occurrence-1",
            "sourceUid": "source-uid",
            "sourceResourceVersion": "7",
            "eventMetadata": {}
        })
        .to_string();
        assert_eq!(
            host.render(render_source, &action_input),
            Invocation::Recoverable(HostError::InputContract)
        );
        assert_eq!(
            host.render(
                "() => ({ abiVersion: 'renderer.oxideloop.dev/v0', actions: [], diagnostics: [] })",
                &render_input
            ),
            Invocation::Recoverable(HostError::OutputContract)
        );
        assert!(matches!(
            host.action(
                "input => ({ abiVersion: 'renderer.oxideloop.dev/v0', actions: [{ logicalId: 'job', kind: 'Job', specification: { occurrenceId: input.occurrenceId } }], diagnostics: [] })",
                &action_input
            ),
            Invocation::Completed(_)
        ));
    }

    #[test]
    fn capability_surface_has_no_unexpected_globals() {
        let surface = RendererHost::new(Limits::default())
            .capability_surface()
            .unwrap();
        assert!(surface.unexpected_globals.is_empty(), "{surface:?}");
        assert!(surface.missing_globals.is_empty(), "{surface:?}");
        assert!(surface.descriptor_drift.is_empty(), "{surface:#?}");
        assert!(surface.globals.contains("JSON"));
        assert!(surface.unproven.contains("heap_limit"));
    }

    #[test]
    fn global_allowlists_are_checked_at_runtime() {
        let host = RendererHost::new(Limits::default());
        assert_eq!(
            host.invoke_probe("() => typeof Deno", b"null"),
            RawInvocation::Completed(b"\"undefined\"".to_vec())
        );
        assert_eq!(
            host.invoke_probe("() => typeof fetch", b"null"),
            RawInvocation::Completed(b"\"undefined\"".to_vec())
        );
        assert_eq!(
            host.invoke_probe("() => typeof Date", b"null"),
            RawInvocation::Completed(b"\"undefined\"".to_vec())
        );
        assert_eq!(
            host.invoke_probe("() => typeof WebAssembly", b"null"),
            RawInvocation::Completed(b"\"undefined\"".to_vec())
        );
        assert_eq!(
            host.invoke_probe(
                "() => { let ran = false; setImmediate(() => { ran = true; }); return ran; }",
                b"null"
            ),
            RawInvocation::Completed(b"false".to_vec())
        );
        assert!(matches!(
            host.invoke_probe("() => (() => {}).constructor('return 1')()", b"null"),
            RawInvocation::Recoverable(HostError::RuntimeFailure)
        ));
        assert!(matches!(
            host.invoke_probe(
                "() => { globalThis.Deno = { core: {} }; return 'bad'; }",
                b"null"
            ),
            RawInvocation::Recoverable(HostError::RuntimeFailure)
        ));
    }

    #[test]
    fn action_phase_receives_no_implicit_host_capability() {
        let host = RendererHost::new(Limits::default());
        assert_eq!(
            host.invoke_probe(
                "input => ({ input, deno: typeof Deno })",
                br#"{"root":"x"}"#
            ),
            RawInvocation::Completed(br#"{"input":{"root":"x"},"deno":"undefined"}"#.to_vec())
        );
        assert_eq!(
            host.invoke_probe(
                "() => ({ phase: __oxideloop.phase, action: typeof __oxideloop.emitAction })",
                b"null"
            ),
            RawInvocation::Completed(br#"{"phase":"render","action":"undefined"}"#.to_vec())
        );
        assert_eq!(
            host.invoke_raw(
                Phase::Action,
                "() => ({ phase: __oxideloop.phase, read: typeof __oxideloop.read })",
                b"null"
            ),
            RawInvocation::Completed(br#"{"phase":"action","read":"undefined"}"#.to_vec())
        );
    }

    #[test]
    fn limits_and_watchdog_return_typed_outcomes() {
        let host = RendererHost::new(Limits {
            wall_clock: Duration::from_millis(250),
            termination_grace: Duration::from_millis(50),
            ..Limits::default()
        });
        assert_eq!(
            host.invoke_probe("() => { while (true) {} }", b"null"),
            RawInvocation::TimedOut
        );
        assert!(matches!(
            host.invoke_probe("() => undefined", b"null"),
            RawInvocation::Recoverable(HostError::OutputContract)
        ));
    }

    #[test]
    fn guest_errors_do_not_cross_boundary() {
        let host = RendererHost::new(Limits::default());
        let invocation = host.render(
            "() => { throw new Error('TOP_SECRET_DO_NOT_LEAK') }",
            &valid_render_input(),
        );
        assert_eq!(
            invocation,
            Invocation::Recoverable(HostError::RuntimeFailure)
        );
        assert!(!format!("{invocation:?}").contains("TOP_SECRET_DO_NOT_LEAK"));
    }

    #[test]
    fn input_json_is_transferred_as_data_not_javascript_source() {
        let host = RendererHost::new(Limits::default());
        let input = serde_json::json!({
            "__proto__": { "constructor": "source ) ; throw new Error('x')" },
            "constructor": "ordinary own property",
            "unicode": "\u{2028}\u{2029}",
            "escaped": "quote \" and backslash \\",
            "deep": [[[[true]]]]
        })
        .to_string();
        let RawInvocation::Completed(output) = host.invoke_probe(
            "input => [Object.getPrototypeOf(input) === Object.prototype, Object.prototype.hasOwnProperty.call(input, '__proto__'), input.__proto__.constructor, input.constructor, input.unicode, input.escaped, input.deep[0][0][0][0]]",
            input.as_bytes(),
        ) else {
            panic!("safe data transfer probe did not complete");
        };
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap(),
            serde_json::json!([
                true,
                true,
                "source ) ; throw new Error('x')",
                "ordinary own property",
                "\u{2028}\u{2029}",
                "quote \" and backslash \\",
                true
            ])
        );
    }
}
