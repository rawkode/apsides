# Rust kernel roadmap

OxideLoop is split into a trusted Rust control plane and an untrusted TypeScript authoring plane. Rust compiles and supervises bundles, projects cluster state into versioned JSON, validates renderer output, owns inventory and mutation decisions, and performs all Kubernetes I/O. TypeScript describes resources and pure enrichments through the SDK.

## M1: process renderer boundary

Ship the selected React-like runtime through a separately supervised renderer process. Define a framed request/response protocol around renderer ABI v0, enforce Linux cgroup or equivalent process limits, authenticate bundle digests, and prove restart behavior after timeout, memory pressure, malformed output, and fatal exit. Replace direct observation-array exposure with host operations that both return projected values and record reads. Keep cluster access out of the worker.

Gate: deterministic replay, hard memory and wall-clock enforcement, clean worker replacement, and zero unresolved capability-surface findings.

## M2: read-only controller

Introduce a CRD for a managed program reference, compile and cache immutable bundles, project one root plus declared observations, and run reconciliation without applying output. Record desired-state hashes, dependency watches, diagnostics, and status conditions. Resolve namespaced versus cluster-scoped references through discovery and an explicit root capability contract; do not infer cluster scope from an empty namespace alone.

Gate: envtest coverage for watch restarts, generation changes, stale observations, dependency indexing, deletion, and controller restart. The controller must prove that this milestone cannot write managed objects.

## M3: ownership and apply

Add server-side apply behind an explicit feature gate. Define field-manager identity, inventory persistence, adoption policy, readiness, pruning, finalizers, conflict behavior, and deletion recovery before enabling writes. Re-render from observed state; never allow JavaScript to issue imperative API calls.

P13-E keeps create as `POST` with strict no-adoption. Lazy SSA ownership
acquisition uses force only for fully fenced State Persist, inventory-pinned
ConfigMap/Service ConditionalApply, and normal WebApplication status after
bootstrap Merge. AutomationRun status and all POST, delete, finalizer, metadata,
and bootstrap paths remain non-force. A definite forced-write `409` receives one
proof-only reread, with no retry or stale rebase; unproven State/root status
holds, while child mutation emits a token-bound failure and then holds.
Canonical legacy objects may be lazily claimed on their next authorized write;
malformed or ownerless objects hold.

Gate: kind-backed integration tests for create/update/conflict/delete/restart flows, rollback tests for inventory state, and a default-deny adoption policy.

## M4: event-driven hooks

Model hooks as declarations in render output: dependencies identify event sources and automations identify immutable action modules. Deduplicate occurrences in Rust using source UID, resource version, automation identity, and occurrence ID. Actions return validated declarations such as Jobs or Events; they do not receive an ambient client.

Gate: replay and crash tests prove at-least-once delivery without duplicate materialization, bounded queues, explicit retry/dead-letter behavior, and namespace/RBAC policy enforcement.

## M5: hardening and release contract

Freeze a versioned ABI only after compatibility fixtures cover SDK, compiler, worker, and controller upgrades. Add signatures and provenance for bundles, supply-chain policy, multi-tenant isolation, metrics, tracing, leader election, backpressure, and upgrade/rollback procedures.

Gate: adversarial review, resource-exhaustion tests, conformance matrix, documented support window, and an operator-visible migration path.

Broad installed-cluster migration or reset remains an explicit M5 non-goal.

## Deliberate non-goals

- No Kubernetes client, filesystem, network, environment, wall clock, randomness, or dynamic module loading inside author code.
- No unrestricted React lifecycle or asynchronous hook semantics.
- No YAML generation as the runtime contract.
- No cluster writes until ownership, inventory, conflict, pruning, and recovery contracts are frozen and tested.
