# Provisional M0 limits

These are falsification targets for the spike, not production guarantees.

| Limit | Inclusive boundary |
|---|---:|
| Whole evaluation watchdog | 1 s |
| Watchdog termination grace | 100 ms |
| Bundled JavaScript source | 262,144 UTF-8 bytes |
| JSON input | 2,097,152 UTF-8 bytes |
| JSON output | 2,097,152 UTF-8 bytes |
| Any string | 262,144 UTF-8 bytes |
| Managed resource entries | 256 |
| Tracked dependencies | 1,024 |
| Automation declarations | 64 |
| Diagnostics | 64 |
| Canonical data depth | 64 |

The root value is depth zero; every object or array edge adds one. Depth 64 must pass and depth 65 must fail. Cycles, sparse holes, functions, symbols, `undefined`, `bigint`, non-finite numbers, negative zero, unsafe integer-valued numbers, duplicate JSON object keys, and non-JSON collections are rejected.

The byte caps apply after UTF-8 encoding. The output cap covers the entire returned value, including resources, dependencies, automation declarations, readiness declarations, and diagnostics. Where possible the host rejects an oversized encoded value before allocating a complete typed Rust model; any gap is recorded in the decision report.

Every isolate is single-use. Recoverable termination must be followed by a successful clean evaluation. A 128 MiB managed-heap target was considered but is not enforced by this spike. If watchdog termination does not stop V8 within the grace period, the host reports a closed `ProcessFatal` category but may leave its in-process worker thread alive; that is neither cleanup nor recovery evidence. Heap exhaustion, aborts, unexpected exits, parent watchdog kills, or unsupported OS memory enforcement disqualify the in-process host even when the supervising parent survives.
