---
title: Apsides
description: Documentation for the Apsides compiled Kubernetes controller toolchain.
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
  <p>Apsides is under active implementation. Candidate builds and focused lifecycle runs exist, but Apsides is not ready for production use. Start with the <a href="/reference/status/">implementation status</a> before evaluating it for a cluster.</p>
</div>

## Names used by the current implementation

**Apsides** is the project. The current CLI command is `aps`; commands in this
site use that executable name. Existing `apsides.dev` schema and protocol identifiers
remain active wire names. TypeScript authoring uses `@apsides/kubernetes`.

## The working model

<div class="architecture-flow">
  <div><strong>Declare</strong><span>TSX components describe fixed resources, selected observations and rollout reactions.</span></div>
  <div><strong>Compile</strong><span>The source closure is evaluated with denied host permissions and becomes validated IR.</span></div>
  <div><strong>Review</strong><span>Capabilities, force risks, lifecycle policy, release integrity and provenance are checked.</span></div>
  <div><strong>Deploy</strong><span>Privileged CI stages a native controller generation, quiesces the old writer and publishes inventory.</span></div>
</div>

Reconciliation uses Kubernetes Server-Side Apply (SSA) only. Force is opt-in at
the domain or resource level and remains subject to independent policy. Resource
history is metadata-only inventory through an `InventoryStore` trait, with
SQLite as the default local backend. OCI is an optional way to
distribute the new release; upgrades do not require retrieving historical OCI
artifacts.

[Follow the first local workflow →](/getting-started/)
