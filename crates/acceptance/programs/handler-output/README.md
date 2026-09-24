# Handler output commit

One domain, one observed `ConfigMap` input (`handler-source`, created and
updated directly by the acceptance test, never managed by this domain), one
handler with no egress hosts that copies every key of that input's `data`
into its declared output (`handler-derived`), verbatim.

`acceptance-prepare` builds and signs it. `tests/kind/handlers.rs` deploys it
and exercises the handler commit path end to end on a real cluster: the
first invocation's written keys, a changed input dropping a key the handler
no longer writes, a foreign field manager's key surviving a handler commit,
and a controller restart writing nothing (see docs/reference/ACCEPTANCE.md).
