//! Canonical decimal strings preserve the complete u64 counter in strict JSON.
use serde::{Deserialize, Deserializer, Serializer, de::Error};

/// Serialize the counter without losing precision in JSON consumers.
/// # Errors
/// Propagates the serializer's errors.
pub fn serialize<S: Serializer>(value: &u64, serializer: S) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&value.to_string())
}

/// Decode only canonical unsigned decimal strings.
/// # Errors
/// Rejects non-string, overflowing, signed or zero-padded representations.
pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<u64, D::Error> {
    let encoded = String::deserialize(deserializer)?;
    let value = encoded.parse::<u64>().map_err(D::Error::custom)?;
    if value.to_string() != encoded {
        return Err(D::Error::custom(
            "allocation counter requires canonical decimal string",
        ));
    }
    Ok(value)
}

#[cfg(test)]
mod tests {
    #[derive(Debug, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
    struct Counter {
        #[serde(with = "super")]
        value: u64,
    }

    #[test]
    fn full_u64_survives_strict_canonical_json() -> Result<(), Box<dyn std::error::Error>> {
        let expected = Counter { value: u64::MAX };
        let encoded = compiler_core::canonical_bytes(&expected)?;
        let parsed = compiler_core::parse_strict(std::str::from_utf8(&encoded)?)?;
        let actual: Counter = serde_json::from_value(parsed)?;
        assert_eq!(actual, expected);
        for encoded in [
            r#"{"value":1}"#,
            r#"{"value":"01"}"#,
            r#"{"value":"+1"}"#,
            r#"{"value":"18446744073709551616"}"#,
        ] {
            assert!(serde_json::from_str::<Counter>(encoded).is_err());
        }
        Ok(())
    }
}
