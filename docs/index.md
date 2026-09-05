---
title: Apsides
description: Documentation for the Reakt compiled Kubernetes controller toolchain.
template: splash
hero:
  title: Compiled reactive GitOps for Kubernetes
  tagline: Author a bounded resource graph with TSX components, compile it into a fixed plan and native controller, then let privileged CI verify and deploy each generation.
  actions:
    - text: Get started
      link: /getting-started/
      icon: right-arrow
      variant: primary
    - text: Read the architecture
      link: /concepts/architecture/
      icon: open-book
      variant: minimal
---

<div class="status-strip">
  <strong>Release status</strong>
  <p>Reakt is under active implementation. Candidate builds and focused lifecycle runs exist, but the v1 production release gates are not complete. Start with the <a href="/reakt/status/">implementation status</a> before evaluating it for a cluster.</p>
</div>

## Repository and tool names

**Apsides** is the repository. The current implementation is named **Reakt**:
the CLI binary is `reakt`, the TypeScript package is `@reakt/kubernetes`, and
the generated controller package is `controller`. Commands in this site use
those names until a separate rename is implemented.

## The working model

<div class="architecture-flow">
  <div><strong>Declare</strong><span>TSX components describe fixed resources, selected observations and rollout reactions.</span></div>
  <div><strong>Compile</strong><span>The source closure is evaluated with denied host permissions and becomes validated IR.</span></div>
  <div><strong>Review</strong><span>Capabilities, force risks, lifecycle policy, provenance and release evidence are checked.</span></div>
  <div><strong>Deploy</strong><span>Privileged CI stages a native controller generation, quiesces the old writer and publishes inventory.</span></div>
</div>

Reconciliation uses Kubernetes Server-Side Apply (SSA) only. Force is opt-in at
the domain or resource level and remains subject to independent policy. Resource
history is metadata-only inventory through an `InventoryStore` trait, with
libSQL as the default local or remote backend. OCI is an optional way to
distribute the new release; upgrades do not require retrieving historical OCI
artifacts.

[Follow the first local workflow →](/getting-started/)
