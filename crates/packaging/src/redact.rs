//! Redaction and bounding of subprocess output before it reaches a failure
//! message or the terminal.
//!
//! The rules are a conservative set of recognisable credential shapes:
//! bearer and API-key headers, basic-auth URLs, common provider token
//! prefixes, signed-URL query parameters and key, token, secret or password
//! assignments. They are not a complete secret scanner. Only subprocess output
//! is redacted; aps's own text never carries secrets.

use regex::Regex;
use std::sync::LazyLock;

pub const REDACTED: &str = "[REDACTED]";
/// How many trailing lines of captured output a failure message keeps.
pub const TAIL_LINES: usize = 200;

const RULES: [(&str, &str); 8] = [
    (r"(?i)(authorization:\s*bearer\s+)\S+", "${1}[REDACTED]"),
    (
        r"(?i)(x-[a-z0-9-]*(?:api-?key|token)\s*:\s*)\S+",
        "${1}[REDACTED]",
    ),
    (r"://[^/\s:@]+:[^/\s:@]+@", "://[REDACTED]@"),
    (r"\bgh[pousr]_[A-Za-z0-9]{20,}\b", "[REDACTED]"),
    (r"\bAKIA[0-9A-Z]{16}\b", "[REDACTED]"),
    (
        r"\bey[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\b",
        "[REDACTED]",
    ),
    (
        r#"(?i)([?&](?:sig|signature|token|jwt|sas|x-amz-signature|x-amz-credential|x-amz-security-token|access_token|api_key|apikey)=)[^&\s'"]+"#,
        "${1}[REDACTED]",
    ),
    (
        r"(?i)\b((?:api|access|secret|private)[-_]?key|token|password|passwd|secret|credential)(s?\s*[:=]\s*)\S+",
        "${1}${2}[REDACTED]",
    ),
];

/// The compiled rules, or `None` if any failed to compile. The patterns are
/// constants pinned by a test; should one ever fail, [`redact`] withholds the
/// whole text rather than show it unredacted.
static COMPILED: LazyLock<Option<Vec<(Regex, &'static str)>>> = LazyLock::new(|| {
    RULES
        .iter()
        .map(|(pattern, replacement)| Regex::new(pattern).ok().map(|rule| (rule, *replacement)))
        .collect()
});

/// `text` with every credential-shaped substring replaced by [`REDACTED`].
#[must_use]
pub fn redact(text: &str) -> String {
    let Some(rules) = COMPILED.as_ref() else {
        return REDACTED.to_owned();
    };
    rules
        .iter()
        .fold(text.to_owned(), |text, (rule, replacement)| {
            rule.replace_all(&text, *replacement).into_owned()
        })
}

/// The last `lines` lines of `text` joined by `\n`, or `text` unchanged when
/// it has no more than that.
#[must_use]
pub fn bounded(text: &str, lines: usize) -> String {
    let parts: Vec<&str> = text.lines().collect();
    if parts.len() <= lines {
        return text.to_owned();
    }
    parts[parts.len() - lines..].join("\n")
}

/// Redacted, bounded subprocess output, safe to put in a failure message.
#[must_use]
pub fn diagnostic(text: &str) -> String {
    bounded(&redact(text), TAIL_LINES)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_rule_compiles() {
        assert_eq!(COMPILED.as_ref().map(Vec::len), Some(RULES.len()));
    }

    #[test]
    fn credential_shapes_are_redacted() {
        let github = format!("ghp_{}", "a".repeat(36));
        let cases = [
            (
                "Authorization: Bearer abc123.def456ghi",
                "Authorization: Bearer [REDACTED]",
            ),
            (
                "https://user:hunterpassword@example.invalid/path",
                "https://[REDACTED]@example.invalid/path",
            ),
            ("token=sekret-value-1234567890", "token=[REDACTED]"),
            ("PASSWORD: hunter2value", "PASSWORD: [REDACTED]"),
            (github.as_str(), "[REDACTED]"),
            ("AKIAABCDEFGHIJKLMNOP", "[REDACTED]"),
            (
                "https://example.invalid/x?sig=abcDEF123%3D&other=1",
                "sig=[REDACTED]&other=1",
            ),
            ("X-Api-Key: abcdef0123", "X-Api-Key: [REDACTED]"),
            (
                "eyJhbGciOiJIUzI1.eyJzdWIiOiIxMjM0.SflKxwRJSMeKKF2QT4",
                "[REDACTED]",
            ),
            ("secret_key = shh-1234", "secret_key = [REDACTED]"),
            ("credentials: abc", "credentials: [REDACTED]"),
        ];
        for (original, expected) in cases {
            let redacted = redact(original);
            assert!(
                redacted.contains(expected),
                "{original:?} became {redacted:?}"
            );
        }
    }

    #[test]
    fn ordinary_cargo_output_is_unchanged() {
        let ordinary = "cargo build --release --manifest-path controller/Cargo.toml\n   \
                        Compiling engine v0.1.0\nwarning: unused variable: `token_count`";
        assert_eq!(redact(ordinary), ordinary);
    }

    #[test]
    fn bounded_keeps_only_the_last_lines() {
        let text = (0..251)
            .map(|line| format!("line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let tail = bounded(&text, TAIL_LINES);
        assert_eq!(tail.matches('\n').count(), 199);
        assert!(tail.starts_with("line 51\n"));
        assert!(tail.ends_with("line 250"));
        let short = "only\ntwo lines";
        assert_eq!(bounded(short, TAIL_LINES), short);
    }

    #[test]
    fn diagnostics_are_redacted_then_bounded() {
        let mut lines: Vec<String> = (0..250).map(|line| format!("line {line}")).collect();
        lines.push("Authorization: Bearer sekret-token-value-should-not-leak".into());
        let message = diagnostic(&lines.join("\n"));
        assert!(!message.contains("sekret-token-value-should-not-leak"));
        assert!(message.ends_with("Authorization: Bearer [REDACTED]"));
        assert!(!message.contains("line 50\n"));
        assert!(message.contains("line 249"));
        assert_eq!(message.matches('\n').count(), 199);
    }
}
