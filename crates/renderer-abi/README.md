# OxideLoop renderer ABI spike

This crate is the normative source for the provisional M0 renderer ABI. Rust
alone validates documents, produces canonical bytes, and hashes those bytes.
Generated JSON Schemas and TypeScript declarations are handoff artifacts, not
independent authorities.

The canonical encoding is deliberately described as **JCS-like**, not RFC 8785
compliant. It proves deterministic UTF-8 output for the accepted ABI subset:

- object keys are ordered by UTF-16 code units;
- strings use `serde_json` JSON escaping and are not Unicode-normalized;
- all accepted numbers are finite binary64 values, negative zero is rejected,
  integer-valued numbers outside JavaScript's safe-integer range are rejected,
  and equivalent accepted spellings are normalized through one Rust `f64`
  representation;
- duplicate keys and non-JSON JavaScript values are rejected before typed
  deserialization.

It does not claim full RFC 8785 number serialization. A later ABI may replace
the numeric formatter only by changing the ABI version and its goldens.
