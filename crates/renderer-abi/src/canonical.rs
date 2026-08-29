use std::cmp::Ordering;

use serde_json::{Number, Value};
use sha2::{Digest, Sha256};

use crate::AbiError;

/// Encode a JSON value using the renderer ABI's deterministic byte form.
///
/// # Errors
///
/// Returns [`AbiError`] when a number is not valid in the ABI's shared
/// JavaScript/Rust numeric domain.
pub fn canonical_bytes(value: &Value) -> Result<Vec<u8>, AbiError> {
    let mut output = Vec::new();
    write_value(value, &mut output)?;
    Ok(output)
}

/// Return the lowercase SHA-256 digest of `bytes`.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn write_value(value: &Value, output: &mut Vec<u8>) -> Result<(), AbiError> {
    match value {
        Value::Null => output.extend_from_slice(b"null"),
        Value::Bool(value) => output.extend_from_slice(if *value { b"true" } else { b"false" }),
        Value::Number(number) => output.extend_from_slice(canonical_number(number)?.as_bytes()),
        Value::String(value) => output.extend_from_slice(
            serde_json::to_string(value)
                .expect("serializing a Rust string cannot fail")
                .as_bytes(),
        ),
        Value::Array(values) => {
            output.push(b'[');
            for (index, value) in values.iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                write_value(value, output)?;
            }
            output.push(b']');
        }
        Value::Object(values) => {
            let mut entries: Vec<_> = values.iter().collect();
            entries.sort_by(|(left, _), (right, _)| compare_utf16(left, right));

            output.push(b'{');
            for (index, (key, value)) in entries.into_iter().enumerate() {
                if index != 0 {
                    output.push(b',');
                }
                output.extend_from_slice(
                    serde_json::to_string(key)
                        .expect("serializing a Rust string cannot fail")
                        .as_bytes(),
                );
                output.push(b':');
                write_value(value, output)?;
            }
            output.push(b'}');
        }
    }
    Ok(())
}

fn canonical_number(number: &Number) -> Result<String, AbiError> {
    let value = number.as_f64().ok_or(AbiError::NumberNotBinary64)?;
    validate_number(value)?;

    if value.fract() == 0.0 {
        #[allow(clippy::cast_possible_truncation)]
        return Ok((value as i64).to_string());
    }

    Ok(ryu::Buffer::new().format_finite(value).to_owned())
}

pub(crate) fn validate_number(value: f64) -> Result<(), AbiError> {
    if !value.is_finite() {
        return Err(AbiError::NonFiniteNumber);
    }
    if value == 0.0 && value.is_sign_negative() {
        return Err(AbiError::NegativeZero);
    }
    if value.fract() == 0.0 && value.abs() > crate::limits::MAX_SAFE_INTEGER {
        return Err(AbiError::UnsafeInteger);
    }
    Ok(())
}

fn compare_utf16(left: &str, right: &str) -> Ordering {
    left.encode_utf16().cmp(right.encode_utf16())
}
