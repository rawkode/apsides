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

/// Parse bounded JSON without duplicate keys or ambiguous numbers.
///
/// # Errors
/// Rejects malformed, oversized, duplicate-key, or nonintegral JSON.
pub fn parse_strict(input: &str) -> Result<Value> {
    if input.len() > 4 * 1024 * 1024 {
        return Err(Error::Invalid("IR exceeds 4 MiB".into()));
    }
    serde_json::from_str::<StrictValue>(input)
        .map(|v| v.0)
        .map_err(|_| {
            Error::Invalid(
                "invalid JSON, duplicate key, depth limit, or unsupported numeric representation"
                    .into(),
            )
        })
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
