//! Allocation guards shared by preview mechanisms; no object data enters errors.
use crate::{Result, invalid};
use serde_json::Value;
use std::io::{self, Write};

const MAX_DEPTH: usize = 64;
const MAX_NODES: usize = 100_000;

pub(crate) fn validate_json(value: &Value, max_bytes: usize) -> Result<()> {
    // Check structure before recursive serialization, cloning or comparison. Bound
    // pending references as well as visited nodes before extending the stack.
    let mut pending = vec![(value, 0_usize)];
    let mut visited = 0_usize;
    while let Some((value, depth)) = pending.pop() {
        visited += 1;
        if depth > MAX_DEPTH || visited > MAX_NODES {
            return Err(invalid("preview JSON structure exceeds limits"));
        }
        let children = match value {
            Value::Object(values) => values.len(),
            Value::Array(values) => values.len(),
            _ => 0,
        };
        if children > MAX_NODES.saturating_sub(visited + pending.len()) {
            return Err(invalid("preview JSON structure exceeds limits"));
        }
        match value {
            Value::Object(values) => {
                pending.extend(values.values().map(|child| (child, depth + 1)));
            }
            Value::Array(values) => {
                pending.extend(values.iter().map(|child| (child, depth + 1)));
            }
            _ => {}
        }
    }
    validate_serialized(value, max_bytes)
}

/// Only for structurally bounded typed output or JSON already checked above.
pub(crate) fn validate_serialized(value: &impl serde::Serialize, max_bytes: usize) -> Result<()> {
    serde_json::to_writer(
        ByteBudget {
            remaining: max_bytes,
        },
        value,
    )
    .map_err(|_| invalid("preview JSON exceeds byte limit"))
}

struct ByteBudget {
    remaining: usize,
}

impl Write for ByteBudget {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(io::Error::other("preview JSON exceeds byte limit"));
        }
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exact_bytes_include_escaping() {
        let value = json!("\n");
        assert!(validate_json(&value, 4).is_ok());
        assert!(validate_json(&value, 3).is_err());
    }

    #[test]
    fn depth_and_pending_children_are_bounded_before_serialization() {
        let mut value = Value::Null;
        for _ in 0..MAX_DEPTH {
            value = Value::Array(vec![value]);
        }
        assert!(validate_json(&value, 1024).is_ok());
        value = Value::Array(vec![value]);
        assert!(validate_json(&value, 1024).is_err());
        let broad = Value::Array(vec![Value::Null; MAX_NODES]);
        assert!(validate_json(&broad, usize::MAX).is_err());
    }
}
