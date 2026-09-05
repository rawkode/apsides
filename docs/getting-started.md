---
title: Getting started
description: Compile and inspect the checked-in Reakt example locally.
---

This path compiles and inspects the checked-in TypeScript example. It does not
deploy to a cluster or establish production readiness.

## Prerequisites

- Rust and Cargo as selected by `rust-toolchain.toml`
- Node.js for the source compiler
- Deno for type checking and permission-restricted evaluation
- the repository's JavaScript dependencies installed with pnpm

The full native image build also needs Docker BuildKit, locally cached pinned
builder and runtime images, prepared Cargo dependencies, and
`cargo-cyclonedx`. Those requirements are covered in [Build and deploy](/guides/build-and-deploy/).

## Build the CLI

From the repository root:

```sh
cargo build -p cli
target/debug/reakt --help
```

The Cargo package is `cli`; its binary is `reakt`.

## Compile the example

Choose a fresh output directory. The compiler refuses to reuse an existing
source staging directory.

```sh
target/debug/reakt compile \
  --source examples/reakt/program.ts \
  --output work/reakt-compiled
```

The output includes validated IR, a recomputed plan, capabilities, test
scenarios, source mapping, generated Rust controller source and integrity
metadata. Compilation alone does not create a deployable release.

## Inspect and check the plan

```sh
target/debug/reakt check --ir work/reakt-compiled/ir.json
target/debug/reakt test \
  --ir work/reakt-compiled/ir.json \
  --output work/reakt-semantic-tests
```

`check` reports validation status, IR digest and whether the IR is production
input. `test` executes the compiler's required semantic scenarios into another
fresh directory. It can intentionally return nonzero when required reactor
coverage is unsupported; a passing semantic run does not satisfy the production
release gate. Neither command contacts Kubernetes.

## Read the result honestly

A successful local run shows that this checkout can compile and validate the
example in the current environment. It does not establish release signatures,
image reproducibility, Kubernetes authorization, controller activation,
workload readiness or upgrade recovery.

Continue with [TypeScript authoring](/guides/typescript-authoring/), then read the
[build and deployment boundaries](/guides/build-and-deploy/).
