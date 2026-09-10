#![cfg(feature = "sqlite-backend")]
//! The store is local by construction; configuration is checked before any I/O.
use inventory::{Error, SqliteInventory};
use std::path::Path;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
const PRIVATE: &str = "private-configuration-canary";

async fn rejected(path: &Path, expected: &'static str) -> TestResult {
    for read_only in [false, true] {
        let result = if read_only {
            SqliteInventory::open_local_read_only(path).await
        } else {
            SqliteInventory::open_local(path).await
        };
        let Err(error) = result else {
            return Err("invalid inventory configuration was accepted".into());
        };
        if !matches!(error, Error::Invalid(actual) if actual == expected) {
            return Err("inventory configuration returned a different error".into());
        }
        if error.to_string().contains(PRIVATE) {
            return Err("inventory diagnostic disclosed configuration content".into());
        }
    }
    Ok(())
}

#[tokio::test]
async fn a_relative_database_path_is_refused_before_any_filesystem_access() -> TestResult {
    for relative in [
        format!("{PRIVATE}.db"),
        format!("./{PRIVATE}/inventory.db"),
        format!("../{PRIVATE}/inventory.db"),
    ] {
        let path = Path::new(&relative);
        rejected(path, "database path must be absolute").await?;
        if path.exists() {
            return Err("refused configuration still touched the filesystem".into());
        }
    }
    Ok(())
}

#[tokio::test]
async fn read_only_open_of_an_absent_absolute_path_never_creates_it() -> TestResult {
    let directory = tempfile::tempdir()?;
    let absent = directory.path().join("absent.db");
    let Err(error) = SqliteInventory::open_local_read_only(&absent).await else {
        return Err("read-only open of an absent database was accepted".into());
    };
    if !matches!(error, Error::Backend("open local read-only database")) {
        return Err("absent read-only database returned a different error".into());
    }
    if absent.exists() {
        return Err("read-only open created the database file".into());
    }
    Ok(())
}
