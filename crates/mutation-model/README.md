# OxideLoop conditional-mutation model

This crate is a pure model and serialization probe. It does not call a
Kubernetes API and passing its tests proves nothing about an API server's
conditional mutation behavior. Runtime use must remain behind a separately
qualified GVK/backend compatibility class.

The model intentionally has no adoption path. A create collision returns a
refetch/no-adopt decision; existing objects can only be changed when both UID
and resourceVersion match the caller's expectation.
