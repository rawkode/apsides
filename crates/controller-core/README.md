# OxideLoop controller core

This crate is a pure deterministic reducer. It does not import `kube`,
`k8s-openapi`, an API client, a clock, or a renderer host; adapters supply
snapshots and execute the returned token-bound commands.

`oxideloop-controller-api` is intentionally shared for CRD durable DTOs and
currently has a transitive `kube` derive dependency. That transitive dependency
is not evidence that this crate has a Kubernetes execution path: no `kube`
types are exposed by the reducer and no API operation can occur here. Splitting
the DTO crate from CRD derives is a separate compatibility change.
