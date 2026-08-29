# Renderer protocol v0

The fixed binary framing and Rust validation in this crate are normative. The
generated JSON Schemas and TypeScript declarations are structural views for
tooling: they do not encode phase-dependent module requirements, payload caps,
the 1–1000 ms duration bound, or the response status/category matrix. Consumers
must pass every frame through the Rust `RequestV0::validate`,
`ResponseV0::validate`, and `read_frame` contracts before acting on it.
