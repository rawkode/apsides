---
title: Getting started
description: Compile and inspect the checked-in Apsides example locally.
---

This path compiles and inspects the checked-in TypeScript example. It does not
deploy to a cluster or establish production readiness.

## Prerequisites

- Rust and Cargo as selected by `rust-toolchain.toml`
- Deno, which hosts the source compiler and performs its type checking and
  permission-restricted evaluation
- the repository's JavaScript dependencies installed with `deno install`

The full native image build also needs Docker BuildKit, locally cached pinned
builder and runtime images, prepared Cargo dependencies, and
`cargo-cyclonedx`. Those requirements are covered in [Build and deploy](/guides/build-and-deploy/).

## Build the CLI

From the repository root:

```sh
cargo build --locked -p cli
target/debug/aps --help
```

The Cargo package is `cli`; its binary is `aps`.

## Compile the example

Choose a fresh output directory. The compiler refuses to reuse an existing
source staging directory. On a clean Cargo cache, run `cargo fetch --locked`
first; without it, `aps compile` fails with "prepared native dependencies are
incomplete".

```sh
cargo fetch --locked
target/debug/aps compile \
  --source examples/web-application/program.tsx \
  --output work/apsides-compiled
```

The output includes validated IR, a recomputed plan, capabilities, the
`surface.json` deployment surface, test scenarios, source mapping, generated
Rust controller source and integrity metadata. Compilation alone does not create a deployable release.

## Inspect and check the plan

```sh
target/debug/aps check --ir work/apsides-compiled/ir.json
target/debug/aps test \
  --ir work/apsides-compiled/ir.json \
  --output work/apsides-semantic-tests
```

`check` reports validation status, IR digest and whether the IR is production
input. `test` executes the compiler's required semantic scenarios into another
fresh directory. Required cases that need a cluster are reported as
unsupported, so it returns nonzero for any program with such cases. Neither
command contacts Kubernetes. `aps test --cluster` runs those cases too, against
a fixture build of the controller on a disposable kind cluster; see
[Build and deploy](/guides/build-and-deploy/#run-the-reactor-tests-on-a-cluster).

## Read the result honestly

A successful local run shows that this checkout can compile and validate the
example in the current environment. It does not establish release signatures,
image reproducibility, Kubernetes authorization, controller activation,
workload readiness or upgrade recovery.

Continue with [TypeScript authoring](/guides/typescript-authoring/), generate a
[typed custom resource from a CRD](/guides/custom-resources/), add an
[event handler](/guides/event-handlers/), then read the
[build and deployment boundaries](/guides/build-and-deploy/).
