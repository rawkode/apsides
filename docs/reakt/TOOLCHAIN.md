# Toolchain choices

Reakt reuses established tooling for language processing, Kubernetes transport, cryptography, and packaging. Its own code defines the constrained IR, validates effects and policy, evaluates declared reactions, and models the deployment lifecycle.

| Responsibility | Implementation |
| --- | --- |
| TypeScript transformation | Oxc, using the workspace's pinned dependency |
| Closed module bundle | Rolldown |
| Build-time source evaluation | Pinned Endo SES `Compartment` and `lockdown`, hosted by Deno from a fixed stdin module identity with filesystem, network, environment, subprocess, FFI, system-information, and remote-import permissions denied |
| SDK type checking | Deno and TypeScript |
| Kubernetes API types and transport | k8s-openapi and kube-rs |
| Field ownership, merge, defaults and admission dry-run | Kubernetes server-side apply |
| Hashes and keyed sensitive projections | RustCrypto SHA-256 and HMAC |
| Native dependency preparation | Cargo lockfiles and Cargo vendor |
| OCI build and export | Docker BuildKit |

The compiler uses Node to host Oxc and Rolldown. A canonical `reakt.lock.json` declares every author module and digest; package imports are limited to `@reakt/kubernetes`, relative imports must resolve to that lock, and all bytes are snapshotted before either build. Deno type-checks a staged copy with remote and npm resolution disabled. Oxc/Rolldown then build the same memory snapshot twice with virtual `reakt:/source` and `reakt:/sdk` identities and source maps.

Deno evaluates the closed IIFE bundle twice in fresh processes using the fixed `file:///$deno$stdin.mts` entry identity, a fixed `TZ`/locale environment, seeded host randomness, and denied permissions. Each process loads the digest-recorded SES 2.3.0 bundle, calls `lockdown` with safe error, locale, domain, and regular-expression taming, and evaluates the program in a `Compartment` with no module hooks. The only custom endowment is the recursively hardened declared build input. SES supplies frozen language intrinsics, disables clock and random observations, removes host APIs such as Deno, WebAssembly, streams and workers from the compartment, prevents constructor escapes, and rejects intrinsic prototype mutation. The compiler does not maintain an ambient-global denylist. The generated controller contains Rust and embedded IR, with no TypeScript evaluator or reconciliation-time package loading.

`build-inputs.json` records separate `observedEqual` and `constrainedInputProof` results, every source and SDK digest, the canonical lock digest, compiler and installed Oxc/Rolldown/SES package digests, the exact SES evaluator digest, the workspace dependency-lock digest, Deno executable digest/version, Node platform, compiler options, lockdown options, endowments, and permission profile. It also emits a canonical `reakt.dev/compiler-closure/v1` identity that directly contains the recorded SDK and authoring toolchain. Its `nativeCompiler` field remains null until trusted packaging adds the builder image, Cargo lock, native source-file manifest, and recomputed trusted CLI identity; a source-only receipt is therefore incomplete for release promotion. The bounded claim applies only to canonical output from this locked, compartmentalized profile. It is not a claim that arbitrary JavaScript is deterministic across other runtimes, toolchains, operating systems, or architectures. Authenticated provenance still needs an established verification integration and independently configured trust policy.

Source builds also emit production `ir.json`, `program.mjs` and its map, `source-map.json`, and test-only `fixtures.json`, `fixture-ir.json`, `fixture-overlay.json`, `fixture-catalogue.json`, and `fixture-coverage.json`. The compiler reconstructs the fixture IR itself, permits only the `testOnly` marker plus Deployment container image/pull-policy changes, and compares the full diff. It derives synthetic observed inputs and all Deployment container substitutions from IR. The coverage catalogue retains reaction IDs, source locations, fixture and overlay identities, expected primitive assertions, test layer, and execution status for every required scenario. Fixture output remains unauthenticated test evidence until the artifact verifier and runner bind and execute it.

CI's intended interface remains `reakt deploy`, independent of the Kubernetes provider. Production deployment is not implemented by the current fixture path. The pure transition model must be connected to trusted artifact verification, Kubernetes persistence, short-lived identity, and qualified old-writer termination before that command can perform production mutations.
