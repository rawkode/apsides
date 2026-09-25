#![cfg(feature = "sqlite-backend")]

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

fn review(policy_ref: &str) -> TestResult<ReviewMetadata> {
    let names = deploy::allocation_names(REGISTRY_UID, 1)?;
    Ok(ReviewMetadata {
        schema_version: REVIEW_METADATA_SCHEMA.to_owned(),
        scope: ReviewScope {
            claim_scope: claim_scope(),
            domain_id: "controller".to_owned(),
            transition_id: names.transition_id,
            sequence: 1,
        },
        surface_digest: format!("sha256:{}", "0".repeat(64)),
        resources: Vec::new(),
        inputs: Vec::new(),
        reactions: Vec::new(),
        handlers: Vec::new(),
        egress: Vec::new(),
        outputs: Vec::new(),
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
            kubernetes_version: ReviewKubernetesVersion {
                min_version: "1.34".into(),
                qualified_on: ReviewQualifiedServer::V1343,
            },
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

fn row_count(path: &std::path::Path, table: &str) -> TestResult<i64> {
    let connection = rusqlite::Connection::open(path)?;
    let query = match table {
        "apsides_coordination_review_objects" => {
            "SELECT count(*) FROM apsides_coordination_review_objects"
        }
        _ => return Err("unsupported review table fixture".into()),
    };
    let mut rows_statement = connection.prepare(query)?;
    let mut rows = rows_statement.query([])?;
    Ok(rows
        .next()?
        .ok_or("review count row missing")?
        .get::<usize, i64>(0)?)
}

#[tokio::test]
async fn exact_review_write_is_idempotent_and_survives_local_reopen() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let metadata = review("policy-a")?;
    let reference = {
        let store = SqliteCoordination::open_local(&path).await?;
        let reference = store.put_review(&metadata).await?;
        assert_eq!(store.put_review(&metadata).await?, reference);
        reference
    };

    let reopened = SqliteCoordination::open_local(&path).await?;
    assert_eq!(reopened.read_review(&reference).await?, Some(metadata));
    let mut missing = reference;
    missing.scope.domain_id = "other-domain".to_owned();
    assert_eq!(reopened.read_review(&missing).await?, None);
    assert_eq!(row_count(&path, "apsides_coordination_review_objects")?, 1);
    Ok(())
}

#[tokio::test]
async fn same_transition_scope_retains_distinct_content_addressed_reviews() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let store = SqliteCoordination::open_local(&path).await?;
    let original = review("policy-a")?;
    let original_reference = store.put_review(&original).await?;
    let subsequent = review("policy-b")?;
    let subsequent_reference = store.put_review(&subsequent).await?;
    assert_ne!(subsequent_reference, original_reference);

    assert_eq!(
        store.read_review(&original_reference).await?,
        Some(original)
    );
    assert_eq!(
        store.read_review(&subsequent_reference).await?,
        Some(subsequent)
    );
    assert_eq!(row_count(&path, "apsides_coordination_review_objects")?, 2);
    Ok(())
}

#[tokio::test]
async fn concurrent_distinct_reviews_for_one_scope_both_succeed() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let first_store = SqliteCoordination::open_local(&path).await?;
    let second_store = SqliteCoordination::open_local(&path).await?;
    let first = review("policy-a")?;
    let second = review("policy-b")?;
    let (first_reference, second_reference) = tokio::join!(
        first_store.put_review(&first),
        second_store.put_review(&second)
    );
    let first_reference = first_reference?;
    let second_reference = second_reference?;
    assert_ne!(first_reference, second_reference);
    let reopened = SqliteCoordination::open_local(&path).await?;
    assert_eq!(reopened.read_review(&first_reference).await?, Some(first));
    assert_eq!(reopened.read_review(&second_reference).await?, Some(second));
    assert_eq!(row_count(&path, "apsides_coordination_review_objects")?, 2);
    Ok(())
}

enum Corruption {
    Blob(Vec<u8>),
    Text(String),
    Sequence(String),
}

fn corrupt_review_row(path: &std::path::Path, corruption: Corruption) -> TestResult {
    let connection = rusqlite::Connection::open(path)?;
    match corruption {
        Corruption::Blob(bytes) => {
            connection.execute(
                "UPDATE apsides_coordination_review_objects SET metadata=?1",
                [bytes],
            )?;
        }
        Corruption::Text(value) => {
            connection.execute(
                "UPDATE apsides_coordination_review_objects SET metadata=?1",
                [value],
            )?;
        }
        Corruption::Sequence(value) => {
            connection.execute(
                "UPDATE apsides_coordination_review_objects SET sequence=?1",
                [value],
            )?;
        }
    }
    Ok(())
}

async fn assert_corruption_rejected(corruption: Corruption) -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let metadata = review("policy-a")?;
    let reference = {
        let store = SqliteCoordination::open_local(&path).await?;
        store.put_review(&metadata).await?
    };
    corrupt_review_row(&path, corruption)?;

    let store = SqliteCoordination::open_local(&path).await?;
    let Err(read_error) = store.read_review(&reference).await else {
        return Err("corrupt review row was accepted".into());
    };
    private(&read_error);
    let Err(write_error) = store.put_review(&metadata).await else {
        return Err("immutable review publication repaired a corrupt row".into());
    };
    private(&write_error);
    assert_eq!(row_count(&path, "apsides_coordination_review_objects")?, 1);
    Ok(())
}

#[tokio::test]
async fn corrupt_exact_review_blob_type_size_and_sequence_are_static_errors() -> TestResult {
    for corruption in [
        Corruption::Blob(format!("{{\"private\":\"{PRIVATE_CANARY}\"}}").into_bytes()),
        Corruption::Text(PRIVATE_CANARY.to_owned()),
        Corruption::Blob(vec![b'x'; MAX_REVIEW_METADATA_BYTES + 1]),
        Corruption::Sequence("2".to_owned()),
        Corruption::Sequence("9".repeat(21)),
    ] {
        assert_corruption_rejected(corruption).await?;
    }
    Ok(())
}

#[tokio::test]
async fn changed_digest_is_a_missing_exact_key_and_does_not_block_republication() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let metadata = review("policy-a")?;
    let store = SqliteCoordination::open_local(&path).await?;
    let reference = store.put_review(&metadata).await?;
    let changed_digest = format!("sha256:{}", "f".repeat(64));
    let database = rusqlite::Connection::open(&path)?;
    database.execute(
        "UPDATE apsides_coordination_review_objects SET digest=?1 WHERE digest=?2",
        rusqlite::params![changed_digest.clone(), reference.digest.clone()],
    )?;

    assert_eq!(store.read_review(&reference).await?, None);
    let mut changed_reference = reference.clone();
    changed_reference.digest = changed_digest;
    assert!(matches!(
        store.read_review(&changed_reference).await,
        Err(Error::Invalid("review metadata integrity"))
    ));
    assert_eq!(store.put_review(&metadata).await?, reference);
    assert_eq!(store.read_review(&reference).await?, Some(metadata));
    assert_eq!(row_count(&path, "apsides_coordination_review_objects")?, 2);
    Ok(())
}

#[tokio::test]
async fn scope_and_digest_substitution_are_rejected() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("coordination.db");
    let metadata = review("policy-a")?;
    let store = SqliteCoordination::open_local(&path).await?;
    let reference = store.put_review(&metadata).await?;

    let mut forged_digest = reference.clone();
    forged_digest.digest = format!("sha256:{}", "f".repeat(64));
    assert_eq!(store.read_review(&forged_digest).await?, None);

    let mut forged_scope = reference.clone();
    forged_scope.scope.domain_id = "other-domain".to_owned();
    let connection = rusqlite::Connection::open(&path)?;
    connection.execute(
        "INSERT INTO apsides_coordination_review_objects(\
             registry_uid,cluster_binding,control_namespace,workload_namespace,\
             domain_id,transition_id,sequence,digest,metadata) \
             SELECT registry_uid,cluster_binding,control_namespace,workload_namespace,\
             ?1,transition_id,sequence,digest,metadata FROM apsides_coordination_review_objects \
             WHERE domain_id=?2",
        rusqlite::params![
            forged_scope.scope.domain_id.clone(),
            reference.scope.domain_id.clone()
        ],
    )?;
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
        SqliteCoordination::open_local_read_only(&missing_path)
            .await
            .is_err()
    );
    assert!(!missing_path.exists());

    let path = directory.path().join("coordination.db");
    let metadata = review("policy-a")?;
    let reference = {
        let writable = SqliteCoordination::open_local(&path).await?;
        writable.put_review(&metadata).await?
    };
    let read_only = SqliteCoordination::open_local_read_only(&path).await?;
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
    assert_eq!(row_count(&path, "apsides_coordination_review_objects")?, 1);
    Ok(())
}

#[tokio::test]
async fn read_only_database_without_review_table_is_rejected_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("missing-review-table.db");
    let database = rusqlite::Connection::open(&path)?;
    database

        .execute_batch(
            "CREATE TABLE apsides_coordination_schema (id INTEGER PRIMARY KEY, version INTEGER NOT NULL);\
             INSERT INTO apsides_coordination_schema VALUES (1,1);\
             CREATE TABLE apsides_coordination_claims (digest TEXT PRIMARY KEY, metadata BLOB NOT NULL);\
             CREATE TABLE apsides_coordination_audits (digest TEXT PRIMARY KEY, metadata BLOB NOT NULL);",
        )?;

    let read_only = SqliteCoordination::open_local_read_only(&path).await?;
    let reference = review("policy-a")?.reference()?;
    assert!(matches!(
        read_only.read_review(&reference).await,
        Err(Error::Invalid("missing immutable review table"))
    ));
    assert!(matches!(
        read_only.put_review(&review("policy-a")?).await,
        Err(Error::Invalid("read-only coordination store"))
    ));
    let count = database.query_one(
        "SELECT count(*) FROM sqlite_master WHERE type='table' \
         AND name='apsides_coordination_review_objects'",
        [],
        |row| row.get::<usize, i64>(0),
    )?;
    assert_eq!(count, 0);
    Ok(())
}

#[tokio::test]
async fn writable_open_rejects_malformed_review_tables_without_repair() -> TestResult {
    let directory = tempfile::tempdir()?;
    let cases = [
        (
            "missing-column.db",
            "CREATE TABLE apsides_coordination_review_objects (\
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
            "CREATE TABLE apsides_coordination_review_objects (\
             registry_uid TEXT NOT NULL,cluster_binding TEXT NOT NULL,\
             control_namespace TEXT NOT NULL,workload_namespace TEXT NOT NULL,\
             domain_id TEXT NOT NULL,transition_id TEXT NOT NULL,\
             sequence TEXT NOT NULL,digest TEXT NOT NULL,metadata BLOB NOT NULL);",
            9_i64,
        ),
    ];
    for (name, schema, expected_columns) in cases {
        let path = directory.path().join(name);
        let database = rusqlite::Connection::open(&path)?;
        database.execute_batch(schema)?;
        assert!(matches!(
            SqliteCoordination::open_local(&path).await,
            Err(Error::Invalid("review table schema"))
        ));
        let actual_columns = database.query_one(
            "SELECT count(*) FROM pragma_table_info('apsides_coordination_review_objects')",
            [],
            |row| row.get::<usize, i64>(0),
        )?;
        assert_eq!(actual_columns, expected_columns);
    }
    Ok(())
}
