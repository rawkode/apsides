use super::*;
use coordination::{AuditRef, AuditStore, TransitionAuditV1};
use deploy::{Event, Phase, Quiescence, TerminationEvidence};
use std::sync::atomic::{AtomicU8, Ordering};

type TestResult<T = ()> = std::result::Result<T, Box<dyn std::error::Error>>;

const PRIVATE_STAGE_KEY: &str = "private-stage-key-canary";

fn prepare_cleaning_upgrade() -> TestResult<LifecycleRegistry> {
    let mut registry = qualified_upgrade_fixture()?;
    let plan = registry.plan(TRANSITION_B)?.clone();
    let account = plan.generation_reservation.service_account_name.clone();
    {
        let stored = registry
            .transitions
            .get_mut(TRANSITION_B)
            .ok_or("upgrade transition missing")?;
        let binding = stored
            .runtime_binding
            .as_mut()
            .ok_or("upgrade runtime binding missing")?;
        binding
            .generation_id
            .clone_from(&plan.request.next.generation_id);
        binding.service_account_name.clone_from(&account);
        stored.staged_identity_uids.clear();
        stored.staged_content_digests.clear();
        stored.staged_identity_uids.extend([
            (
                format!("apps/v1/Deployment/control/{account}"),
                "controller-b-uid".to_owned(),
            ),
            (
                "v1/ConfigMap/control/reakt-activation-example".to_owned(),
                "activation-uid".to_owned(),
            ),
            (
                format!("v1/ServiceAccount/control/{account}"),
                "credential-b-uid".to_owned(),
            ),
        ]);
        for capability in &plan.request.next.compiled.capabilities {
            stored.staged_identity_uids.insert(
                format!(
                    "rbac.authorization.k8s.io/v1/RoleBinding/{}/{}",
                    capability.namespace, account
                ),
                format!("{}-binding-b-uid", capability.namespace),
            );
        }
    }
    qualified_writer::qualify(&mut registry, TRANSITION_B)?;
    for claim in &mut registry.claims {
        claim
            .generation_id
            .clone_from(&plan.request.next.generation_id);
        claim.transition_id.clone_from(&plan.request.transition_id);
        claim.state = ClaimState::Active;
    }
    let stored = registry
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("upgrade transition missing")?;
    for event in [
        Event::Quiesced {
            event_id: "quiesced-b".to_owned(),
            evidence: Quiescence::OldGeneration(TerminationEvidence {
                generation_id: GENERATION_A.to_owned(),
                executor_id: "executor-b".to_owned(),
                verification_epoch: 0,
                environment_qualification_digest: digest(&"qualified environment")?,
                process_termination_receipt_digest: digest(&"terminated process")?,
                access_revocation_receipt_digest: digest(&"revoked access")?,
            }),
        },
        Event::Activated {
            event_id: "activated-b".to_owned(),
            generation_id: GENERATION_B.to_owned(),
            activation_receipt_digest: digest(&"activated successor")?,
            verification_epoch: 0,
        },
        Event::Cleaning {
            event_id: "cleaning-b".to_owned(),
        },
    ] {
        stored.record = deploy::advance(
            &plan,
            &stored.record,
            stored.record.revision,
            "executor-b",
            event,
        )?;
    }
    if stored.record.phase != Phase::Cleaning || !plan.cleanup.is_empty() {
        return Err("upgrade fixture did not reach empty-cleanup Cleaning phase".into());
    }
    Ok(registry)
}

fn prepare_cleaning_initial() -> TestResult<LifecycleRegistry> {
    let (mut registry, _, _, _, _) = staged_initial_fixture()?;
    initial_support_fixture(&mut registry)?;
    qualified_writer::qualify(&mut registry, TRANSITION_A)?;
    for claim in &mut registry.claims {
        claim.state = ClaimState::Active;
    }
    let plan = registry.plan(TRANSITION_A)?.clone();
    let stored = registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("initial transition missing")?;
    for event in [
        Event::Quiesced {
            event_id: "initial-quiesced".to_owned(),
            evidence: Quiescence::InitialInstallation {
                verification_epoch: 0,
            },
        },
        Event::Activated {
            event_id: "initial-activated".to_owned(),
            generation_id: GENERATION_A.to_owned(),
            activation_receipt_digest: digest(&"activated initial")?,
            verification_epoch: 0,
        },
        Event::Cleaning {
            event_id: "initial-cleaning-audit".to_owned(),
        },
    ] {
        stored.record = deploy::advance(
            &plan,
            &stored.record,
            stored.record.revision,
            "executor-a",
            event,
        )?;
    }
    Ok(registry)
}

fn committed_event(id: &str) -> TestResult<Event> {
    Ok(Event::Committed {
        event_id: id.to_owned(),
        cleanup_receipts: Vec::new(),
        obsolete_identity_retirement_digest: digest(&"retirement complete")?,
    })
}

fn bind_audit_store(
    registry: &LifecycleRegistry,
    client: Client,
    transition: &str,
    audit_store: Arc<dyn AuditStore>,
) -> TestResult<PlatformClient> {
    Ok(registry
        .bind(client, "control", "workload", "cluster-a", transition)?
        .with_audit_store(audit_store))
}

#[tokio::test]
async fn successor_commit_archives_reads_and_prunes_only_its_predecessor() -> TestResult {
    let mut registry = prepare_cleaning_upgrade()?;
    registry.histories.insert(
        "other".to_owned(),
        deploy::GenerationHistory {
            domain_id: "other".to_owned(),
            cluster_binding: "cluster-a".to_owned(),
            revision: "42".to_owned(),
            active_allocation: None,
        },
    );
    let private_digest = digest(&PRIVATE_STAGE_KEY)?;
    registry
        .transitions
        .get_mut(TRANSITION_A)
        .ok_or("predecessor transition missing")?
        .staged_content_digests
        .insert(PRIVATE_STAGE_KEY.to_owned(), private_digest.clone());
    let previous = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.to_owned(),
        resource_version: "42".to_owned(),
    };
    let audit_store = Arc::new(audit_support::MemoryAuditStore::default());
    let policy = policy(&registry.plan(TRANSITION_B)?.request.next.compiled);
    let (client, calls) = wire(vec![
        object("reakt-policy-example", &policy, "policy-uid", "40"),
        registry_object(&registry, REGISTRY_UID, "42")?,
        json!("echo-put"),
    ]);
    let platform = bind_audit_store(&registry, client, TRANSITION_B, audit_store.clone())?;
    let committed = platform
        .transition_event(
            &previous,
            TRANSITION_B,
            "executor-b",
            committed_event("committed-b")?,
        )
        .await?;

    assert!(!committed.value.transitions.contains_key(TRANSITION_A));
    assert_eq!(
        committed.value.transition(TRANSITION_B)?.record.phase,
        Phase::Committed
    );
    assert!(committed.value.histories.contains_key("other"));
    let head = committed
        .value
        .audit_heads
        .get("example")
        .ok_or("audit head missing")?;
    assert_eq!(head.scope.transition_id, TRANSITION_A);
    let archived = audit_store
        .read_audit(head)
        .await?
        .ok_or("archived predecessor missing")?;
    assert_eq!(archived.allocation.transition_id, TRANSITION_A);
    assert_eq!(archived.executor_id, "executor-a");
    assert_eq!(
        archived.events.last().map(|event| event.kind),
        Some(coordination::AuditEventKind::Committed)
    );
    assert!(archived.previous.is_none());
    let encoded = serde_json::to_string(&archived)?;
    for private in [
        PRIVATE_STAGE_KEY,
        private_digest.as_str(),
        "stagedContentDigests",
        "evidenceDigest",
    ] {
        assert!(
            !encoded.contains(private),
            "private archive payload retained"
        );
    }
    {
        let recorded = calls.lock().map_err(|_| "calls lock poisoned")?;
        assert_eq!(recorded.iter().filter(|call| call.0 == "PUT").count(), 1);
    }

    let object = registry_object(&committed.value, REGISTRY_UID, &committed.resource_version)?;
    let (client, calls) = wire(vec![object]);
    let reader =
        platform_client(client, "control", "workload", "cluster-a")?.with_audit_store(audit_store);
    assert_eq!(
        reader
            .load_transition_audit("example", TRANSITION_A)
            .await?,
        Some(archived)
    );
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|call| call.0 == "GET")
    );
    Ok(())
}

struct RejectAuditStore;

#[async_trait::async_trait]
impl AuditStore for RejectAuditStore {
    async fn put_audit(&self, _: &TransitionAuditV1) -> coordination::Result<AuditRef> {
        Err(coordination::Error::Backend("test audit write rejected"))
    }

    async fn read_audit(&self, _: &AuditRef) -> coordination::Result<Option<TransitionAuditV1>> {
        Err(coordination::Error::Backend("test audit read rejected"))
    }
}

#[derive(Clone, Copy)]
enum AuditMode {
    Normal = 0,
    RejectPut = 1,
    LiePut = 2,
    MissingRead = 3,
    CorruptRead = 4,
}

#[derive(Default)]
struct ControlledAuditStore {
    mode: AtomicU8,
    rows: Mutex<BTreeMap<String, TransitionAuditV1>>,
}

impl ControlledAuditStore {
    fn with_mode(mode: AuditMode) -> Self {
        Self {
            mode: AtomicU8::new(mode as u8),
            rows: Mutex::new(BTreeMap::new()),
        }
    }

    fn mode(&self) -> AuditMode {
        match self.mode.load(Ordering::SeqCst) {
            1 => AuditMode::RejectPut,
            2 => AuditMode::LiePut,
            3 => AuditMode::MissingRead,
            4 => AuditMode::CorruptRead,
            _ => AuditMode::Normal,
        }
    }

    fn set_mode(&self, mode: AuditMode) {
        self.mode.store(mode as u8, Ordering::SeqCst);
    }

    fn key(reference: &AuditRef) -> coordination::Result<String> {
        reference.validate()?;
        compiler_core::digest(&reference.scope)
            .map_err(|_| coordination::Error::Invalid("test audit scope"))
    }

    fn only_audit(&self) -> TestResult<TransitionAuditV1> {
        let rows = self.rows.lock().map_err(|_| "audit rows lock poisoned")?;
        if rows.len() != 1 {
            return Err("expected exactly one archived audit".into());
        }
        Ok(rows
            .values()
            .next()
            .ok_or("archived audit unexpectedly absent")?
            .clone())
    }

    fn create_conflict(&self) -> TestResult {
        let mut rows = self.rows.lock().map_err(|_| "audit rows lock poisoned")?;
        let audit = rows
            .values_mut()
            .next()
            .ok_or("archived audit unexpectedly absent")?;
        "conflicting-executor".clone_into(&mut audit.executor_id);
        audit.validate()?;
        Ok(())
    }

    fn row_count(&self) -> TestResult<usize> {
        Ok(self
            .rows
            .lock()
            .map_err(|_| "audit rows lock poisoned")?
            .len())
    }
}

#[async_trait::async_trait]
impl AuditStore for ControlledAuditStore {
    async fn put_audit(&self, audit: &TransitionAuditV1) -> coordination::Result<AuditRef> {
        let reference = audit.reference()?;
        match self.mode() {
            AuditMode::RejectPut => {
                return Err(coordination::Error::Backend("test audit write rejected"));
            }
            AuditMode::LiePut => {
                let mut lied = reference;
                lied.digest = format!("sha256:{}", "0".repeat(64));
                return Ok(lied);
            }
            _ => {}
        }
        let key = Self::key(&reference)?;
        let mut rows = self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("test audit lock"))?;
        if let Some(existing) = rows.get(&key) {
            if existing != audit {
                return Err(coordination::Error::Invalid(
                    "test immutable audit conflict",
                ));
            }
        } else {
            rows.insert(key, audit.clone());
        }
        Ok(reference)
    }

    async fn read_audit(
        &self,
        reference: &AuditRef,
    ) -> coordination::Result<Option<TransitionAuditV1>> {
        if matches!(self.mode(), AuditMode::MissingRead) {
            return Ok(None);
        }
        let audit = self
            .rows
            .lock()
            .map_err(|_| coordination::Error::Backend("test audit lock"))?
            .get(&Self::key(reference)?)
            .cloned();
        if matches!(self.mode(), AuditMode::CorruptRead) {
            return Ok(audit.map(|mut audit| {
                "corrupt-reader".clone_into(&mut audit.executor_id);
                audit
            }));
        }
        Ok(audit)
    }
}

async fn attempt_upgrade_commit(
    registry: &LifecycleRegistry,
    audit_store: Arc<dyn AuditStore>,
    admission: Value,
) -> TestResult<(platform::Result<Versioned<NamespaceRegistry>>, Calls)> {
    let policy = policy(&registry.plan(TRANSITION_B)?.request.next.compiled);
    let (client, calls) = wire(vec![
        object("reakt-policy-example", &policy, "policy-uid", "40"),
        registry_object(registry, REGISTRY_UID, "42")?,
        admission,
    ]);
    let platform = bind_audit_store(registry, client, TRANSITION_B, audit_store)?;
    let previous = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.to_owned(),
        resource_version: "42".to_owned(),
    };
    let result = platform
        .transition_event(
            &previous,
            TRANSITION_B,
            "executor-b",
            committed_event("committed-b")?,
        )
        .await;
    Ok((result, calls))
}

#[tokio::test]
async fn audit_backend_failures_reject_successor_commit_before_registry_put() -> TestResult {
    for mode in [
        AuditMode::RejectPut,
        AuditMode::LiePut,
        AuditMode::MissingRead,
        AuditMode::CorruptRead,
    ] {
        let registry = prepare_cleaning_upgrade()?;
        let store = Arc::new(ControlledAuditStore::with_mode(mode));
        let (result, calls) = attempt_upgrade_commit(&registry, store, json!("echo-put")).await?;
        assert!(result.is_err());
        assert!(
            calls
                .lock()
                .map_err(|_| "calls lock poisoned")?
                .iter()
                .all(|call| call.0 != "PUT")
        );
    }
    Ok(())
}

#[tokio::test]
async fn orphaned_truthful_archive_is_reused_after_rejected_admission_response() -> TestResult {
    let registry = prepare_cleaning_upgrade()?;
    let store = Arc::new(ControlledAuditStore::default());
    let rejected_admission = registry_object(&registry, REGISTRY_UID, "43")?;
    let (failed, calls) =
        attempt_upgrade_commit(&registry, store.clone(), rejected_admission).await?;
    assert!(failed.is_err());
    let orphan = store.only_audit()?;
    assert_eq!(orphan.scope.transition_id, TRANSITION_A);
    assert_eq!(orphan.executor_id, "executor-a");
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .filter(|call| call.0 == "PUT")
            .count(),
        1
    );

    let (retried, calls) = attempt_upgrade_commit(&registry, store, json!("echo-put")).await?;
    let committed = retried?;
    assert!(!committed.value.transitions.contains_key(TRANSITION_A));
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .filter(|call| call.0 == "PUT")
            .count(),
        1
    );
    Ok(())
}

fn conflict_wire(responses: Vec<Value>) -> (Client, Calls) {
    let replies = Arc::new(Mutex::new(VecDeque::from(responses)));
    let calls: Calls = Arc::default();
    let service = service_fn({
        let calls = calls.clone();
        move |request: Request<Body>| {
            let replies = replies.clone();
            let calls = calls.clone();
            async move {
                let (parts, body) = request.into_parts();
                let data = body
                    .collect()
                    .await
                    .map_err(std::io::Error::other)?
                    .to_bytes();
                let data: Value = if data.is_empty() {
                    Value::Null
                } else {
                    serde_json::from_slice(&data).map_err(std::io::Error::other)?
                };
                calls
                    .lock()
                    .map_err(|_| std::io::Error::other("calls lock poisoned"))?
                    .push((parts.method.to_string(), parts.uri.to_string(), data));
                if parts.method == http::Method::PUT {
                    return Response::builder()
                        .status(http::StatusCode::CONFLICT)
                        .header("content-type", "application/json")
                        .body(Body::from(
                            serde_json::to_vec(&json!({
                                "apiVersion": "v1",
                                "kind": "Status",
                                "status": "Failure",
                                "message": "registry resourceVersion conflict",
                                "reason": "Conflict",
                                "code": 409
                            }))
                            .map_err(std::io::Error::other)?,
                        ))
                        .map_err(std::io::Error::other);
                }
                let reply = if parts
                    .uri
                    .path()
                    .ends_with("/configmaps/reakt-registry-authority.control.workload")
                {
                    registry_authority()
                } else {
                    replies
                        .lock()
                        .map_err(|_| std::io::Error::other("replies lock poisoned"))?
                        .pop_front()
                        .ok_or_else(|| std::io::Error::other("unexpected Kubernetes request"))?
                };
                Response::builder()
                    .header("content-type", "application/json")
                    .body(Body::from(
                        serde_json::to_vec(&reply).map_err(std::io::Error::other)?,
                    ))
                    .map_err(std::io::Error::other)
            }
        }
    });
    (Client::new(service, "control"), calls)
}

#[tokio::test]
async fn actual_registry_conflict_leaves_truthful_archive_for_retry() -> TestResult {
    let registry = prepare_cleaning_upgrade()?;
    let store = Arc::new(ControlledAuditStore::default());
    let policy = policy(&registry.plan(TRANSITION_B)?.request.next.compiled);
    let (client, calls) = conflict_wire(vec![
        object("reakt-policy-example", &policy, "policy-uid", "40"),
        registry_object(&registry, REGISTRY_UID, "42")?,
    ]);
    let platform = bind_audit_store(&registry, client, TRANSITION_B, store.clone())?;
    let previous = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.to_owned(),
        resource_version: "42".to_owned(),
    };
    assert!(
        platform
            .transition_event(
                &previous,
                TRANSITION_B,
                "executor-b",
                committed_event("committed-b")?,
            )
            .await
            .is_err()
    );
    let orphan = store.only_audit()?;
    assert_eq!(orphan.scope.transition_id, TRANSITION_A);
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .filter(|call| call.0 == "PUT")
            .count(),
        1
    );

    let (retried, calls) = attempt_upgrade_commit(&registry, store, json!("echo-put")).await?;
    assert!(!retried?.value.transitions.contains_key(TRANSITION_A));
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .filter(|call| call.0 == "PUT")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn conflicting_orphaned_archive_blocks_retry_without_registry_put() -> TestResult {
    let registry = prepare_cleaning_upgrade()?;
    let store = Arc::new(ControlledAuditStore::default());
    let rejected_admission = registry_object(&registry, REGISTRY_UID, "43")?;
    let (failed, _) = attempt_upgrade_commit(&registry, store.clone(), rejected_admission).await?;
    assert!(failed.is_err());
    store.create_conflict()?;

    let (retried, calls) = attempt_upgrade_commit(&registry, store, json!("echo-put")).await?;
    assert!(retried.is_err());
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|call| call.0 != "PUT")
    );
    Ok(())
}

async fn committed_upgrade_fixture(
    store: Arc<ControlledAuditStore>,
) -> TestResult<(LifecycleRegistry, Versioned<NamespaceRegistry>)> {
    let registry = prepare_cleaning_upgrade()?;
    let (result, _) = attempt_upgrade_commit(&registry, store, json!("echo-put")).await?;
    let committed = result?;
    Ok((
        LifecycleRegistry {
            registry: committed.value.clone(),
            plans: registry.plans,
        },
        committed,
    ))
}

fn prepare_pending_third_transition(
    mut registry: LifecycleRegistry,
) -> TestResult<(LifecycleRegistry, String)> {
    let predecessor_plan = registry.plan(TRANSITION_B)?.clone();
    let names = deploy::allocation_names(REGISTRY_UID, 3)?;
    let mut history = registry
        .histories
        .get("example")
        .ok_or("domain history missing")?
        .clone();
    "43".clone_into(&mut history.revision);
    let mut request = predecessor_plan.request.clone();
    request.transition_id.clone_from(&names.transition_id);
    request.previous = Some(deploy::PreviousGeneration::Metadata(Box::new(
        deploy::predecessor_metadata(
            &predecessor_plan.request.next,
            &predecessor_plan.generation_reservation,
            &registry.claims,
        )?,
    )));
    request.next.generation_id.clone_from(&names.generation_id);
    request.registry = deploy::RegistrySnapshot {
        registry_uid: REGISTRY_UID.to_owned(),
        next_sequence: 3,
        revision: "43".to_owned(),
        claims: registry.claims.clone(),
        generation_history: history,
    };
    let plan = deploy::plan_transition(&request)?;
    let record = deploy::begin(&plan, "executor-c")?;
    let inputs = transition_inputs(&plan, &policy(&plan.request.next.compiled))?;
    let mut stored = StoredTransition::reserved(inputs, record);
    stored.previous_writer_checkpoint = Some(
        registry
            .active_writer_checkpoints
            .get("example")
            .ok_or("active writer checkpoint missing")?
            .clone(),
    );
    registry
        .transitions
        .insert(names.transition_id.clone(), stored);
    registry.plans.insert(names.transition_id.clone(), plan);
    registry.next_sequence = 4;
    Ok((registry, names.transition_id))
}

fn commit_third_record(registry: &mut LifecycleRegistry, transition: &str) -> TestResult {
    let plan = registry.plan(transition)?.clone();
    let stored = registry
        .transitions
        .get_mut(transition)
        .ok_or("third transition missing")?;
    for event in [
        Event::Staged {
            event_id: "staged-c".to_owned(),
            generation_id: plan.request.next.generation_id.clone(),
            service_account_name: plan.generation_reservation.service_account_name.clone(),
            registry_cas_receipt_digest: digest(&"third reservation")?,
            verification_epoch: 0,
        },
        Event::Quiesced {
            event_id: "quiesced-c".to_owned(),
            evidence: Quiescence::OldGeneration(TerminationEvidence {
                generation_id: GENERATION_B.to_owned(),
                executor_id: "executor-c".to_owned(),
                verification_epoch: 0,
                environment_qualification_digest: digest(&"third qualified environment")?,
                process_termination_receipt_digest: digest(&"third terminated process")?,
                access_revocation_receipt_digest: digest(&"third revoked access")?,
            }),
        },
        Event::Activated {
            event_id: "activated-c".to_owned(),
            generation_id: plan.request.next.generation_id.clone(),
            activation_receipt_digest: digest(&"activated third")?,
            verification_epoch: 0,
        },
        Event::Cleaning {
            event_id: "cleaning-c".to_owned(),
        },
        committed_event("committed-c")?,
    ] {
        stored.record = deploy::advance(
            &plan,
            &stored.record,
            stored.record.revision,
            "executor-c",
            event,
        )?;
    }
    Ok(())
}

#[tokio::test]
async fn successor_archive_rejects_missing_or_corrupt_prior_head_before_put_or_prune() -> TestResult
{
    for mode in [AuditMode::MissingRead, AuditMode::CorruptRead] {
        let good_store = Arc::new(ControlledAuditStore::default());
        let (committed_b, committed) = committed_upgrade_fixture(good_store.clone()).await?;
        let store = if matches!(mode, AuditMode::CorruptRead) {
            good_store.set_mode(mode);
            good_store
        } else {
            Arc::new(ControlledAuditStore::with_mode(mode))
        };
        let rows_before = store.row_count()?;
        let (mut registry, transition_c) = prepare_pending_third_transition(committed_b)?;
        let previous = Versioned {
            value: registry.registry.clone(),
            uid: REGISTRY_UID.to_owned(),
            resource_version: committed.resource_version,
        };
        commit_third_record(&mut registry, &transition_c)?;
        let (client, calls) = wire(Vec::new());
        let platform = bind_audit_store(&registry, client, &transition_c, store.clone())?;

        let error = match platform
            .archive_predecessor(&previous, &mut registry.registry, &transition_c)
            .await
        {
            Ok(()) => return Err("prior audit head unexpectedly accepted".into()),
            Err(error) => error,
        };
        let expected = match mode {
            AuditMode::MissingRead => "protected transition audit missing",
            AuditMode::CorruptRead => "transition audit differs from its protected reference",
            _ => return Err("prior-head fixture uses an unsupported audit mode".into()),
        };
        assert!(error.to_string().contains(expected));
        assert_eq!(store.row_count()?, rows_before);
        assert!(registry.transitions.contains_key(TRANSITION_B));
        assert_eq!(
            registry
                .audit_heads
                .get("example")
                .map(|head| head.scope.transition_id.as_str()),
            Some(TRANSITION_A)
        );
        assert!(
            calls
                .lock()
                .map_err(|_| "calls lock poisoned")?
                .iter()
                .all(|call| call.0 != "PUT")
        );
    }
    Ok(())
}

#[tokio::test]
async fn protected_audit_head_rejects_missing_or_corrupt_archive_rows() -> TestResult {
    let good_store = Arc::new(ControlledAuditStore::default());
    let (_, committed) = committed_upgrade_fixture(good_store.clone()).await?;
    let control = registry_object(&committed.value, REGISTRY_UID, &committed.resource_version)?;

    let missing_store = Arc::new(ControlledAuditStore::with_mode(AuditMode::MissingRead));
    let (client, calls) = wire(vec![control.clone()]);
    let reader = platform_client(client, "control", "workload", "cluster-a")?
        .with_audit_store(missing_store);
    assert!(
        reader
            .load_transition_audit("example", TRANSITION_A)
            .await
            .is_err()
    );
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|call| call.0 == "GET")
    );

    good_store.set_mode(AuditMode::CorruptRead);
    let (client, calls) = wire(vec![control]);
    let reader =
        platform_client(client, "control", "workload", "cluster-a")?.with_audit_store(good_store);
    assert!(
        reader
            .load_transition_audit("example", TRANSITION_A)
            .await
            .is_err()
    );
    assert!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .all(|call| call.0 == "GET")
    );
    Ok(())
}

fn committed_without_archive(mut registry: LifecycleRegistry) -> TestResult<LifecycleRegistry> {
    let plan = registry.plan(TRANSITION_B)?.clone();
    let stored = registry
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("upgrade transition missing")?;
    stored.record = deploy::advance(
        &plan,
        &stored.record,
        stored.record.revision,
        "executor-b",
        committed_event("unarchived-commit")?,
    )?;
    Ok(registry)
}

#[tokio::test]
async fn malformed_v5_retention_states_fail_on_authenticated_load_without_writes() -> TestResult {
    let multiple_committed = committed_without_archive(prepare_cleaning_upgrade()?)?;

    let mut missing_active = qualified_upgrade_fixture()?;
    missing_active
        .histories
        .get_mut("example")
        .ok_or("example history missing")?
        .active_allocation = None;

    let mut invalid_pending = qualified_upgrade_fixture()?;
    invalid_pending
        .transitions
        .get_mut(TRANSITION_B)
        .ok_or("pending transition missing")?
        .previous_writer_checkpoint = None;

    for registry in [multiple_committed, missing_active, invalid_pending] {
        let (client, calls) = wire(vec![registry_object(&registry, REGISTRY_UID, "42")?]);
        let platform = platform_client(client, "control", "workload", "cluster-a")?;
        assert!(platform.load_registry().await.is_err());
        assert!(
            calls
                .lock()
                .map_err(|_| "calls lock poisoned")?
                .iter()
                .all(|call| call.0 == "GET")
        );
    }
    Ok(())
}

#[tokio::test]
async fn initial_commit_retains_current_record_and_never_writes_an_archive() -> TestResult {
    let registry = prepare_cleaning_initial()?;
    let previous = Versioned {
        value: registry.registry.clone(),
        uid: REGISTRY_UID.to_owned(),
        resource_version: "42".to_owned(),
    };
    let policy = policy(&registry.plan(TRANSITION_A)?.request.next.compiled);
    let (client, calls) = wire(vec![
        object("reakt-policy-example", &policy, "policy-uid", "40"),
        registry_object(&registry, REGISTRY_UID, "42")?,
        json!("echo-put"),
    ]);
    let platform = bind_audit_store(&registry, client, TRANSITION_A, Arc::new(RejectAuditStore))?;
    let committed = platform
        .transition_event(
            &previous,
            TRANSITION_A,
            "executor-a",
            committed_event("committed-initial")?,
        )
        .await?;
    assert_eq!(
        committed.value.transition(TRANSITION_A)?.record.phase,
        Phase::Committed
    );
    assert!(committed.value.audit_heads.is_empty());
    assert_eq!(
        calls
            .lock()
            .map_err(|_| "calls lock poisoned")?
            .iter()
            .filter(|call| call.0 == "PUT")
            .count(),
        1
    );
    Ok(())
}
