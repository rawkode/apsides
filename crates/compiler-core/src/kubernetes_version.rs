//! Parsing and comparison for Kubernetes `MAJOR.MINOR` version floors.
//!
//! Two different shapes appear here.
//!
//! A domain's own declared `minKubernetesVersion` is a strict
//! `"MAJOR.MINOR"` string (major fixed at 1, minor at or above the
//! qualified floor). An observed server's discovery `major`/`minor` fields
//! (from `/version`) are looser: `minor` may carry a non-digit suffix such
//! as `"35+"`.

/// The smallest Kubernetes minor version a declared `minKubernetesVersion`
/// may name.
///
/// This is the smallest release the absent-only conditional SSA create
/// contract (`apply_absent_with_list_version`) is qualified for.
pub const QUALIFIED_FLOOR: (u32, u32) = (1, 34);

/// Parses a strict `"MAJOR.MINOR"` string: digits only, exactly one dot, no
/// leading `+`/sign and no surrounding whitespace. Returns `None` for any
/// other shape.
#[must_use]
pub fn parse_min_version(value: &str) -> Option<(u32, u32)> {
    let (major, minor) = value.split_once('.')?;
    if major.is_empty()
        || minor.is_empty()
        || !major.bytes().all(|b| b.is_ascii_digit())
        || !minor.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// True when `value` is a `"MAJOR.MINOR"` string at or above the qualified
/// floor (major fixed at 1, minor >= 34).
#[must_use]
pub fn is_supported_min_version(value: &str) -> bool {
    matches!(parse_min_version(value), Some((major, minor)) if major == QUALIFIED_FLOOR.0 && minor >= QUALIFIED_FLOOR.1)
}

/// Parses an observed server's discovery `major`/`minor` fields into a
/// comparable `(major, minor)` pair.
///
/// These come from `k8s_openapi::apimachinery::pkg::version::Info`. `minor`
/// may carry a non-digit suffix (Kubernetes marks some minors like
/// `"35+"`); any such suffix is stripped. Returns `None` when either field
/// has no leading digits.
#[must_use]
pub fn parse_server_version(major: &str, minor: &str) -> Option<(u32, u32)> {
    let major_digits: String = major.chars().take_while(char::is_ascii_digit).collect();
    let minor_digits: String = minor.chars().take_while(char::is_ascii_digit).collect();
    if major_digits.is_empty() || major_digits.len() != major.len() || minor_digits.is_empty() {
        return None;
    }
    Some((major_digits.parse().ok()?, minor_digits.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::{
        QUALIFIED_FLOOR, is_supported_min_version, parse_min_version, parse_server_version,
    };

    type Case = (&'static str, &'static str, Option<(u32, u32)>);

    #[test]
    fn version_parse_matrix() {
        let cases: &[Case] = &[
            ("1", "34", Some((1, 34))),
            ("1", "35+", Some((1, 35))),
            ("1", "33", Some((1, 33))),
            ("garbage", "34", None),
            ("1", "garbage", None),
            ("2", "0", Some((2, 0))),
        ];
        for (major, minor, expected) in cases {
            assert_eq!(
                parse_server_version(major, minor),
                *expected,
                "parse_server_version({major:?}, {minor:?})"
            );
        }
        // Meeting the 1.34 floor: 1.34 and 1.35+ qualify, 1.33 does not, and
        // 2.0 exceeds it.
        assert!(matches!(parse_server_version("1", "34"), Some(v) if v >= QUALIFIED_FLOOR));
        assert!(matches!(parse_server_version("1", "35+"), Some(v) if v >= QUALIFIED_FLOOR));
        assert!(matches!(parse_server_version("1", "33"), Some(v) if v < QUALIFIED_FLOOR));
        assert!(matches!(parse_server_version("2", "0"), Some(v) if v >= QUALIFIED_FLOOR));
        assert_eq!(parse_server_version("garbage", "garbage"), None);
    }

    #[test]
    fn declared_min_version_requires_major_one_and_floor_minor() {
        assert_eq!(parse_min_version("1.34"), Some((1, 34)));
        assert_eq!(parse_min_version("1.40"), Some((1, 40)));
        assert_eq!(parse_min_version("v1.34"), None);
        assert_eq!(parse_min_version("1.34.3"), None);
        assert_eq!(parse_min_version("1"), None);
        assert_eq!(parse_min_version(""), None);
        assert!(is_supported_min_version("1.34"));
        assert!(is_supported_min_version("1.40"));
        assert!(!is_supported_min_version("1.33"));
        assert!(!is_supported_min_version("2.0"));
        assert!(!is_supported_min_version("garbage"));
    }
}
