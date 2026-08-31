use serde_json::Value;

use crate::{AbiError, canonical};

pub const MAX_CANONICAL_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_STRING_BYTES: usize = 256 * 1024;
pub const MAX_DEPTH: usize = 64;
pub const MAX_RESOURCES: usize = 256;
pub const MAX_DEPENDENCIES: usize = 1_024;
pub const MAX_AUTOMATIONS: usize = 64;
pub const MAX_DIAGNOSTICS: usize = 64;
pub const MAX_ACTIONS: usize = 64;
/// Maximum canonical UTF-8 JSON size for automation event metadata. This is
/// shared by render declarations and action inputs so the durable action
/// protocol cannot receive metadata accepted by the render ABI but rejected
/// by its action phase.
pub const MAX_EVENT_METADATA_CANONICAL_BYTES: usize = 512 * 1024;
pub const MAX_SAFE_INTEGER: f64 = 9_007_199_254_740_991.0;

/// Validate recursive and canonical-size limits for an untyped JSON value.
///
/// # Errors
///
/// Returns [`AbiError`] when the value exceeds a depth, string, canonical-byte,
/// or shared numeric-domain limit.
pub fn validate_json_limits(value: &Value) -> Result<(), AbiError> {
    validate_value(value, 0)?;
    let byte_count = canonical::canonical_bytes(value)?.len();
    if byte_count > MAX_CANONICAL_BYTES {
        return Err(AbiError::CanonicalBytesLimit {
            actual: byte_count,
            maximum: MAX_CANONICAL_BYTES,
        });
    }
    Ok(())
}

/// Validate JSON limits for event metadata and apply the tighter action
/// protocol bound in canonical UTF-8 bytes.
///
/// # Errors
///
/// Returns [`AbiError`] when the metadata exceeds a shared JSON limit or the
/// tighter canonical-byte bound for automation event metadata.
pub fn validate_event_metadata(value: &Value) -> Result<(), AbiError> {
    validate_json_limits(value)?;
    let byte_count = canonical::canonical_bytes(value)?.len();
    if byte_count > MAX_EVENT_METADATA_CANONICAL_BYTES {
        return Err(AbiError::CanonicalBytesLimit {
            actual: byte_count,
            maximum: MAX_EVENT_METADATA_CANONICAL_BYTES,
        });
    }
    Ok(())
}

fn validate_value(value: &Value, depth: usize) -> Result<(), AbiError> {
    if depth > MAX_DEPTH {
        return Err(AbiError::DepthLimit {
            actual: depth,
            maximum: MAX_DEPTH,
        });
    }

    match value {
        Value::String(value) => validate_string(value)?,
        Value::Number(number) => {
            let value = number.as_f64().ok_or(AbiError::NumberNotBinary64)?;
            canonical::validate_number(value)?;
        }
        Value::Array(values) => {
            for value in values {
                validate_value(value, depth + 1)?;
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                validate_string(key)?;
                validate_value(value, depth + 1)?;
            }
        }
        Value::Null | Value::Bool(_) => {}
    }
    Ok(())
}

fn validate_string(value: &str) -> Result<(), AbiError> {
    let byte_count = value.len();
    if byte_count > MAX_STRING_BYTES {
        return Err(AbiError::StringLimit {
            actual: byte_count,
            maximum: MAX_STRING_BYTES,
        });
    }
    Ok(())
}
