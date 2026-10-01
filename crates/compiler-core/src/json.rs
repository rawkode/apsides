use serde::de::{self, Deserialize, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::fmt;

use crate::{Error, Result};

struct StrictValue(Value);
impl<'de> Deserialize<'de> for StrictValue {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        struct StrictVisitor;
        impl<'de> Visitor<'de> for StrictVisitor {
            type Value = StrictValue;
            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("canonicalizable JSON without duplicate keys or fractional numbers")
            }
            fn visit_bool<E: de::Error>(self, v: bool) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Bool(v)))
            }
            fn visit_i64<E: de::Error>(self, v: i64) -> std::result::Result<Self::Value, E> {
                if v.unsigned_abs() > 9_007_199_254_740_991 {
                    return Err(E::custom("integer exceeds interoperable JSON range"));
                }
                Ok(StrictValue(v.into()))
            }
            fn visit_u64<E: de::Error>(self, v: u64) -> std::result::Result<Self::Value, E> {
                if v > 9_007_199_254_740_991 {
                    return Err(E::custom("integer exceeds interoperable JSON range"));
                }
                Ok(StrictValue(v.into()))
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Self::Value, E> {
                Err(E::custom(
                    "fractional or exponent-form numbers are unsupported",
                ))
            }
            fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v.into())))
            }
            fn visit_string<E: de::Error>(self, v: String) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::String(v)))
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                Ok(StrictValue(Value::Null))
            }
            fn visit_none<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
                self.visit_unit()
            }
            fn visit_seq<A: SeqAccess<'de>>(
                self,
                mut seq: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Vec::new();
                while let Some(StrictValue(value)) = seq.next_element()? {
                    values.push(value);
                }
                Ok(StrictValue(Value::Array(values)))
            }
            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> std::result::Result<Self::Value, A::Error> {
                let mut values = Map::new();
                while let Some((key, StrictValue(value))) =
                    map.next_entry::<String, StrictValue>()?
                {
                    if values.insert(key, value).is_some() {
                        return Err(de::Error::custom("duplicate JSON object key"));
                    }
                }
                Ok(StrictValue(Value::Object(values)))
            }
        }
        deserializer.deserialize_any(StrictVisitor)
    }
}

/// The IR document's own size budget.
///
/// Real programs stay well under this (compile-time
/// `maxResources`/`maxInputs`/etc. budgets keep IR itself small), so this
/// can stay tight. [`parse_strict`] is IR's dedicated reader; other document
/// types must not share this limit or its message -- see [`parse_bounded`].
pub const MAX_IR_BYTES: usize = 4 * 1024 * 1024;

/// `build-inputs.json`'s own size budget.
///
/// Unlike IR, it is not budget-bounded by the IR spec: its
/// `compilerClosure.nativeCompiler.buildContextFiles` records a `{path,
/// sha256}` pair for every file `cargo vendor` stages (thousands, once
/// `crates/engine`'s `deno_core` pulls in its own dependency closure --
/// measured over 5 MiB for that field alone against the real Cargo.lock as
/// of this pin). Matches `crates/release`'s existing
/// `MAX_DOCUMENT`/`MAX_COMPILER_CLOSURE_BYTES` (both already 8 MiB), so a
/// release document's raw-byte cap and its parse-time cap agree instead of
/// the tighter one silently overriding it.
pub const MAX_BUILD_INPUTS_BYTES: usize = 8 * 1024 * 1024;

/// Parse bounded JSON without duplicate keys or ambiguous numbers.
///
/// `document` names what is being parsed (e.g. `"IR"`, `"build-inputs.json"`,
/// `"release.json"`) so a caller reading something other than IR gets an
/// error that says so, rather than [`parse_strict`]'s IR-specific message
/// regardless of what actually exceeded its limit.
///
/// # Errors
/// Rejects malformed, oversized, duplicate-key, or nonintegral JSON.
pub fn parse_bounded(input: &str, max_bytes: usize, document: &str) -> Result<Value> {
    if input.len() > max_bytes {
        return Err(Error::Invalid(format!(
            "{document} exceeds {} MiB",
            max_bytes / (1024 * 1024)
        )));
    }
    serde_json::from_str::<StrictValue>(input)
        .map(|v| v.0)
        .map_err(|_| {
            Error::Invalid(format!(
                "{document}: invalid JSON, duplicate key, depth limit, or unsupported numeric \
                 representation"
            ))
        })
}

/// [`parse_bounded`] for IR specifically, at IR's own tight [`MAX_IR_BYTES`]
/// budget.
///
/// Every other document type (release documents, build inputs, policies,
/// stored fields, ...) must call [`parse_bounded`] directly with its own
/// limit and name instead of reusing this: IR's budget is not a generic
/// "JSON document" budget, and reusing it produces a misleading "IR exceeds
/// 4 MiB" error for a document that was never IR.
///
/// # Errors
/// Rejects malformed, oversized, duplicate-key, or nonintegral JSON.
pub fn parse_strict(input: &str) -> Result<Value> {
    parse_bounded(input, MAX_IR_BYTES, "IR")
}

/// Serialize JSON with sorted object keys and preserved array order.
///
/// # Errors
/// Returns an error if the supplied value cannot be represented as JSON.
pub fn canonical_bytes(value: &impl serde::Serialize) -> Result<Vec<u8>> {
    // serde_json's default map backend is sorted. Explicit recursion makes the
    // byte contract independent of feature unification with preserve_order.
    fn sort(value: Value) -> Value {
        match value {
            Value::Object(map) => {
                let sorted: std::collections::BTreeMap<_, _> =
                    map.into_iter().map(|(k, v)| (k, sort(v))).collect();
                Value::Object(sorted.into_iter().collect())
            }
            Value::Array(values) => Value::Array(values.into_iter().map(sort).collect()),
            other => other,
        }
    }
    let value = serde_json::to_value(value)
        .map_err(|_| Error::Invalid("value cannot be serialized".into()))?;
    serde_json::to_vec(&sort(value))
        .map_err(|_| Error::Invalid("value cannot be serialized".into()))
}

/// Hash canonical JSON using SHA-256.
///
/// # Errors
/// Returns an error when canonical serialization fails.
pub fn digest(value: &impl serde::Serialize) -> Result<String> {
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(canonical_bytes(value)?)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

    /// A syntactically valid JSON document (one large string value) of at
    /// least `bytes` bytes.
    fn oversized_document(bytes: usize) -> String {
        format!("{{\"padding\":\"{}\"}}", "a".repeat(bytes))
    }

    #[test]
    fn parse_strict_rejects_ir_over_its_own_budget_by_name() -> TestResult {
        let oversized = oversized_document(MAX_IR_BYTES);
        let Err(error) = parse_strict(&oversized) else {
            return Err("expected oversized IR to be rejected".into());
        };
        let error = error.to_string();
        assert!(error.contains("IR"), "{error}");
        assert!(error.contains("4 MiB"), "{error}");
        Ok(())
    }

    #[test]
    fn parse_bounded_accepts_a_document_that_would_exceed_the_ir_budget() -> TestResult {
        // Bigger than MAX_IR_BYTES -- parse_strict would reject this -- but
        // within a document type's own larger budget, e.g.
        // build-inputs.json's MAX_BUILD_INPUTS_BYTES: a compiler closure's
        // build-context file manifest alone measured over 5 MiB against the
        // real Cargo.lock once crates/engine's deno_core was added.
        let oversized = oversized_document(MAX_IR_BYTES + 1024);
        assert!(parse_strict(&oversized).is_err());
        let value = parse_bounded(&oversized, MAX_BUILD_INPUTS_BYTES, "build-inputs.json")?;
        let Some(padding) = value["padding"].as_str() else {
            return Err("expected a string padding field".into());
        };
        assert_eq!(padding.len(), MAX_IR_BYTES + 1024);
        Ok(())
    }

    #[test]
    fn parse_bounded_names_the_actual_document_type_that_exceeded_its_own_limit() -> TestResult {
        let oversized = oversized_document(MAX_BUILD_INPUTS_BYTES);
        let Err(error) = parse_bounded(&oversized, MAX_BUILD_INPUTS_BYTES, "build-inputs.json")
        else {
            return Err("expected an oversized build-inputs.json to be rejected".into());
        };
        let error = error.to_string();
        assert!(error.contains("build-inputs.json"), "{error}");
        assert!(!error.contains("IR"), "{error}");
        Ok(())
    }

    #[test]
    fn parse_bounded_rejects_malformed_json_by_document_name_too() -> TestResult {
        let Err(error) = parse_bounded("{not json", MAX_BUILD_INPUTS_BYTES, "build-inputs.json")
        else {
            return Err("expected malformed JSON to be rejected".into());
        };
        let error = error.to_string();
        assert!(error.contains("build-inputs.json"), "{error}");
        Ok(())
    }
}
