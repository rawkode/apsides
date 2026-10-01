//! The JavaScript surface a handler sees, pinned against the type
//! declarations the SDK ships for it (`packages/kubernetes/src/handler.ts`)
//! and the ECMAScript-only `lib` the compiler type-checks handler modules
//! with. A change to either side must update the other: this test fails
//! when the engine grows or loses a global the declarations describe, and
//! the compiler's own tests fail when the declarations drift from what a
//! handler module may call.

mod common;

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use common::{handler_invocation, wrap_as_handler};
use engine::invoke_handler_with_watchdog;
use serde_json::json;

/// Host (non-ECMAScript) globals a TypeScript author might reach for.
/// Every one is absent except `queueMicrotask`, which the SDK declares;
/// the rest being absent is what lets the compiler check handlers against
/// `lib: ["esnext"]` plus those declarations.
const HOST_GLOBALS: &[&str] = &[
    "console",
    "Deno",
    "URL",
    "URLSearchParams",
    "TextEncoder",
    "TextDecoder",
    "fetch",
    "setTimeout",
    "setInterval",
    "queueMicrotask",
    "structuredClone",
    "atob",
    "btoa",
    "crypto",
    "performance",
    "process",
    "require",
    "WebAssembly",
    "SharedArrayBuffer",
];

#[test]
fn handler_globals_match_the_sdk_declarations() -> Result<(), String> {
    let probes = HOST_GLOBALS
        .iter()
        .map(|name| format!("{name:?}: typeof globalThis[{name:?}]"))
        .collect::<Vec<_>>()
        .join(", ");
    let bundle = wrap_as_handler(&format!(
        r"
        return {{
            apsides: Object.getOwnPropertyNames(apsides).sort(),
            frozen: Object.isFrozen(apsides),
            members: Object.getOwnPropertyNames(apsides).sort().map(name => typeof apsides[name]),
            host: {{ {probes} }},
        }};
    "
    ));
    let report =
        invoke_handler_with_watchdog(handler_invocation(&bundle), &Arc::new(AtomicUsize::new(0)));
    let value = report
        .result
        .map_err(|err| format!("probe handler failed: {err}"))?;
    let host = HOST_GLOBALS
        .iter()
        .map(|name| {
            let kind = if *name == "queueMicrotask" {
                "function"
            } else {
                "undefined"
            };
            ((*name).to_owned(), json!(kind))
        })
        .collect::<serde_json::Map<_, _>>();
    let expected = json!({
        "apsides": ["fetch", "log", "writeOutput"],
        "frozen": true,
        "members": ["function", "function", "function"],
        "host": host,
    });
    if value != expected {
        return Err(format!(
            "the handler global surface changed; update packages/kubernetes/src/handler.ts \
             and this test together.\nexpected {expected}\ngot      {value}"
        ));
    }
    Ok(())
}
