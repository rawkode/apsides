use renderer_abi::{
    AbiError, canonical_bytes,
    limits::{MAX_CANONICAL_BYTES, MAX_DEPTH, MAX_STRING_BYTES, validate_json_limits},
    parse_strict_value, sha256_hex,
};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GoldenCase {
    inputs: Vec<String>,
    canonical: String,
    sha256: String,
}

#[test]
fn rejects_duplicate_keys_at_any_depth() {
    let error = parse_strict_value(r#"{"outer":{"same":1,"same":2}}"#).unwrap_err();
    assert!(error.to_string().contains("duplicate object key `same`"));
}

#[test]
fn equivalent_json_spellings_have_one_rust_hash() {
    let fixture = include_str!("../../../abi/renderer/v0/fixtures/canonical-equivalence.json");
    let cases: Vec<GoldenCase> = serde_json::from_str(fixture).unwrap();

    for case in cases {
        for input in case.inputs {
            let value = parse_strict_value(&input).unwrap();
            validate_json_limits(&value).unwrap();
            let canonical = canonical_bytes(&value).unwrap();
            assert_eq!(
                String::from_utf8(canonical.clone()).unwrap(),
                case.canonical
            );
            assert_eq!(sha256_hex(&canonical), case.sha256);
        }
    }
}

#[test]
fn key_sorting_uses_utf16_code_units() {
    let value = json!({"\u{fffd}": "bmp", "\u{1f600}": "astral", "z": 1});
    let canonical = String::from_utf8(canonical_bytes(&value).unwrap()).unwrap();
    assert_eq!(canonical, r#"{"z":1,"😀":"astral","�":"bmp"}"#);
}

#[test]
fn rejects_negative_zero_and_unsafe_integer_values() {
    let negative_zero = parse_strict_value("-0.0").unwrap();
    assert_eq!(
        validate_json_limits(&negative_zero),
        Err(AbiError::NegativeZero)
    );

    let unsafe_integer = parse_strict_value("9007199254740992").unwrap();
    assert_eq!(
        validate_json_limits(&unsafe_integer),
        Err(AbiError::UnsafeInteger)
    );
}

#[test]
fn depth_64_passes_and_65_fails() {
    let mut at_limit = Value::Null;
    for _ in 0..MAX_DEPTH {
        at_limit = Value::Array(vec![at_limit]);
    }
    validate_json_limits(&at_limit).unwrap();

    let over_limit = Value::Array(vec![at_limit]);
    assert_eq!(
        validate_json_limits(&over_limit),
        Err(AbiError::DepthLimit {
            actual: MAX_DEPTH + 1,
            maximum: MAX_DEPTH,
        })
    );
}

#[test]
fn string_limit_is_inclusive() {
    validate_json_limits(&Value::String("x".repeat(MAX_STRING_BYTES))).unwrap();
    assert_eq!(
        validate_json_limits(&Value::String("x".repeat(MAX_STRING_BYTES + 1))),
        Err(AbiError::StringLimit {
            actual: MAX_STRING_BYTES + 1,
            maximum: MAX_STRING_BYTES,
        })
    );
}

#[test]
fn canonical_byte_limit_is_inclusive() {
    // Seven maximum-size strings plus one 262,119-byte string, quotes,
    // commas and brackets encode to exactly 2 MiB.
    let mut values = vec![Value::String("x".repeat(MAX_STRING_BYTES)); 7];
    values.push(Value::String("x".repeat(262_119)));
    let at_limit = Value::Array(values);
    assert_eq!(
        canonical_bytes(&at_limit).unwrap().len(),
        MAX_CANONICAL_BYTES
    );
    validate_json_limits(&at_limit).unwrap();

    let mut over_limit = at_limit;
    over_limit.as_array_mut().unwrap()[7] = Value::String("x".repeat(262_120));
    assert_eq!(
        validate_json_limits(&over_limit),
        Err(AbiError::CanonicalBytesLimit {
            actual: MAX_CANONICAL_BYTES + 1,
            maximum: MAX_CANONICAL_BYTES,
        })
    );
}
