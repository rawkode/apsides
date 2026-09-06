#![cfg(feature = "libsql-backend")]
use inventory::{Error, LibsqlInventory};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const PRIVATE: &str = "private-remote-canary";

async fn rejected(url: String, token: String, expected: &'static str) -> TestResult {
    for read_only in [false, true] {
        let result = if read_only {
            LibsqlInventory::open_remote_read_only(url.clone(), token.clone()).await
        } else {
            LibsqlInventory::open_remote(url.clone(), token.clone()).await
        };
        let Err(error) = result else {
            return Err("invalid remote inventory configuration was accepted".into());
        };
        if !matches!(error, Error::Invalid(actual) if actual == expected) {
            return Err("remote inventory configuration returned a different error".into());
        }
        let diagnostic = error.to_string();
        if diagnostic.contains(PRIVATE) {
            return Err("remote inventory diagnostic disclosed configuration content".into());
        }
    }
    Ok(())
}

#[tokio::test]
async fn remote_endpoint_rejects_every_noncanonical_authority_shape_before_connecting() -> TestResult
{
    let invalid = [
        (format!("{PRIVATE} not a URL"), "remote endpoint"),
        (
            "http://example.invalid".to_owned(),
            "remote endpoint must use TLS without embedded credentials, query or path",
        ),
        ("https:///".to_owned(), "remote endpoint"),
        (
            format!("https://{PRIVATE}@example.invalid"),
            "remote endpoint must use TLS without embedded credentials, query or path",
        ),
        (
            format!("https://user:{PRIVATE}@example.invalid"),
            "remote endpoint must use TLS without embedded credentials, query or path",
        ),
        (
            format!("https://example.invalid/?value={PRIVATE}"),
            "remote endpoint must use TLS without embedded credentials, query or path",
        ),
        (
            format!("https://example.invalid/#{PRIVATE}"),
            "remote endpoint must use TLS without embedded credentials, query or path",
        ),
        (
            format!("libsql://example.invalid/{PRIVATE}"),
            "remote endpoint must use TLS without embedded credentials, query or path",
        ),
        (
            format!("https://example.invalid/\n{PRIVATE}"),
            "remote endpoint",
        ),
        (format!("https://{}", "a".repeat(2049)), "remote endpoint"),
    ];
    for (url, expected) in invalid {
        rejected(url, "valid-token".to_owned(), expected).await?;
    }
    Ok(())
}

#[tokio::test]
async fn remote_token_is_bounded_and_control_free_for_both_supported_schemes() -> TestResult {
    for endpoint in ["https://example.invalid", "libsql://example.invalid/"] {
        rejected(
            endpoint.to_owned(),
            String::new(),
            "remote authentication token",
        )
        .await?;
        rejected(
            endpoint.to_owned(),
            format!("{PRIVATE}\n"),
            "remote authentication token",
        )
        .await?;
        rejected(
            endpoint.to_owned(),
            "a".repeat(8193),
            "remote authentication token",
        )
        .await?;
    }
    Ok(())
}
