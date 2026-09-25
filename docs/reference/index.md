---
title: Reference
description: Canonical Apsides specifications, contracts, and implementation status.
---

The contract documents below are the canonical source for implementation and
review. The guide pages explain the main path; they do not replace these
contracts.

<div class="contract-grid">
  <a href="/reference/spec/">Specification</a>
  <a href="/reference/status/">Implementation status</a>
  <a href="/reference/acceptance/">Acceptance suite</a>
  <a href="/reference/ir-contract/">IR contract</a>
  <a href="/reference/platform-contract/">Platform contract</a>
  <a href="/reference/deployment-contract/">Deployment protocol</a>
  <a href="/reference/deployment-permissions/">Deployment permissions</a>
  <a href="/reference/review-contract/">Deployment review</a>
  <a href="/reference/inventory/">Resource inventory</a>
  <a href="/reference/audit/">Transition audit history</a>
  <a href="/reference/state-capacity/">State capacity</a>
  <a href="/reference/artifact-resolution/">Artifact resolution</a>
  <a href="/reference/oci-packaging/">OCI packaging</a>
  <a href="/reference/cli-distribution/">CLI distribution</a>
  <a href="/reference/toolchain/">Toolchain</a>
</div>

## Choosing a document

| Question | Read |
| --- | --- |
| What is the intended system? | [Specification](/reference/spec/) |
| How do event handlers run, and what can they reach? | [Event handlers](/guides/event-handlers/) and the [IR contract](/reference/ir-contract/#handlers-and-outputs) |
| What works today, and what remains open? | [Implementation status](/reference/status/) |
| How is the implementation accepted against the examples? | [Acceptance suite](/reference/acceptance/) |
| What TypeScript can compile into IR? | [IR contract](/reference/ir-contract/) |
| Where are trust and runtime authority enforced? | [Platform contract](/reference/platform-contract/) |
| How do generation changes and recovery work? | [Deployment protocol](/reference/deployment-contract/) |
| Which permissions belong to CI and runtime? | [Deployment permissions](/reference/deployment-permissions/) |
| What must an independent review accept before deployment? | [Deployment review](/reference/review-contract/) |
| What persists for prune and upgrades? | [Resource inventory](/reference/inventory/), [transition audit](/reference/audit/), and [state capacity](/reference/state-capacity/) |
| How can a current release be retrieved? | [Artifact resolution](/reference/artifact-resolution/) |
| How is a native image constructed? | [OCI packaging](/reference/oci-packaging/) |
| How is the CLI packaged and distributed? | [CLI distribution](/reference/cli-distribution/) |
| Which OSS tools and isolation boundaries are used? | [Toolchain](/reference/toolchain/) |
