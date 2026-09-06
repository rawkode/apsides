#![cfg(feature = "libsql-backend")]

use coordination::*;

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;

const REGISTRY_UID: &str = "11111111-1111-4111-8111-111111111111";
const PRIVATE_CANARY: &str = "private-review-storage-canary";

fn claim_scope() -> ClaimScope {
    ClaimScope {
        registry_uid: REGISTRY_UID.to_owned(),
        cluster_binding: "cluster-a".to_owned(),
        control_namespace: "control".to_owned(),
        workload_namespace: "workload".to_owned(),
    }
}

fn permission_ceiling(policy_ref: &str) -> ReviewCeiling {
    ReviewCeiling {
        policy_ref: policy_ref.to_owned(),
        domain_id: "controller".to_owned(),
        cluster_binding: "cluster-a".to_owned(),
        workload_namespace: "workload".to_owned(),
        control_namespace: "control".to_owned(),
        capabilities: Vec::new(),
        accepted_enforcement_gaps: Vec::new(),
        accepted_indirect_risks: Vec::new(),
    }
}

fn review(policy_ref: &str) -> TestResult<ReviewMetadataV1> {
    let names = deploy::allocation_names(REGISTRY_UID, 1)?;
    Ok(ReviewMetadataV1 {
        schema_version: REVIEW_METADATA_SCHEMA.to_owned(),
        scope: ReviewScope {
            claim_scope: claim_scope(),
            domain_id: "controller".to_owned(),
            transition_id: names.transition_id,
            sequence: 1,
        },
        resources: Vec::new(),
        inputs: Vec::new(),
        reactions: Vec::new(),
        runtime_permissions: Vec::new(),
        cleanup_permissions: Vec::new(),
        deployment_permissions: ReviewDeploymentPermissions {
            required_resources: Vec::new(),
            creation_targets: Vec::new(),
            role_delegation: Vec::new(),
            scope: ReviewDeploymentScope::IssuedDeployerOnly,
            authorization: ReviewAuthorization::RequiredAccessOnly,
            limitations: vec![
                ReviewDeploymentLimitation::CreateNamesRequireAdmission,
                ReviewDeploymentLimitation::GeneratedNamesRequireBroadAccess,
                ReviewDeploymentLimitation::InvokingIdentityBootstrapCredentialsAndDiscoveryExcluded,
                ReviewDeploymentLimitation::DatabaseCredentialsSeparate,
            ],
        },
        compatibility: ReviewCompatibility {
            runtime_budgets: ReviewBudgets {
                max_resources: 1,
                max_inputs: 1,
                max_reactions: 1,
                max_object_bytes: 1024,
                max_queue_depth: 1,
                max_concurrent_requests: 1,
            },
            schema_library: ReviewSchemaLibrary::K8sOpenapi0280,
            schema_kubernetes_minor: ReviewSchemaMinor::V136,
            required_runtime_apis: Vec::new(),
            required_cleanup_apis: Vec::new(),
            required_deployment_apis: Vec::new(),
            deployment_api_scope: ReviewDeploymentScope::IssuedDeployerOnly,
            qualified_create_precondition_servers: vec![ReviewQualifiedServer::V1343],
            field_validation: ReviewFieldValidation::Strict,
            transition_plan_schema: ReviewTransitionSchema::V4,
            runtime_replicas: 1,
        },
        enforcement: ReviewEnforcement {
            profile: ReviewEnforcementProfile::ApiBound,
            gaps: Vec::new(),
            indirect_risks: Vec::new(),
            runtime_ceiling: permission_ceiling(policy_ref),
            cleanup_ceiling: permission_ceiling(policy_ref),
            decision: ReviewCeilingDecision::ApprovedByPlanner,
        },
    })
}

fn private(error: &coordination::Error) {
    assert!(!error.to_string().contains(PRIVATE_CANARY));
}

async fn row_count(path: &std::path::Path) -> TestResult<i64> {
    let database = libsql::Builder::new_local(path).build().await?;
    let connection = database.connect()?;
    let mut rows = connection
        .query("SELECT count(*) FROM reakt_coordination_reviews", ())
        .await?;
    Ok(rows
        .next()
        .await?
        .ok_or("review count row missing")?
        .get::<i64>(0)?)
}

#[tokio::test]
async fn exact_review_write_is_idempotent_and_survives_local_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let metadata = review("policy-a")?;
    let reference = {
        let store = LibsqlCoordination::open_local(&path).await?;
        let reference = store.put_review(&metadata).await?;
        assert_eq!(store.put_review(&metadata).await?, reference);
        reference
    };

    let reopened = LibsqlCoordination::open_local(&path).await?;
    assert_eq!(reopened.read_review(&reference).await?, Some(metadata));
    let mut missing = reference;
    missing.scope.domain_id = "other-domain".to_owned();
    assert_eq!(reopened.read_review(&missing).await?, None);
    assert_eq!(row_count(&path).await?, 1);
    Ok(())
}

#[tokio::test]
async fn same_transition_slot_rejects_a_different_valid_review() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let store = LibsqlCoordination::open_local(&path).await?;
    let original = review("policy-a")?;
    let original_reference = store.put_review(&original).await?;
    let conflicting = review("policy-b")?;
    conflicting.validate()?;
    assert_ne!(conflicting.reference()?, original_reference);

    assert!(matches!(
        store.put_review(&conflicting).await,
        Err(Error::Invalid("immutable review mismatch"))
    ));
    assert_eq!(
        store.read_review(&original_reference).await?,
        Some(original)
    );
    assert_eq!(row_count(&path).await?, 1);
    Ok(())
}

#[tokio::test]
async fn concurrent_conflicting_review_writes_have_one_winner() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let first_store = LibsqlCoordination::open_local(&path).await?;
    let second_store = LibsqlCoordination::open_local(&path).await?;
    let first = review("policy-a")?;
    let second = review("policy-b")?;
    let (first_result, second_result) = tokio::join!(
        first_store.put_review(&first),
        second_store.put_review(&second)
    );

    let (winner, winner_reference, loser_reference) = match (first_result, second_result) {
        (Ok(reference), Err(Error::Invalid("immutable review mismatch"))) => {
            (first, reference, second.reference()?)
        }
        (Err(Error::Invalid("immutable review mismatch")), Ok(reference)) => {
            (second, reference, first.reference()?)
        }
        _ => return Err("conflicting review writers did not produce exactly one winner".into()),
    };
    let reopened = LibsqlCoordination::open_local(&path).await?;
    assert_eq!(reopened.read_review(&winner_reference).await?, Some(winner));
    assert!(matches!(
        reopened.read_review(&loser_reference).await,
        Err(Error::Invalid("review reference mismatch"))
    ));
    assert_eq!(row_count(&path).await?, 1);
    Ok(())
}

enum Corruption {
    Blob(Vec<u8>),
    Text(String),
    Digest(String),
    Sequence(String),
}

async fn corrupt_review_row(path: &std::path::Path, corruption: Corruption) -> TestResult {
    let database = libsql::Builder::new_local(path).build().await?;
    let connection = database.connect()?;
    match corruption {
        Corruption::Blob(bytes) => {
            connection
                .execute("UPDATE reakt_coordination_reviews SET metadata=?1", [bytes])
                .await?;
        }
        Corruption::Text(value) => {
            connection
                .execute("UPDATE reakt_coordination_reviews SET metadata=?1", [value])
                .await?;
        }
        Corruption::Digest(value) => {
            connection
                .execute("UPDATE reakt_coordination_reviews SET digest=?1", [value])
                .await?;
        }
        Corruption::Sequence(value) => {
            connection
                .execute("UPDATE reakt_coordination_reviews SET sequence=?1", [value])
                .await?;
        }
    }
    Ok(())
}

async fn assert_corruption_rejected(corruption: Corruption) -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let metadata = review("policy-a")?;
    let reference = {
        let store = LibsqlCoordination::open_local(&path).await?;
        store.put_review(&metadata).await?
    };
    corrupt_review_row(&path, corruption).await?;

    let store = LibsqlCoordination::open_local(&path).await?;
    let Err(read_error) = store.read_review(&reference).await else {
        return Err("corrupt review row was accepted".into());
    };
    private(&read_error);
    let Err(write_error) = store.put_review(&metadata).await else {
        return Err("immutable review publication repaired a corrupt row".into());
    };
    private(&write_error);
    assert_eq!(row_count(&path).await?, 1);
    Ok(())
}

#[tokio::test]
async fn corrupt_review_blob_type_size_digest_and_sequence_are_static_errors() -> TestResult {
    for corruption in [
        Corruption::Blob(format!("{{\"private\":\"{PRIVATE_CANARY}\"}}").into_bytes()),
        Corruption::Text(PRIVATE_CANARY.to_owned()),
        Corruption::Blob(vec![b'x'; MAX_REVIEW_METADATA_BYTES + 1]),
        Corruption::Digest(format!("sha256:{}", "f".repeat(64))),
        Corruption::Digest(format!("sha256:{}", "f".repeat(65))),
        Corruption::Sequence("2".to_owned()),
        Corruption::Sequence("9".repeat(21)),
    ] {
        assert_corruption_rejected(corruption).await?;
    }
    Ok(())
}

#[tokio::test]
async fn scope_and_digest_substitution_are_rejected() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let metadata = review("policy-a")?;
    let store = LibsqlCoordination::open_local(&path).await?;
    let reference = store.put_review(&metadata).await?;

    let mut forged_digest = reference.clone();
    forged_digest.digest = format!("sha256:{}", "f".repeat(64));
    assert!(matches!(
        store.read_review(&forged_digest).await,
        Err(Error::Invalid("review reference mismatch"))
    ));

    let mut forged_scope = reference.clone();
    forged_scope.scope.domain_id = "other-domain".to_owned();
    let database = libsql::Builder::new_local(&path).build().await?;
    let connection = database.connect()?;
    connection
        .execute(
            "INSERT INTO reakt_coordination_reviews(\
             registry_uid,cluster_binding,control_namespace,workload_namespace,\
             domain_id,transition_id,sequence,digest,metadata) \
             SELECT registry_uid,cluster_binding,control_namespace,workload_namespace,\
             ?1,transition_id,sequence,digest,metadata FROM reakt_coordination_reviews \
             WHERE domain_id=?2",
            libsql::params![
                forged_scope.scope.domain_id.clone(),
                reference.scope.domain_id.clone()
            ],
        )
        .await?;
    let Err(error) = store.read_review(&forged_scope).await else {
        return Err("review body was accepted under a forged scope".into());
    };
    private(&error);
    Ok(())
}

#[tokio::test]
async fn local_read_only_store_reads_without_creating_or_mutating_reviews() -> TestResult {
    let directory = tempfile::tempdir()?;
    let missing_path = directory.path().join("missing.db");
    assert!(
        LibsqlCoordination::open_local_read_only(&missing_path)
            .await
            .is_err()
    );
    assert!(!missing_path.exists());

    let path = directory.path().join("coordination.db");
    let metadata = review("policy-a")?;
    let reference = {
        let writable = LibsqlCoordination::open_local(&path).await?;
        writable.put_review(&metadata).await?
    };
    let read_only = LibsqlCoordination::open_local_read_only(&path).await?;
    assert_eq!(
        read_only.read_review(&reference).await?,
        Some(metadata.clone())
    );
    let mut invalid = metadata;
    invalid.schema_version = PRIVATE_CANARY.to_owned();
    assert!(matches!(
        read_only.put_review(&invalid).await,
        Err(Error::Invalid("read-only coordination store"))
    ));
    assert_eq!(row_count(&path).await?, 1);
    Ok(())
}

#[tokio::test]
async fn read_only_old_schema_reports_a_missing_review_table_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("old-coordination.db");
    let database = libsql::Builder::new_local(&path).build().await?;
    database
        .connect()?
        .execute_batch(
            "CREATE TABLE reakt_coordination_schema (id INTEGER PRIMARY KEY, version INTEGER NOT NULL);\
             INSERT INTO reakt_coordination_schema VALUES (1,1);\
             CREATE TABLE reakt_coordination_claims (digest TEXT PRIMARY KEY, metadata BLOB NOT NULL);\
             CREATE TABLE reakt_coordination_audits (digest TEXT PRIMARY KEY, metadata BLOB NOT NULL);",
        )
        .await?;

    let read_only = LibsqlCoordination::open_local_read_only(&path).await?;
    let reference = review("policy-a")?.reference()?;
    assert!(matches!(
        read_only.read_review(&reference).await,
        Err(Error::Backend("read immutable review"))
    ));
    assert!(matches!(
        read_only.put_review(&review("policy-a")?).await,
        Err(Error::Invalid("read-only coordination store"))
    ));
    let mut rows = database
        .connect()?
        .query(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name='reakt_coordination_reviews'",
            (),
        )
        .await?;
    let count = rows
        .next()
        .await?
        .ok_or("sqlite_master count row missing")?
        .get::<i64>(0)?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn writable_open_rejects_malformed_review_tables_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let cases = [
        (
            "missing-column.db",
            "CREATE TABLE reakt_coordination_reviews (\
             registry_uid TEXT NOT NULL,cluster_binding TEXT NOT NULL,\
             control_namespace TEXT NOT NULL,workload_namespace TEXT NOT NULL,\
             domain_id TEXT NOT NULL,transition_id TEXT NOT NULL,\
             sequence TEXT NOT NULL,digest TEXT NOT NULL,\
             PRIMARY KEY (registry_uid,cluster_binding,control_namespace,\
             workload_namespace,domain_id,transition_id));",
            8_i64,
        ),
        (
            "missing-primary-key.db",
            "CREATE TABLE reakt_coordination_reviews (\
             registry_uid TEXT NOT NULL,cluster_binding TEXT NOT NULL,\
             control_namespace TEXT NOT NULL,workload_namespace TEXT NOT NULL,\
             domain_id TEXT NOT NULL,transition_id TEXT NOT NULL,\
             sequence TEXT NOT NULL,digest TEXT NOT NULL,metadata BLOB NOT NULL);",
            9_i64,
        ),
    ];
    for (name, schema, expected_columns) in cases {
        let path = directory.path().join(name);
        let database = libsql::Builder::new_local(&path).build().await?;
        database.connect()?.execute_batch(schema).await?;
        assert!(matches!(
            LibsqlCoordination::open_local(&path).await,
            Err(Error::Invalid("review table schema"))
        ));
        let mut rows = database
            .connect()?
            .query(
                "SELECT count(*) FROM pragma_table_info('reakt_coordination_reviews')",
                (),
            )
            .await?;
        let actual_columns = rows
            .next()
            .await?
            .ok_or("review table column count row missing")?
            .get::<i64>(0)?;
        assert_eq!(actual_columns, expected_columns);
    }
    Ok(())
}
