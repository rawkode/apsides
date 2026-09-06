---
title: Reference
description: Canonical Apsides specifications, contracts, and implementation status.
---

The contract documents below are the canonical source for implementation and
review. The guide pages explain the main path; they do not replace these
contracts.

<div class="contract-grid">
  <a href="/reakt/spec-v0-2/">Specification 0.2</a>
  <a href="/reakt/status/">Implementation status</a>
  <a href="/reakt/ir-contract/">IR contract</a>
  <a href="/reakt/platform-contract/">Platform contract</a>
  <a href="/reakt/deployment-contract/">Deployment protocol</a>
  <a href="/reakt/deployment-permissions/">Deployment permissions</a>
  <a href="/reakt/inventory/">Resource inventory</a>
  <a href="/reakt/artifact-resolution/">Artifact resolution</a>
  <a href="/reakt/oci-packaging/">OCI packaging</a>
  <a href="/reakt/toolchain/">Toolchain</a>
  <a href="/reakt/performance/">Performance contract</a>
</div>

## Choosing a document

| Question | Read |
| --- | --- |
| What is the intended v0.2 system? | [Specification 0.2](/reakt/spec-v0-2/) |
| What works today, and what remains open? | [Implementation status](/reakt/status/) |
| What TypeScript can compile into IR? | [IR contract](/reakt/ir-contract/) |
| Where are trust and runtime authority enforced? | [Platform contract](/reakt/platform-contract/) |
| How do generation changes and recovery work? | [Deployment protocol](/reakt/deployment-contract/) |
| Which permissions belong to CI and runtime? | [Deployment permissions](/reakt/deployment-permissions/) |
| What persists for prune and upgrades? | [Resource inventory](/reakt/inventory/), [transition audit](/reakt/audit/), and [state capacity](/reakt/state-capacity/) |
| How can a current release be retrieved? | [Artifact resolution](/reakt/artifact-resolution/) |
| How is a native image constructed? | [OCI packaging](/reakt/oci-packaging/) |
| Which OSS tools and isolation boundaries are used? | [Toolchain](/reakt/toolchain/) |
| Is there a performance comparison? | [Performance contract](/reakt/performance/), which records why no completed comparison is claimed |
