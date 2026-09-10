#![cfg(feature = "sqlite-backend")]

use coordination::*;
use deploy::{ClaimState, ProtectedClaim, ResourceIdentity};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const PRIVATE_VALUE: &str = "private-value-canary";
const PRIVATE_STATE: &str = "private-state-canary";
const PRIVATE_TOKEN: &str = "private-token-canary";
const PRIVATE_PATH: &str = "private-path-canary/coordination.db";

fn scope() -> ClaimScope {
    ClaimScope {
        registry_uid: REGISTRY_UID.to_owned(),
        cluster_binding: "cluster-a".to_owned(),
        control_namespace: "control".to_owned(),
        workload_namespace: "workload".to_owned(),
    }
}

fn claim(name: String, sequence: u64) -> TestResult<ProtectedClaim> {
    let names = deploy::allocation_names(REGISTRY_UID, sequence)?;
    Ok(ProtectedClaim {
        identity: ResourceIdentity {
            api_group: String::new(),
            resource: "configmaps".to_owned(),
            namespace: "workload".to_owned(),
            name,
        },
        domain_id: "controller".to_owned(),
        generation_id: names.generation_id,
        transition_id: names.transition_id,
        state: ClaimState::Active,
        uid: Some(format!("uid-{sequence}")),
    })
}

fn snapshot(count: usize) -> TestResult<ClaimSnapshotV1> {
    let claims = (0..count)
        .map(|index| claim(format!("resource-{index:06}"), 1))
        .collect::<TestResult<Vec<_>>>()?;
    Ok(ClaimSnapshotV1 {
        schema_version: CLAIM_SNAPSHOT_SCHEMA.to_owned(),
        scope: scope(),
        claims,
    })
}

fn missing_reference() -> ClaimRef {
    ClaimRef {
        scope: scope(),
        digest: format!("sha256:{}", "f".repeat(64)),
    }
}

fn assert_private_diagnostic(error: &coordination::Error) {
    let diagnostic = error.to_string();
    assert!(!diagnostic.contains(PRIVATE_VALUE));
    assert!(!diagnostic.contains(PRIVATE_STATE));
    assert!(!diagnostic.contains(PRIVATE_TOKEN));
    assert!(!diagnostic.contains(PRIVATE_PATH));
}

#[tokio::test]
async fn large_snapshot_survives_local_reopen_without_a_mutable_head() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let snapshot = snapshot(12_000)?;
    assert!(serde_json::to_vec(&snapshot)?.len() > 1024 * 1024);
    assert!(serde_json::to_vec(&snapshot)?.len() < MAX_CLAIM_SNAPSHOT_BYTES);

    let reference = {
        let store = SqliteCoordination::open_local(&path).await?;
        store.put_claims(&snapshot).await?
    };
    let reopened = SqliteCoordination::open_local(&path).await?;
    assert_eq!(reopened.read_claims(&reference).await?, Some(snapshot));
    assert_eq!(reopened.read_claims(&missing_reference()).await?, None);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn identical_repeated_and_concurrent_writes_are_idempotent() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let left = SqliteCoordination::open_local(&path).await?;
    let right = SqliteCoordination::open_local(&path).await?;
    let snapshot = snapshot(2)?;
    let expected = snapshot.reference()?;

    let (left_result, right_result) =
        tokio::join!(left.put_claims(&snapshot), right.put_claims(&snapshot));
    assert_eq!(left_result?, expected);
    assert_eq!(right_result?, expected);
    assert_eq!(left.put_claims(&snapshot).await?, expected);
    assert_eq!(right.read_claims(&expected).await?, Some(snapshot));
    Ok(())
}

#[tokio::test]
async fn references_are_scope_bound_and_domain_separated() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let store = SqliteCoordination::open_local(&path).await?;
    let snapshot = snapshot(1)?;
    let reference = store.put_claims(&snapshot).await?;
    assert_ne!(reference.digest, compiler_core::digest(&snapshot)?);

    let mut wrong_scope = reference.clone();
    wrong_scope.scope.control_namespace = "different-control".to_owned();
    assert!(matches!(
        store.read_claims(&wrong_scope).await,
        Err(Error::Invalid(_))
    ));

    let wrong_digest = missing_reference();
    let encoded = compiler_core::canonical_bytes(&snapshot)?;
    let database = rusqlite::Connection::open(&path)?;
    database.execute(
        "INSERT INTO apsides_coordination_claims(digest,metadata) VALUES (?1,?2)",
        rusqlite::params![wrong_digest.digest.clone(), encoded],
    )?;
    assert!(matches!(
        store.read_claims(&wrong_digest).await,
        Err(Error::Invalid(_))
    ));
    Ok(())
}

#[tokio::test]
async fn missing_rows_are_distinct_from_corrupt_rows_and_diagnostics_are_private() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let store = SqliteCoordination::open_local(&path).await?;
    let missing = missing_reference();
    assert_eq!(store.read_claims(&missing).await?, None);

    let snapshot = snapshot(1)?;
    let reference = store.put_claims(&snapshot).await?;
    let canonical = String::from_utf8(compiler_core::canonical_bytes(&snapshot)?)?;
    let corrupted = canonical.replace("\"active\"", &format!("\"{PRIVATE_STATE}\""));
    assert!(corrupted.contains(PRIVATE_STATE));
    let database = rusqlite::Connection::open(&path)?;
    database.execute(
        "UPDATE apsides_coordination_claims SET metadata=?1 WHERE digest=?2",
        rusqlite::params![corrupted.into_bytes(), reference.digest.clone()],
    )?;

    let Err(error) = store.read_claims(&reference).await else {
        return Err("corrupt claim row was accepted".into());
    };
    assert_private_diagnostic(&error);
    let Err(repeated_error) = store.put_claims(&snapshot).await else {
        return Err("idempotent write replaced a corrupt claim row".into());
    };
    assert_private_diagnostic(&repeated_error);

    database.execute(
        "UPDATE apsides_coordination_claims SET metadata=?1 WHERE digest=?2",
        rusqlite::params![b"{".to_vec(), reference.digest.clone()],
    )?;
    let Err(malformed_error) = store.read_claims(&reference).await else {
        return Err("malformed claim row was accepted".into());
    };
    assert_private_diagnostic(&malformed_error);
    Ok(())
}

#[tokio::test]
async fn duplicate_and_unsorted_claims_are_rejected() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteCoordination::open_local(&directory.path().join("coordination.db")).await?;

    let mut duplicate = snapshot(2)?;
    let first = duplicate
        .claims
        .first()
        .ok_or("claim fixture unexpectedly empty")?
        .clone();
    let second = duplicate
        .claims
        .get_mut(1)
        .ok_or("second claim fixture missing")?;
    *second = first;
    assert!(matches!(
        store.put_claims(&duplicate).await,
        Err(Error::Invalid(_))
    ));

    let mut unsorted = snapshot(2)?;
    unsorted.claims.reverse();
    assert!(matches!(
        store.put_claims(&unsorted).await,
        Err(Error::Invalid(_))
    ));

    Ok(())
}

#[tokio::test]
async fn claim_count_over_the_bounded_snapshot_budget_is_rejected() -> TestResult {
    let directory = tempfile::tempdir()?;
    let store = SqliteCoordination::open_local(&directory.path().join("coordination.db")).await?;
    let repeated = claim("resource".to_owned(), 1)?;
    let oversized = ClaimSnapshotV1 {
        schema_version: CLAIM_SNAPSHOT_SCHEMA.to_owned(),
        scope: scope(),
        claims: vec![repeated; MAX_CLAIMS + 1],
    };
    assert!(matches!(
        store.put_claims(&oversized).await,
        Err(Error::Invalid(_))
    ));
    Ok(())
}

#[test]
fn typed_snapshot_rejects_unknown_private_payload_fields() -> TestResult {
    let mut value = serde_json::to_value(snapshot(1)?)?;
    let claims = value
        .as_object_mut()
        .and_then(|object| object.get_mut("claims"))
        .and_then(serde_json::Value::as_array_mut)
        .ok_or("claim JSON fixture missing")?;
    let claim = claims
        .first_mut()
        .and_then(serde_json::Value::as_object_mut)
        .ok_or("claim JSON object missing")?;
    claim.insert(
        "desired".to_owned(),
        serde_json::json!({"password":PRIVATE_VALUE}),
    );

    let error = serde_json::from_value::<ClaimSnapshotV1>(value)
        .err()
        .ok_or("unknown private payload field was accepted")?;
    assert!(!error.to_string().contains(PRIVATE_VALUE));
    Ok(())
}

#[tokio::test]
async fn a_relative_database_path_is_refused_without_echoing_it() -> TestResult {
    for read_only in [false, true] {
        let path = std::path::Path::new(PRIVATE_PATH);
        let result = if read_only {
            SqliteCoordination::open_local_read_only(path).await
        } else {
            SqliteCoordination::open_local(path).await
        };
        let Err(error) = result else {
            return Err("relative coordination path was accepted".into());
        };
        assert!(matches!(&error, Error::Invalid(_)));
        assert_private_diagnostic(&error);
        if path.exists() {
            return Err("refused configuration still touched the filesystem".into());
        }
    }
    Ok(())
}
